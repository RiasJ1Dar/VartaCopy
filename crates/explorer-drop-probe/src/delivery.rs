use super::{choice, config::Config};
use std::{
    io::{self, Write},
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
};
use vartacopy_shell_windows::transfer::{TransferRequest, encode_transfer};
use windows::{
    Win32::{
        Foundation::HMODULE,
        System::LibraryLoader::{
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN,
            GetModuleHandleExW,
        },
    },
    core::PCWSTR,
};
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
struct Slot;
impl Drop for Slot {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}
fn pin() -> io::Result<()> {
    let mut module = HMODULE::default();
    // SAFETY: FROM_ADDRESS interprets the pointer as an address in this DLL,
    // never as text. Pinning keeps worker code alive until Explorer exits.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
            PCWSTR(pin as *const () as *const u16),
            &mut module,
        )
    }
    .map_err(io::Error::other)
}
fn launch(config: &Config, bytes: &[u8]) -> io::Result<()> {
    let mut command = Command::new(&config.gui);
    command.arg("--enqueue-transfer-stdin");
    if let Some(journal) = &config.journal {
        command.arg("--journal").arg(journal);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("GUI input pipe unavailable"))?;
    input.write_all(bytes)?;
    drop(input); // EOF commits the bounded frame; truncated frames never execute.
    Ok(())
}
pub fn send(config: Config, request: TransferRequest) -> io::Result<()> {
    let bytes = encode_transfer(&request)?;
    ACTIVE
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < 4).then_some(n + 1)
        })
        .map_err(|_| io::Error::other("Канал передачі зайнятий; повторіть запит пізніше"))?;
    let slot = Slot;
    pin()?;
    std::thread::Builder::new().name("vartacopy-shell-delivery".into()).spawn(move||{
        let _slot=slot;
        if let Err(error)=launch(&config,&bytes){
            eprintln!("VartaCopy shell delivery failed: {error}");
            choice::error(&format!("Не вдалося передати запит VartaCopy: {error}\n\nАвтоматичного повтору немає. Перевірте стан операції у VartaCopy перед повторним запуском."));
        }
    })?;
    Ok(())
}
