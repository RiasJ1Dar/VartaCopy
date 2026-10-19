//! Explorer transfer selection and an independent, opt-in passive callback probe.
#![cfg(windows)]
#![allow(non_snake_case)]
mod choice;
mod config;
mod controls;
mod delivery;
mod handler;
mod selection;

use std::{
    ffi::c_void,
    fs::OpenOptions,
    io::{self, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use windows::{
    Win32::{
        Foundation::{
            CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_NOTIMPL, E_POINTER,
            S_FALSE, S_OK,
        },
        System::{
            Com::{IClassFactory, IClassFactory_Impl, IDataObject},
            Registry::HKEY,
            Threading::{GetCurrentProcessId, GetCurrentThreadId},
        },
        UI::{
            Shell::{
                CMINVOKECOMMANDINFO, Common::ITEMIDLIST, IContextMenu, IContextMenu_Impl,
                IShellExtInit, IShellExtInit_Impl, IShellItem, IShellItemArray,
                SHCreateItemFromIDList, SHCreateShellItemArrayFromDataObject,
            },
            WindowsAndMessaging::{GetMenuDefaultItem, GetMenuItemCount, HMENU},
        },
    },
    core::{BOOL, GUID, HRESULT, IUnknown, Interface, PSTR, Ref, Result, implement},
};

// Independent identity, exclusively for the opt-in diagnostic probe.
pub const PROBE_CLSID: GUID = GUID::from_u128(0x328db05e_95e2_4427_9e48_d9f11f57e96c);
pub const TRANSFER_CLSID: GUID = GUID::from_u128(0x91dd2708_74c6_4498_87b6_09420a94f66d);
const MAX_LOG_BYTES: u64 = 256 * 1024;
static OBJECTS: AtomicUsize = AtomicUsize::new(0);
static LOCKS: AtomicUsize = AtomicUsize::new(0);
static LOG_LOCK: Mutex<()> = Mutex::new(());
#[cfg(test)]
static TEST_COM_LOCK: Mutex<()> = Mutex::new(());

struct Lifetime;
impl Lifetime {
    fn new() -> Self {
        OBJECTS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}
impl Drop for Lifetime {
    fn drop(&mut self) {
        OBJECTS.fetch_sub(1, Ordering::SeqCst);
    }
}
fn ffi<T>(body: impl FnOnce() -> Result<T>) -> Result<T> {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or_else(|_| Err(E_FAIL.into()))
}

fn append_record(path: &Path, record: &serde_json::Value) -> io::Result<()> {
    let _guard = LOG_LOCK
        .lock()
        .map_err(|_| io::Error::other("probe log lock poisoned"))?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut bytes = serde_json::to_vec(record)?;
    bytes.push(b'\n');
    if file.metadata()?.len().saturating_add(bytes.len() as u64) > MAX_LOG_BYTES {
        return Err(io::Error::other(
            "probe log reached 256 KiB; archive it before another probe",
        ));
    }
    file.write_all(&bytes)
}
fn record(event: &str, detail: serde_json::Value) -> Result<()> {
    let root =
        std::env::var_os("LOCALAPPDATA").ok_or_else(|| windows::core::Error::from(E_FAIL))?;
    let root = std::path::PathBuf::from(root).join("VartaCopy");
    let write = || -> io::Result<()> {
        if !root.is_absolute() {
            return Err(io::Error::other("LOCALAPPDATA must be absolute"));
        }
        std::fs::create_dir_all(&root)?;
        // SAFETY: These APIs have no pointer arguments or preconditions.
        let (pid, tid) = unsafe { (GetCurrentProcessId(), GetCurrentThreadId()) };
        append_record(
            &root.join("explorer-ingress-probe.jsonl"),
            &serde_json::json!({
                "version":1,"event":event,"pid":pid,"tid":tid,
                "unix_ms":SystemTime::now().duration_since(UNIX_EPOCH).map_err(io::Error::other)?.as_millis(),
                "detail":detail
            }),
        )
    };
    write().map_err(|error| {
        windows::core::Error::new(E_FAIL, format!("VartaCopy Explorer probe: {error}"))
    })
}

#[derive(Default)]
struct Selection {
    roots: Option<u32>,
    destination: bool,
}

#[implement(IShellExtInit, IContextMenu)]
struct Probe {
    _lifetime: Lifetime,
    selection: Mutex<Selection>,
}
impl Probe {
    fn new() -> Self {
        Self {
            _lifetime: Lifetime::new(),
            selection: Mutex::new(Selection::default()),
        }
    }
}
impl IShellExtInit_Impl for Probe_Impl {
    fn Initialize(&self, folder: *const ITEMIDLIST, data: Ref<IDataObject>, _: HKEY) -> Result<()> {
        ffi(|| {
            let destination = if folder.is_null() {
                false
            } else {
                // SAFETY: Shell owns the readable PIDL for the duration of Initialize.
                match unsafe { SHCreateItemFromIDList::<IShellItem>(folder) } {
                    Ok(_) => true,
                    Err(error) => {
                        record(
                            "unsupported_destination",
                            serde_json::json!({"hresult":error.code().0}),
                        )?;
                        false
                    }
                }
            };
            let roots = match data.as_ref() {
                Some(data) => {
                    // SAFETY: IDataObject is borrowed from the COM call, on this apartment.
                    match unsafe {
                        SHCreateShellItemArrayFromDataObject::<_, IShellItemArray>(data)
                    } {
                        // SAFETY: The newly returned shell array is a live COM reference.
                        Ok(array) => Some(unsafe { array.GetCount()? }),
                        // Unsupported data is recorded explicitly; the probe must not reject it.
                        Err(error) => {
                            record(
                                "unsupported_data",
                                serde_json::json!({"hresult":error.code().0}),
                            )?;
                            None
                        }
                    }
                }
                None => None,
            };
            *self
                .selection
                .lock()
                .map_err(|_| windows::core::Error::from(E_FAIL))? =
                Selection { roots, destination };
            record(
                "initialize",
                serde_json::json!({"roots":roots,"destination":destination}),
            )
        })
    }
}
impl IContextMenu_Impl for Probe_Impl {
    fn QueryContextMenu(
        &self,
        menu: HMENU,
        index: u32,
        first: u32,
        last: u32,
        flags: u32,
    ) -> HRESULT {
        match ffi(|| {
            let selection = self
                .selection
                .lock()
                .map_err(|_| windows::core::Error::from(E_FAIL))?;
            // SAFETY: Shell owns this menu throughout QueryContextMenu; these calls only read it.
            let (default, count) = unsafe {
                (
                    GetMenuDefaultItem(menu, 0, Default::default()),
                    GetMenuItemCount(Some(menu)),
                )
            };
            record(
                "query_context_menu",
                serde_json::json!({"flags":flags,"index":index,"first":first,"last":last,"default":default,"menu_count":count,"roots":selection.roots,"destination":selection.destination}),
            )
        }) {
            Ok(()) => S_OK, // Zero new command IDs, no menu mutation.
            Err(error) => error.code(),
        }
    }
    fn InvokeCommand(&self, _: *const CMINVOKECOMMANDINFO) -> Result<()> {
        ffi(|| {
            record("unexpected_invoke", serde_json::json!({}))?;
            Err(E_NOTIMPL.into())
        })
    }
    fn GetCommandString(&self, _: usize, _: u32, _: *const u32, _: PSTR, _: u32) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
}

#[implement(IClassFactory)]
struct Factory {
    _lifetime: Lifetime,
    probe: bool,
}
impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<IUnknown>,
        iid: *const GUID,
        out: *mut *mut c_void,
    ) -> Result<()> {
        ffi(|| {
            if out.is_null() || iid.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: COM caller provides writable output storage; initialize before any failure.
            unsafe { *out = std::ptr::null_mut() };
            if outer.is_some() {
                return Err(CLASS_E_NOAGGREGATION.into());
            }
            let object: IShellExtInit = if self.probe {
                Probe::new().into()
            } else {
                handler::TransferHandler::new().into()
            };
            // SAFETY: iid/out are readable/writable COM call parameters; query supplies one owned reference.
            unsafe { object.query(iid, out).ok() }
        })
    }
    fn LockServer(&self, lock: BOOL) -> Result<()> {
        if lock.as_bool() {
            LOCKS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        } else {
            LOCKS
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .map(|_| ())
                .map_err(|_| E_FAIL.into())
        }
    }
}

/// Return a class factory for the passive probe.
/// # Safety
/// All non-null pointers must satisfy the standard DllGetClassObject COM ABI.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    clsid: *const GUID,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    ffi(|| {
        if clsid.is_null() || iid.is_null() || out.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: Caller supplies initialized GUIDs and writable output storage by the documented COM ABI.
        unsafe {
            *out = std::ptr::null_mut();
            if *clsid != PROBE_CLSID && *clsid != TRANSFER_CLSID {
                return Err(CLASS_E_CLASSNOTAVAILABLE.into());
            }
        }
        // SAFETY: clsid was validated above and remains borrowed for this COM call.
        let probe = unsafe { *clsid == PROBE_CLSID };
        if probe {
            record("class_factory", serde_json::json!({}))?;
        }
        let factory: IClassFactory = Factory {
            _lifetime: Lifetime::new(),
            probe,
        }
        .into();
        // SAFETY: Validated ABI parameters; query transfers a reference for the requested supported interface.
        unsafe { factory.query(iid, out).ok() }
    })
    .into()
}
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if OBJECTS.load(Ordering::SeqCst) == 0 && LOCKS.load(Ordering::SeqCst) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::{
        Win32::UI::WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, DestroyMenu, MF_STRING, SetMenuDefaultItem,
        },
        core::w,
    };

    #[test]
    fn probe_com_identity_lifetime_and_callbacks_never_mutate_the_menu() {
        let _guard = TEST_COM_LOCK.lock().unwrap();
        // SAFETY: All COM out parameters point to initialized local storage. The
        // temporary menu is owned here and destroyed after callbacks release it.
        unsafe {
            assert_eq!(DllCanUnloadNow(), S_OK);
            let mut raw = std::ptr::null_mut();
            assert_eq!(
                DllGetClassObject(&PROBE_CLSID, &IClassFactory::IID, &mut raw),
                S_OK
            );
            let factory = IClassFactory::from_raw(raw);
            assert_eq!(DllCanUnloadNow(), S_FALSE);
            let init: IShellExtInit = factory.CreateInstance(None).unwrap();
            let menu: IContextMenu = init.cast().unwrap();
            assert_eq!(
                init.cast::<IUnknown>().unwrap(),
                menu.cast::<IUnknown>().unwrap()
            );
            init.Initialize(None, None, None).unwrap();
            let popup = CreatePopupMenu().unwrap();
            AppendMenuW(popup, MF_STRING, 17, w!("Synthetic copy")).unwrap();
            AppendMenuW(popup, MF_STRING, 29, w!("Synthetic move")).unwrap();
            SetMenuDefaultItem(popup, 29, 0).unwrap();
            for flags in [0, 1, 0x100, 0x400, 0xffff_0000] {
                assert_eq!(menu.QueryContextMenu(popup, 1, 100, 200, flags), S_OK);
                assert_eq!(GetMenuItemCount(Some(popup)), 2);
                assert_eq!(GetMenuDefaultItem(popup, 0, Default::default()), 29);
            }
            assert_eq!(
                menu.GetCommandString(0, 0, None, PSTR::null(), 0)
                    .unwrap_err()
                    .code(),
                E_NOTIMPL
            );
            DestroyMenu(popup).unwrap();
            drop(menu);
            drop(init);
            drop(factory);
            assert_eq!(DllCanUnloadNow(), S_OK);
            assert_eq!(
                DllGetClassObject(&TRANSFER_CLSID, &IClassFactory::IID, &mut raw),
                S_OK
            );
            let factory = IClassFactory::from_raw(raw);
            let init: IShellExtInit = factory.CreateInstance(None).unwrap();
            let menu: IContextMenu = init.cast().unwrap();
            assert_eq!(
                init.cast::<IUnknown>().unwrap(),
                menu.cast::<IUnknown>().unwrap()
            );
            assert!(init.Initialize(None, None, None).is_err());
            let popup = CreatePopupMenu().unwrap();
            assert_eq!(menu.QueryContextMenu(popup, 0, 100, 200, 0), S_OK);
            assert_eq!(GetMenuItemCount(Some(popup)), 0);
            assert!(menu.InvokeCommand(std::ptr::null()).is_err());
            DestroyMenu(popup).unwrap();
            drop(menu);
            drop(init);
            drop(factory);
            assert_eq!(DllCanUnloadNow(), S_OK);
        }
    }
    #[test]
    fn log_is_bounded_and_io_errors_are_reported() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("probe.jsonl");
        append_record(&path, &serde_json::json!({"event":"test"})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_LOG_BYTES)
            .unwrap();
        assert!(append_record(&path, &serde_json::json!({})).is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), MAX_LOG_BYTES);
        assert!(append_record(root.path(), &serde_json::json!({})).is_err());
    }
}
