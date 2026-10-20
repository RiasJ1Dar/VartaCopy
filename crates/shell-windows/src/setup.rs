//! Setup lifecycle primitives. UAC is always confirmed by the person using Windows.
use crate::{
    transfer_registration::{self, Installation, Status},
    wide,
};
use std::{ffi::OsStr, io, marker::PhantomData, os::windows::ffi::OsStrExt, rc::Rc};
use windows_sys::Win32::{
    Foundation::*,
    System::Threading::*,
    UI::{Shell::*, WindowsAndMessaging::SW_HIDE},
};
pub const PRESENCE_MUTEX: &str = r"Local\VartaCopy.Running";
pub(crate) struct KernelMutex {
    handle: HANDLE,
    acquired: bool,
    _thread: PhantomData<Rc<()>>,
}
impl KernelMutex {
    fn open(name: &str, timeout: Option<u32>) -> io::Result<Self> {
        // SAFETY: Default DACL and live terminated name. The returned handle is uniquely owned.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide(name).as_ptr()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut result = Self {
            handle,
            acquired: false,
            _thread: PhantomData,
        };
        if let Some(timeout) = timeout {
            // SAFETY: Wait only for this owned mutex. Abandoned means acquired after a crash.
            match unsafe { WaitForSingleObject(handle, timeout) } {
                WAIT_OBJECT_0 | WAIT_ABANDONED => result.acquired = true,
                WAIT_TIMEOUT => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "Інше налаштування VartaCopy ще виконується. Спробуйте пізніше.",
                    ));
                }
                _ => return Err(io::Error::last_os_error()),
            }
        }
        Ok(result)
    }
}
impl Drop for KernelMutex {
    fn drop(&mut self) {
        // SAFETY: Release only if acquired on this same thread; then close our sole handle.
        unsafe {
            if self.acquired && ReleaseMutex(self.handle) == 0 {
                eprintln!(
                    "VartaCopy configuration mutex release failed: {}",
                    io::Error::last_os_error()
                );
            }
            if CloseHandle(self.handle) == 0 {
                eprintln!(
                    "VartaCopy mutex close failed: {}",
                    io::Error::last_os_error()
                );
            }
        }
    }
}
pub(crate) fn configuration_lock() -> io::Result<KernelMutex> {
    KernelMutex::open(r"Global\VartaCopy.ExplorerTransfer.Setup.v1", Some(15000))
}
/// Presence only: every live GUI keeps a reference, without acquiring exclusivity.
pub struct Presence {
    _mutex: KernelMutex,
}
impl Presence {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            _mutex: KernelMutex::open(PRESENCE_MUTEX, None)?,
        })
    }
}
#[derive(Clone, Copy, Debug)]
pub enum Action {
    Enable,
    Disable,
}
fn quote(value: &OsStr, output: &mut Vec<u16>) {
    output.push(b'"' as u16);
    let mut slashes = 0;
    for unit in value.encode_wide() {
        if unit == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        if unit == b'"' as u16 {
            output.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2 + 1));
        } else {
            output.extend(std::iter::repeat_n(b'\\' as u16, slashes));
        }
        output.push(unit);
        slashes = 0;
    }
    output.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    output.push(b'"' as u16);
}
fn parameters(installation: &Installation, action: Action) -> Vec<u16> {
    let mut result = format!(
        "integration {} --gui ",
        match action {
            Action::Enable => "install",
            Action::Disable => "remove",
        }
    )
    .encode_utf16()
    .collect::<Vec<_>>();
    quote(installation.gui.as_os_str(), &mut result);
    result.extend(" --dll ".encode_utf16());
    quote(installation.dll.as_os_str(), &mut result);
    result.extend(" --json".encode_utf16());
    result.push(0);
    result
}
struct Process(HANDLE);
impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: A valid process handle returned by ShellExecuteEx, owned by this wrapper.
        if unsafe { CloseHandle(self.0) } == 0 {
            eprintln!(
                "VartaCopy setup process close failed: {}",
                io::Error::last_os_error()
            );
        }
    }
}
pub fn request(installation: &Installation, action: Action) -> io::Result<Status> {
    transfer_registration::preflight(installation)?;
    let cli = installation.gui.with_file_name("vartacopy.exe");
    if !cli.is_file() {
        return Err(io::Error::other(
            "Засіб встановлення відсутній. Розпакуйте повний пакет VartaCopy.",
        ));
    }
    struct Com;
    impl Drop for Com {
        fn drop(&mut self) {
            // SAFETY: Balances successful initialization on this same worker thread.
            unsafe {
                windows::Win32::System::Com::CoUninitialize();
            }
        }
    }
    // SAFETY: Called on a dedicated setup worker; no GUI or caller COM object is moved here.
    unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        )
    }
    .ok()
    .map_err(io::Error::other)?;
    let _com = Com;
    let file = wide(&cli);
    let params = parameters(installation, action);
    let verb = wide("runas");
    // SAFETY: Windows C structure permits zero initialization of optional fields.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_HIDE;
    // SAFETY: All native strings outlive the call. UAC is presented by Windows, never auto-confirmed.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.hProcess.is_null() {
        return Err(io::Error::other(
            "Windows не повернула підтвердження запуску налаштування.",
        ));
    }
    let process = Process(info.hProcess);
    // SAFETY: Wait for only this process, off the UI thread. Never force terminate setup or Explorer.
    match unsafe { WaitForSingleObject(process.0, 120000) } {
        WAIT_OBJECT_0 => {}
        WAIT_TIMEOUT => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Налаштування ще не підтверджене. Перевірте стан перед повторною спробою.",
            ));
        }
        _ => return Err(io::Error::last_os_error()),
    }
    let mut code = 0;
    // SAFETY: Live process handle and writable scalar output.
    if unsafe { GetExitCodeProcess(process.0, &mut code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let status = transfer_registration::status(installation)?;
    if code != 0 {
        return Err(io::Error::other(format!(
            "Налаштування не завершилося (код {code}). Поточний стан: {status:?}"
        )));
    }
    if !matches!(
        (action, &status),
        (Action::Enable, Status::Enabled { .. }) | (Action::Disable, Status::Disabled)
    ) {
        return Err(io::Error::other(
            "Windows не підтвердила очікувану зміну інтеграції. Оновіть стан.",
        ));
    }
    Ok(status)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn argument_quoting_preserves_utf16_quotes_spaces_and_trailing_slashes() {
        use std::{ffi::OsString, os::windows::ffi::OsStringExt};
        let source = OsString::from_wide(&[b'D' as u16, 58, 92, 0xd800, 32, 34, 92]);
        let mut result = Vec::new();
        quote(&source, &mut result);
        assert_eq!(result, vec![34, 68, 58, 92, 0xd800, 32, 92, 34, 92, 92, 34]);
    }
    #[test]
    fn configuration_mutex_serializes_threads_and_presence_is_not_exclusive() {
        let name = format!(r"Local\VartaCopy.Setup.Tests.{}", std::process::id());
        let first = KernelMutex::open(&name, Some(0)).unwrap();
        let next_name = name.clone();
        let blocked = std::thread::spawn(move || {
            KernelMutex::open(&next_name, Some(1)).err().unwrap().kind()
        })
        .join()
        .unwrap();
        assert_eq!(blocked, io::ErrorKind::WouldBlock);
        drop(first);
        let _next = KernelMutex::open(&name, Some(0)).unwrap();
        let presence_name = format!("{name}.presence");
        let _a = KernelMutex::open(&presence_name, None).unwrap();
        let _b = KernelMutex::open(&presence_name, None).unwrap();
    }
}
