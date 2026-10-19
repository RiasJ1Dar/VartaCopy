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
            *self
                .state
                .lock()
                .map_err(|_| windows::core::Error::from(E_FAIL))? = State::default();
            let data = data.as_ref().ok_or(E_INVALIDARG)?;
            // SAFETY: Shell lends the absolute target PIDL and data object for this callback.
            let request = unsafe { selection::capture(folder, data)? };
            self.state
                .lock()
                .map_err(|_| windows::core::Error::from(E_FAIL))?
                .request = Some(request);
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
            let mut request = {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| windows::core::Error::from(E_FAIL))?;
                if state.querying || state.consumed {
                    return Ok(0);
                }
                let Some(request) = state.request.clone() else {
                    return Ok(0);
                };
                state.querying = true;
                request
            };
            let configuration = config::read();
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
    use vartacopy_shell_windows::transfer::TransferOrigin;
    use windows::{
        Win32::UI::WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, DestroyMenu, GetMenuItemCount,
        },
        core::w,
    };
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
