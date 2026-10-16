use std::{
    ffi::OsStr,
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    process::Command,
};
use windows::{
    Win32::{
        Foundation::{E_FAIL, E_INVALIDARG, HMODULE},
        System::LibraryLoader::{
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            GetModuleFileNameW, GetModuleHandleExW,
        },
    },
    core::{HRESULT, PCWSTR},
};

pub(crate) fn module_path() -> Result<PathBuf, HRESULT> {
    let mut module = HMODULE::default();
    // SAFETY: FROM_ADDRESS treats the exported function address as an address,
    // not a string. The active COM call keeps this DLL loaded throughout this use.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(crate::DllGetClassObject as *const () as *const u16),
            &mut module,
        )
    }
    .map_err(|e| e.code())?;
    let mut buffer = vec![0; 512];
    loop {
        // SAFETY: module is live, buffer is a writable UTF-16 slice.
        let len = unsafe { GetModuleFileNameW(Some(module), &mut buffer) } as usize;
        if len == 0 {
            return Err(windows::core::Error::from_thread().code());
        }
        if len < buffer.len() {
            return Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer[..len])));
        }
        if buffer.len() >= 32768 {
            return Err(E_FAIL);
        }
        buffer.resize((buffer.len() * 2).min(32768), 0);
    }
}

pub(crate) fn gui_from_module(module: &Path) -> Result<PathBuf, HRESULT> {
    if !module.is_absolute() {
        return Err(E_INVALIDARG);
    }
    let parent = module.parent().ok_or(E_INVALIDARG)?;
    Ok(parent.join(crate::GUI_FILENAME))
}

pub(crate) fn source_command(gui: &Path, source: &OsStr) -> Result<Command, HRESULT> {
    if !gui.is_absolute() {
        return Err(E_INVALIDARG);
    }
    let mut command = Command::new(gui);
    command.arg("--source").arg(source);
    command.current_dir(gui.parent().ok_or(E_INVALIDARG)?);
    Ok(command)
}

pub(crate) fn launch(gui: &Path, source: &OsStr) -> Result<(), HRESULT> {
    let child = source_command(gui, source)?.spawn().map_err(|error| {
        error
            .raw_os_error()
            .map(|code| HRESULT::from_win32(code as u32))
            .unwrap_or(E_FAIL)
    })?;
    // Dropping Child closes the process handles. No wait, shell, or copy work.
    drop(child);
    Ok(())
}
