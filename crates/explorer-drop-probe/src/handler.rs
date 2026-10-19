//! File-system drop menu integration. No filesystem operation starts in QueryContextMenu.
use super::{
    Lifetime,
    choice::{self, Choice},
    config::{self, Config},
    delivery, ffi, selection,
};
use std::sync::Mutex;
use vartacopy_shell_windows::transfer::{TransferOperation, TransferRequest};
use windows::{
    Win32::{
        Foundation::{E_FAIL, E_INVALIDARG, E_NOTIMPL},
        System::{Com::IDataObject, Registry::HKEY},
        UI::{
            Shell::{
                CMINVOKECOMMANDINFO, Common::ITEMIDLIST, IContextMenu, IContextMenu_Impl,
                IShellExtInit, IShellExtInit_Impl,
            },
            WindowsAndMessaging::{
                GetMenuDefaultItem, HMENU, InsertMenuW, MF_BYCOMMAND, MF_BYPOSITION, MF_STRING,
                RemoveMenu, SetMenuDefaultItem,
            },
        },
    },
    core::{HRESULT, PSTR, Ref, Result, implement},
};

#[derive(Default)]
struct State {
    pending: Option<(std::path::PathBuf, IDataObject)>,
    request: Option<TransferRequest>,
    config: Option<Config>,
    prepared: bool,
    querying: bool,
    consumed: bool,
}
#[implement(IShellExtInit, IContextMenu)]
pub struct TransferHandler {
    _lifetime: Lifetime,
    state: Mutex<State>,
}
impl TransferHandler {
    pub fn new() -> Self {
        Self {
            _lifetime: Lifetime::new(),
            state: Mutex::new(State::default()),
        }
    }
}
impl IShellExtInit_Impl for TransferHandler_Impl {
    fn Initialize(&self, folder: *const ITEMIDLIST, data: Ref<IDataObject>, _: HKEY) -> Result<()> {
        ffi(|| {
            crate::trace(
                "transfer_initialize",
                serde_json::json!({"folder":!folder.is_null(),"data":data.is_some()}),
            );
            // Release the previous foreign interface after unlocking, in case Release reenters COM.
            let previous = std::mem::take(
                &mut *self
                    .state
                    .lock()
                    .map_err(|_| windows::core::Error::from(E_FAIL))?,
            );
            drop(previous);
            let data = data.as_ref().ok_or(E_INVALIDARG)?;
            // SAFETY: Shell lends the target PIDL. Do not render IDataObject formats here:
            // Initialize may happen while Explorer is still constructing the folder view.
            let destination = match unsafe { selection::destination(folder) } {
                Ok(destination) => destination,
                Err(error) => {
                    crate::trace(
                        "transfer_capture_ignored",
                        serde_json::json!({"hresult":error.code().0}),
                    );
                    // Unsupported Shell formats are outside this handler. Returning success
                    // with no prepared request lets the original Windows operation continue.
                    return Ok(());
                }
            };
            self.state
                .lock()
                .map_err(|_| windows::core::Error::from(E_FAIL))?
                .pending = Some((destination, data.clone()));
            Ok(())
        })
    }
}

