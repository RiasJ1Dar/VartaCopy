//! Explicit Windows engine. This is an attempt report, never a verified-copy proof.
use crate::transfer::{TransferOperation, TransferRequest, encode_transfer};
use serde::{Deserialize, Serialize};
use std::{
    io,
    os::windows::ffi::OsStrExt,
    path::Path,
    sync::{Arc, Mutex},
};
use windows::{
    Win32::{
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
        UI::Shell::{
            FOF_NOCONFIRMATION, FOF_NOCONFIRMMKDIR, FOF_NOERRORUI, FOF_SILENT, FOFX_NOCOPYHOOKS,
            FileOperation, IFileOperation, IFileOperationProgressSink,
            IFileOperationProgressSink_Impl, IShellItem, SHCreateItemFromParsingName,
        },
    },
    core::PCWSTR,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeResult {
    pub perform_hresult: i32,
    pub aborted: Option<bool>,
    pub aborted_hresult: i32,
    pub progress: NativeProgress,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NativeProgress {
    pub finished_hresult: Option<i32>,
    pub item_callbacks: u64,
    pub non_success_items: u64,
    /// At most sixteen status codes. Callbacks include nested items; not a root count.
    pub first_non_success_codes: Vec<i32>,
}
impl NativeResult {
    pub fn reported_complete(&self) -> bool {
        self.perform_hresult == 0
            && self.aborted == Some(false)
            && self.aborted_hresult == 0
            && self.progress.finished_hresult == Some(0)
            && self.progress.item_callbacks > 0
            && self.progress.non_success_items == 0
    }
}

#[windows::core::implement(IFileOperationProgressSink)]
struct Sink(Arc<Mutex<NativeProgress>>);
impl Sink_Impl {
    fn post(&self, hr: windows::core::HRESULT) -> windows::core::Result<()> {
        let mut p = self.0.lock().map_err(|_| {
            windows::core::Error::from_hresult(windows::core::HRESULT(0x80004005u32 as i32))
        })?;
        p.item_callbacks = p.item_callbacks.saturating_add(1);
        // Optimized same-volume moves can finish the item without walking
        // children. This documented control status is not a user skip.
        if hr.0 != 0 && hr != windows::Win32::UI::Shell::COPYENGINE_S_DONT_PROCESS_CHILDREN {
            p.non_success_items = p.non_success_items.saturating_add(1);
            if p.first_non_success_codes.len() < 16 {
                p.first_non_success_codes.push(hr.0);
            }
        }
        Ok(())
    }
}
#[allow(non_snake_case)]
impl IFileOperationProgressSink_Impl for Sink_Impl {
    fn StartOperations(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn FinishOperations(&self, hr: windows::core::HRESULT) -> windows::core::Result<()> {
        self.0
            .lock()
            .map_err(|_| {
                windows::core::Error::from_hresult(windows::core::HRESULT(0x80004005u32 as i32))
            })?
            .finished_hresult = Some(hr.0);
        Ok(())
    }
    fn PreRenameItem(
        &self,
        _: u32,
        _: windows::core::Ref<IShellItem>,
        _: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostRenameItem(
        &self,
        _: u32,
        _: windows::core::Ref<IShellItem>,
        _: &PCWSTR,
        _: windows::core::HRESULT,
        _: windows::core::Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreMoveItem(
        &self,
        _: u32,
        _: windows::core::Ref<IShellItem>,
        _: windows::core::Ref<IShellItem>,
        _: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostMoveItem(
        &self,
        _: u32,
        _: windows::core::Ref<IShellItem>,
        _: windows::core::Ref<IShellItem>,
        _: &PCWSTR,
        hr: windows::core::HRESULT,
        _: windows::core::Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        self.post(hr)
    }
    fn PreCopyItem(
        &self,
        _: u32,
        _: windows::core::Ref<IShellItem>,
        _: windows::core::Ref<IShellItem>,
        _: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostCopyItem(
        &self,
        _: u32,
        _: windows::core::Ref<IShellItem>,
        _: windows::core::Ref<IShellItem>,
        _: &PCWSTR,
        hr: windows::core::HRESULT,
        _: windows::core::Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        self.post(hr)
    }
    fn PreDeleteItem(
        &self,
        _: u32,
        _: windows::core::Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostDeleteItem(
        &self,
        _: u32,
        _: windows::core::Ref<IShellItem>,
        _: windows::core::HRESULT,
        _: windows::core::Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreNewItem(
        &self,
        _: u32,
        _: windows::core::Ref<IShellItem>,
        _: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostNewItem(
        &self,
        _: u32,
        _: windows::core::Ref<IShellItem>,
        _: &PCWSTR,
        _: &PCWSTR,
        _: u32,
        _: windows::core::HRESULT,
        _: windows::core::Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn UpdateProgress(&self, _: u32, _: u32) -> windows::core::Result<()> {
        Ok(())
    }
    fn ResetTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn PauseTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn ResumeTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
}

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: Balanced with this thread's successful CoInitializeEx, after COM objects drop.
        unsafe { CoUninitialize() };
    }
}
fn shell_item(path: &Path) -> windows::core::Result<IShellItem> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: NUL-terminated native UTF-16 stays live for this synchronous call.
    unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None) }
}
fn as_io(error: windows::core::Error) -> io::Error {
    io::Error::other(format!("Windows file operation: {error}"))
}

/// Call only after an explicit engine choice and a committed native intent.
/// Runs synchronously on the caller's STA worker, never on the GUI thread.
/// Windows owns collision prompts, progress and cancellation. No shortcut replay,
/// source cleanup, clipboard acknowledgement or other-engine fallback is performed.
pub fn execute(request: &TransferRequest) -> io::Result<NativeResult> {
    execute_inner(request, false)
}
fn execute_inner(request: &TransferRequest, quiet_fixture: bool) -> io::Result<NativeResult> {
    encode_transfer(request)?;
    // SAFETY: Initialize only this worker thread; failed initialization is not balanced.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
        .ok()
        .map_err(as_io)?;
    let _apartment = Apartment;
    // SAFETY: Documented in-process Windows COM class, accessed only in this STA.
    let operation: IFileOperation =
        unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER) }.map_err(as_io)?;
    let mut flags = FOF_NOCONFIRMMKDIR | FOFX_NOCOPYHOOKS;
    if quiet_fixture {
        // Private test path only; production always retains Windows prompts.
        flags |= FOF_SILENT | FOF_NOERRORUI | FOF_NOCONFIRMATION;
    }
    // SAFETY: Valid STA interface, explicit flags; this only configures future operations.
    unsafe { operation.SetOperationFlags(flags) }.map_err(as_io)?;
    let progress = Arc::new(Mutex::new(NativeProgress::default()));
    let sink: IFileOperationProgressSink = Sink(progress.clone()).into();
    // SAFETY: Sink is owned until Unadvise and all callbacks run inside the STA operation.
    let cookie = unsafe { operation.Advise(&sink) }.map_err(as_io)?;
    let destination = shell_item(&request.destination).map_err(as_io)?;
    // Resolve every source before declaring or executing any mutation.
    let sources = request
        .sources
        .iter()
        .map(|p| shell_item(p))
        .collect::<windows::core::Result<Vec<_>>>()
        .map_err(as_io)?;
    for source in &sources {
        // SAFETY: STA-owned shell items remain live. Null new name preserves the source name.
        unsafe {
            match request.operation {
                TransferOperation::Copy => {
                    operation.CopyItem(source, &destination, PCWSTR::null(), None)
                }
                TransferOperation::Move => {
                    operation.MoveItem(source, &destination, PCWSTR::null(), None)
                }
            }
        }
        .map_err(as_io)?;
    }
    // SAFETY: Executes only this explicitly constructed batch in its STA.
    let performed = unsafe { operation.PerformOperations() };
    // SAFETY: Required even when PerformOperations failed; success alone does not exclude cancellation.
    let aborted = unsafe { operation.GetAnyOperationsAborted() };
    // SAFETY: This cookie belongs to the same live operation and sink.
    unsafe { operation.Unadvise(cookie) }.map_err(as_io)?;
    Ok(NativeResult {
        perform_hresult: performed.err().map_or(0, |e| e.code().0),
        aborted: aborted.as_ref().ok().map(|b| b.as_bool()),
        aborted_hresult: aborted.err().map_or(0, |e| e.code().0),
        progress: progress
            .lock()
            .map_err(|_| io::Error::other("native progress state poisoned"))?
            .clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::TransferOrigin;
    #[test]
    fn windows_engine_copies_and_moves_own_unicode_fixtures_without_clipboard() {
        // No visible UI, collisions or user data. Each call uses a new STA thread.
        for operation in [TransferOperation::Copy, TransferOperation::Move] {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("власні дані.txt");
            let destination = root.path().join("призначення");
            std::fs::write(&source, b"native fixture").unwrap();
            std::fs::create_dir(&destination).unwrap();
            let request = TransferRequest {
                id: [1; 16],
                operation,
                origin: TransferOrigin::AppPaste,
                sources: vec![source.clone()],
                destination: destination.clone(),
            };
            let result = std::thread::spawn(move || execute_inner(&request, true))
                .join()
                .unwrap()
                .unwrap();
            assert!(result.reported_complete(), "{result:?}");
            assert_eq!(
                std::fs::read(destination.join("власні дані.txt")).unwrap(),
                b"native fixture"
            );
            assert_eq!(source.exists(), operation == TransferOperation::Copy);
        }
    }
    #[test]
    fn incomplete_or_unknown_native_results_are_not_reported_complete() {
        for (perform, aborted, query) in [
            (0, Some(true), 0),
            (-1, Some(false), 0),
            (0, None, -1),
            (1, Some(false), 0),
        ] {
            assert!(
                !NativeResult {
                    perform_hresult: perform,
                    aborted,
                    aborted_hresult: query,
                    progress: NativeProgress::default()
                }
                .reported_complete()
            );
        }
    }
}