struct Inserted {
    menu: HMENU,
    first: u32,
    count: u32,
    committed: bool,
}
impl Drop for Inserted {
    fn drop(&mut self) {
        if !self.committed {
            for i in (0..self.count).rev() {
                // SAFETY: These are exactly the IDs added to the borrowed menu by this call.
                if let Err(error) = unsafe { RemoveMenu(self.menu, self.first + i, MF_BYCOMMAND) } {
                    eprintln!("VartaCopy: menu rollback failed: {error}");
                }
            }
        }
    }
}
fn prepare_menu(
    menu: HMENU,
    index: u32,
    first: u32,
    last: u32,
    flags: u32,
    request: &mut TransferRequest,
    choose: impl FnOnce(&TransferRequest) -> Result<Choice>,
) -> Result<bool> {
    // CMF_DEFAULTONLY is not a drop-menu invocation. Never modify ordinary default verbs.
    if flags & 1 != 0 || first <= 2 || first.checked_add(2).is_none_or(|end| end > last) {
        return Ok(false);
    }
    // SAFETY: The caller lends a valid HMENU for the duration of this method.
    let original = unsafe { GetMenuDefaultItem(menu, 0, Default::default()) };
    request.operation = match original {
        1 => TransferOperation::Copy,
        2 => TransferOperation::Move,
        _ => return Ok(false),
    };
    let titles = [
        "Копіювати через VartaCopy",
        "Перемістити через VartaCopy",
        "Скасувати передачу",
    ];
    let mut inserted = Inserted {
        menu,
        first,
        count: 0,
        committed: false,
    };
    for (offset, title) in titles.iter().enumerate() {
        let text = title.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        // SAFETY: Shell allocates this ID range, and InsertMenu copies the live text.
        unsafe {
            InsertMenuW(
                menu,
                index + offset as u32,
                MF_BYPOSITION | MF_STRING,
                (first + offset as u32) as usize,
                windows::core::PCWSTR(text.as_ptr()),
            )?;
        }
        inserted.count += 1;
    }
    let choice = match choose(request) {
        Ok(choice) => choice,
        Err(error) => {
            choice::error(&format!("Не вдалося відкрити вибір обробника: {error}"));
            Choice::Cancel
        }
    };
    let new_default = match choice {
        Choice::Explorer => None,
        Choice::Cancel => Some(first + 2),
        Choice::VartaCopy => Some(first + u32::from(request.operation == TransferOperation::Move)),
    };
    if let Some(id) = new_default {
        // SAFETY: The selected default is an enabled command inserted above.
        unsafe {
            SetMenuDefaultItem(menu, id, 0)?;
        }
    }
    crate::trace(
        "transfer_choice",
        serde_json::json!({"choice":format!("{choice:?}")}),
    );
    inserted.committed = true;
    Ok(true)
}
impl IContextMenu_Impl for TransferHandler_Impl {
    fn QueryContextMenu(
        &self,
        menu: HMENU,
        index: u32,
        first: u32,
        last: u32,
        flags: u32,
    ) -> HRESULT {
        match ffi(|| {
            // SAFETY: The Shell lends a valid menu throughout this callback.
            let default = unsafe { GetMenuDefaultItem(menu, 0, Default::default()) };
            crate::trace(
                "transfer_query",
                serde_json::json!({"default":default,"index":index,"first":first,"last":last,"flags":flags}),
            );
            if flags & 1 != 0
                || !matches!(default, 1 | 2)
                || first <= 2
                || first.checked_add(2).is_none_or(|end| end > last)
            {
                return Ok(0);
            }
            let configuration = config::read();
            if matches!(&configuration, Ok(None)) {
                return Ok(0);
            }
            let (existing, pending) = {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| windows::core::Error::from(E_FAIL))?;
                if state.querying || state.consumed {
                    return Ok(0);
                }
                if state.request.is_none() && state.pending.is_none() {
                    return Ok(0);
                }
                state.querying = true;
                (state.request.clone(), state.pending.clone())
            };
            let mut request = if let Some(request) = existing {
                request
            } else {
                let (destination, data) = pending.ok_or(E_INVALIDARG)?;
                match selection::capture(destination, &data) {
                    Ok(request) => {
                        crate::trace(
                            "transfer_captured",
                            serde_json::json!({"roots":request.sources.len()}),
                        );
                        request
                    }
                    Err(error) => {
                        self.state
                            .lock()
                            .map_err(|_| windows::core::Error::from(E_FAIL))?
                            .querying = false;
                        crate::trace(
                            "transfer_capture_ignored",
                            serde_json::json!({"hresult":error.code().0}),
                        );
                        return Ok(0);
                    }
                }
            };
            let mut selected_config = None;
            let outcome = match configuration {
                Ok(Some(config)) => {
                    selected_config = Some(config);
                    prepare_menu(menu, index, first, last, flags, &mut request, choice::show)
                }
                Ok(None) => Ok(false), // This user has not enabled the extension.
                Err(error) => {
                    choice::error(&format!(
                        "Налаштування інтеграції недоступні: {error}\nПередачу скасовано."
                    ));
                    prepare_menu(menu, index, first, last, flags, &mut request, |_| {
                        Ok(Choice::Cancel)
                    })
                }
            };
            let mut state = self
                .state
                .lock()
                .map_err(|_| windows::core::Error::from(E_FAIL))?;
            state.querying = false;
            state.prepared = outcome?;
            state.request = Some(request);
            state.config = selected_config;
            Ok(if state.prepared { 3 } else { 0 })
        }) {
            Ok(count) => HRESULT(count),
            Err(error) => error.code(),
        }
    }
    fn InvokeCommand(&self, info: *const CMINVOKECOMMANDINFO) -> Result<()> {
        ffi(|| {
            if info.is_null() {
                return Err(E_INVALIDARG.into());
            }
            // SAFETY: COM supplies at least cbSize; verify the whole base structure before reading it.
            if unsafe { info.cast::<u32>().read() }
                < std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32
            {
                return Err(E_INVALIDARG.into());
            }
            // SAFETY: The caller's cbSize describes a complete CMINVOKECOMMANDINFO.
            let info = unsafe { &*info };
            let verb = info.lpVerb.as_ptr() as usize;
            crate::trace(
                "transfer_invoke",
                serde_json::json!({"numeric":verb<=u16::MAX as usize,"offset":if verb<=u16::MAX as usize {Some(verb)} else {None}}),
            );
            if verb > u16::MAX as usize {
                return Err(E_NOTIMPL.into());
            }
            if verb > 2 {
                return Err(E_INVALIDARG.into());
            }
            let (request, config) = {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| windows::core::Error::from(E_FAIL))?;
                if !state.prepared {
                    return Err(E_INVALIDARG.into());
                }
                if state.consumed || verb == 2 {
                    state.consumed = true;
                    return Ok(());
                }
                let mut request = state.request.clone().ok_or(E_INVALIDARG)?;
                request.operation = if verb == 0 {
                    TransferOperation::Copy
                } else {
                    TransferOperation::Move
                };
                let config = state.config.clone().ok_or(E_FAIL)?;
                state.consumed = true; // No fallback or automatic retry after any delivery attempt.
                (request, config)
            };
            if let Err(error) = delivery::send(config, request) {
                choice::error(&format!("Не вдалося передати запит VartaCopy: {error}"));
                return Err(windows::core::Error::new(E_FAIL, error.to_string()));
            }
            Ok(())
        })
    }
    fn GetCommandString(
        &self,
        id: usize,
        kind: u32,
        _: *const u32,
        out: PSTR,
        max: u32,
    ) -> Result<()> {
        ffi(|| {
            if id > 2 {
                return Err(E_INVALIDARG.into());
            }
            if kind == 2 || kind == 6 {
                return Ok(());
            }
            // Canonical string invocation is deliberately unsupported; do not advertise a verb.
            if kind == 0 || kind == 4 {
                return Err(E_NOTIMPL.into());
            }
            let text = match id {
                0 => "Копіювати через VartaCopy",
                1 => "Перемістити через VartaCopy",
                _ => "Скасувати передачу",
            };
            if kind != 5 || out.is_null() {
                return Err(E_NOTIMPL.into());
            }
            let text = text.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
            if text.len() > max as usize {
                return Err(E_INVALIDARG.into());
            }
            // SAFETY: GCS_HELPTEXTW supplies max writable wide characters, checked above.
            unsafe {
                std::ptr::copy_nonoverlapping(text.as_ptr(), out.0.cast::<u16>(), text.len());
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use vartacopy_shell_windows::transfer::TransferOrigin;
    use windows::Win32::System::Com::{
        FORMATETC, IAdviseSink, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, STGMEDIUM,
    };
    use windows::{
        Win32::UI::WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, DestroyMenu, GetMenuItemCount,
        },
        core::{Interface, w},
    };
    #[implement(IDataObject)]
    struct UnrenderedData {
        reads: Arc<AtomicUsize>,
    }
    impl IDataObject_Impl for UnrenderedData_Impl {
        fn GetData(&self, _: *const FORMATETC) -> Result<STGMEDIUM> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Err(E_NOTIMPL.into())
        }
        fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> Result<()> {
            Err(E_NOTIMPL.into())
        }
        fn QueryGetData(&self, _: *const FORMATETC) -> HRESULT {
            self.reads.fetch_add(1, Ordering::SeqCst);
            E_NOTIMPL
        }
        fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, _: *mut FORMATETC) -> HRESULT {
            E_NOTIMPL
        }
        fn SetData(
            &self,
            _: *const FORMATETC,
            _: *const STGMEDIUM,
            _: windows::core::BOOL,
        ) -> Result<()> {
            Err(E_NOTIMPL.into())
        }
        fn EnumFormatEtc(&self, _: u32) -> Result<IEnumFORMATETC> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Err(E_NOTIMPL.into())
        }
        fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<IAdviseSink>) -> Result<u32> {
            Err(E_NOTIMPL.into())
        }
        fn DUnadvise(&self, _: u32) -> Result<()> {
            Err(E_NOTIMPL.into())
        }
        fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
            Err(E_NOTIMPL.into())
        }
    }
    #[test]
    fn initialize_retains_data_without_rendering_formats_or_blocking_folder_construction() {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::{
            System::Com::{
                COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
            },
            UI::Shell::SHParseDisplayName,
        };
        let _guard = crate::TEST_COM_LOCK.lock().unwrap();
        struct Apartment;
        impl Drop for Apartment {
            fn drop(&mut self) {
                /* SAFETY: balanced on this test's thread. */
                unsafe {
                    CoUninitialize();
                }
            }
        }
        // SAFETY: All owned interfaces are released before the successful STA initialization ends.
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
            let _apartment = Apartment;
            let directory = tempfile::tempdir().unwrap();
            let wide = directory
                .path()
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>();
            let mut pidl = std::ptr::null_mut();
            SHParseDisplayName(
                windows::core::PCWSTR(wide.as_ptr()),
                None,
                &mut pidl,
                0,
                None,
            )
            .unwrap();
            let reads = Arc::new(AtomicUsize::new(0));
            let data: IDataObject = UnrenderedData {
                reads: reads.clone(),
            }
            .into();
            let init: IShellExtInit = TransferHandler::new().into();
            let initialized = init.Initialize(Some(pidl), &data, None);
            CoTaskMemFree(Some(pidl.cast()));
            initialized.unwrap();
            assert_eq!(
                reads.load(Ordering::SeqCst),
                0,
                "Initialize must not request data formats"
            );
            drop(data);
            assert_eq!(
                Arc::strong_count(&reads),
                2,
                "handler retains the data object on its apartment"
            );
            let menu: IContextMenu = init.cast().unwrap();
            let popup = CreatePopupMenu().unwrap();
            AppendMenuW(popup, MF_STRING, 1, w!("Windows copy")).unwrap();
            SetMenuDefaultItem(popup, 1, 0).unwrap();
            assert_eq!(menu.QueryContextMenu(popup, 1, 100, 102, 1), HRESULT(0));
            assert_eq!(
                reads.load(Ordering::SeqCst),
                0,
                "DEFAULTONLY must not render data either"
            );
            assert_eq!(GetMenuItemCount(Some(popup)), 1);
            DestroyMenu(popup).unwrap();
            drop(menu);
            drop(init);
            assert_eq!(Arc::strong_count(&reads), 1);
        }
    }
    #[test]
    fn choosing_changes_only_the_menu_and_cancel_has_a_dedicated_noop_command() {
        let _guard = crate::TEST_COM_LOCK.lock().unwrap();
        let mut request = TransferRequest {
            id: [31; 16],
            operation: TransferOperation::Copy,
            origin: TransferOrigin::Explorer,
            sources: vec![r"D:\source.txt".into()],
            destination: r"D:\target".into(),
        };
        for operation in [1, 2] {
            for choice in [Choice::Explorer, Choice::VartaCopy, Choice::Cancel] {
                // SAFETY: This test exclusively owns the menu and destroys it after the call.
                unsafe {
                    let menu = CreatePopupMenu().unwrap();
                    AppendMenuW(menu, MF_STRING, 1, w!("Windows copy")).unwrap();
                    AppendMenuW(menu, MF_STRING, 2, w!("Windows move")).unwrap();
                    SetMenuDefaultItem(menu, operation, 0).unwrap();
                    assert!(
                        !prepare_menu(menu, 2, 100, 102, 1, &mut request, |_| panic!(
                            "DEFAULTONLY must not ask"
                        ))
                        .unwrap()
                    );
                    assert!(
                        prepare_menu(menu, 2, 100, 102, 0, &mut request, |_| Ok(choice)).unwrap()
                    );
                    assert_eq!(GetMenuItemCount(Some(menu)), 5);
                    let expected = match choice {
                        Choice::Explorer => operation,
                        Choice::VartaCopy => 100 + u32::from(operation == 2),
                        Choice::Cancel => 102,
                    };
                    assert_eq!(GetMenuDefaultItem(menu, 0, Default::default()), expected);
                    assert_eq!(
                        request.operation,
                        if operation == 1 {
                            TransferOperation::Copy
                        } else {
                            TransferOperation::Move
                        }
                    );
                    DestroyMenu(menu).unwrap();
                }
            }
        }
        let object = TransferHandler::new();
        {
            let mut state = object.state.lock().unwrap();
            state.prepared = true;
            state.request = Some(request);
        }
        let menu: IContextMenu = object.into();
        let mut info = CMINVOKECOMMANDINFO {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
            // MAKEINTRESOURCEA(2): the cancel command offset, never string memory.
            lpVerb: windows::core::PCSTR(std::ptr::without_provenance(2)),
            ..Default::default()
        };
        // SAFETY: The descriptor is owned and complete. No config/executable exists:
        // Cancel and every later duplicate must succeed without attempting delivery.
        unsafe {
            menu.InvokeCommand(&info).unwrap();
            info.lpVerb = windows::core::PCSTR::null();
            menu.InvokeCommand(&info).unwrap();
        }
    }
}
