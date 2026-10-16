use std::{
    ffi::{OsStr, OsString, c_void},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr,
};
use windows::{
    Win32::{
        Foundation::{
            CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_INVALIDARG, E_NOINTERFACE,
            E_NOTIMPL, E_POINTER, S_FALSE, S_OK,
        },
        System::Com::{
            COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize, IClassFactory,
        },
        UI::Shell::{
            ECS_ENABLED, ECS_HIDDEN, IExplorerCommand, IShellItem, IShellItemArray,
            SHCreateItemFromParsingName, SHCreateShellItemArrayFromIDLists,
            SHCreateShellItemArrayFromShellItem, SHGetIDListFromObject,
        },
    },
    core::{BOOL, GUID, HRESULT, IUnknown, Interface, PCWSTR, PWSTR},
};

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().collect()
}

struct Apartment;
impl Apartment {
    fn new() -> Self {
        // SAFETY: each test initializes its own worker thread, balances on Drop.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .unwrap();
        Self
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: Apartment exists only after a successful matching initialize.
        unsafe { CoUninitialize() };
    }
}

fn item(path: &Path) -> IShellItem {
    let mut value = wide(path.as_os_str());
    value.push(0);
    // SAFETY: terminated native path; returned interface owns its COM reference.
    unsafe { SHCreateItemFromParsingName(PCWSTR(value.as_ptr()), None) }.unwrap()
}

fn selection(path: &Path) -> IShellItemArray {
    let item = item(path);
    // SAFETY: live Shell item; returned array owns its reference.
    unsafe { SHCreateShellItemArrayFromShellItem(&item) }.unwrap()
}

fn obtain_factory() -> IClassFactory {
    let mut raw = ptr::null_mut();
    // SAFETY: inputs and writable out storage meet DllGetClassObject contract.
    unsafe { crate::DllGetClassObject(&crate::COMMAND_CLSID, &IClassFactory::IID, &mut raw) }
        .ok()
        .unwrap();
    // SAFETY: successful export returns the initial owning factory reference.
    unsafe { IClassFactory::from_raw(raw) }
}

fn command(factory: &IClassFactory) -> IExplorerCommand {
    // SAFETY: class factory and requested interface are valid; no aggregation.
    unsafe { factory.CreateInstance(None) }.unwrap()
}

#[test]
fn identity_is_loaded_from_shared_contract() {
    let json: serde_json::Value =
        serde_json::from_str(include_str!("../../../specs/explorer-identity-v1.json")).unwrap();
    assert_eq!(
        crate::COMMAND_TITLE,
        json["command_title"].as_str().unwrap()
    );
    assert_eq!(crate::GUI_FILENAME, json["gui_filename"].as_str().unwrap());
    assert_eq!(
        crate::COMMAND_CLSID,
        GUID::from_u128(0xc282efce_1e66_41b6_a895_b43bcb86c1fb)
    );
}

#[test]
fn native_paths_and_arguments_remain_lossless_and_separate() {
    let native = [67, 58, 92, 0x0414, 32, 38, 32, 0xd800, 92, 0xD83D, 0xDE42];
    let source = OsString::from_wide(&native);
    assert!(crate::selection::local_components(&native).is_ok());
    let dll = PathBuf::from(r"C:\App Space\vartacopy_explorer.dll");
    let gui = crate::launch::gui_from_module(&dll).unwrap();
    assert_eq!(gui, Path::new(r"C:\App Space\vartacopy-gui.exe"));
    let cmd = crate::launch::source_command(&gui, &source).unwrap();
    let args: Vec<_> = cmd.get_args().collect();
    assert_eq!(cmd.get_program(), gui.as_os_str());
    assert_eq!(args.len(), 2);
    assert_eq!(args[0], OsStr::new("--source"));
    assert_eq!(wide(args[1]), native);
    assert_eq!(cmd.get_current_dir(), gui.parent());
    assert_eq!(
        crate::launch::gui_from_module(Path::new("relative.dll")),
        Err(E_INVALIDARG)
    );
}

#[test]
fn unsupported_path_syntax_is_rejected_without_filesystem_calls() {
    for path in [
        r"C:\",
        r"C:",
        r"folder",
        r"\folder",
        r"\\host\share\folder",
        r"\\?\UNC\host\share\folder",
        r"\\.\C:\folder",
        r"C:\folder\..\child",
        r"C:\folder\.",
        r"C:\folder\",
        r"C:\folder:stream",
        r"C:\folder.\child",
        r"C:\folder \child",
        r"C:/folder",
    ] {
        assert_eq!(
            crate::selection::local_components(&wide(OsStr::new(path))),
            Err(E_INVALIDARG),
            "{path}"
        );
    }
    assert!(
        crate::selection::local_components(&wide(OsStr::new(r"\\?\C:\ordinary\folder"))).is_ok()
    );
}

#[test]
fn launch_failures_return_hresult_without_starting_a_process() {
    let temp = tempfile::tempdir().unwrap();
    let absent = temp.path().join("missing-gui.exe");
    let error = crate::launch::launch(&absent, OsStr::new(r"C:\source")).unwrap_err();
    assert_eq!(error, HRESULT::from_win32(2));
    let invalid = temp.path().join("invalid.exe");
    std::fs::write(&invalid, b"not a PE executable").unwrap();
    assert!(
        crate::launch::launch(&invalid, OsStr::new(r"C:\source"))
            .unwrap_err()
            .is_err()
    );
    assert!(crate::launch::source_command(Path::new("relative.exe"), OsStr::new("x")).is_err());
}

#[test]
fn com_lifetime_nulls_interfaces_and_locks() {
    let _apartment = Apartment::new();
    // Keep all global unload assertions in this one test. Other tests never create
    // our COM objects, so test-worker concurrency cannot invalidate the counts.
    assert_eq!(crate::DllCanUnloadNow(), S_OK);
    let mut raw = ptr::dangling_mut::<c_void>();
    // SAFETY: all non-null pointers below refer to valid inputs/output storage;
    // null parameters deliberately verify the COM boundary's validation.
    unsafe {
        assert_eq!(
            crate::DllGetClassObject(ptr::null(), &IClassFactory::IID, &mut raw),
            E_POINTER
        );
        assert!(raw.is_null());
        assert_eq!(
            crate::DllGetClassObject(&GUID::zeroed(), &IClassFactory::IID, &mut raw),
            CLASS_E_CLASSNOTAVAILABLE
        );
        assert_eq!(
            crate::DllGetClassObject(&crate::COMMAND_CLSID, &IExplorerCommand::IID, &mut raw),
            E_NOINTERFACE
        );
        assert!(raw.is_null());
        assert_eq!(
            crate::DllGetClassObject(&crate::COMMAND_CLSID, ptr::null(), &mut raw),
            E_POINTER
        );
        assert_eq!(
            crate::DllGetClassObject(&crate::COMMAND_CLSID, &IClassFactory::IID, ptr::null_mut()),
            E_POINTER
        );
    }
    let factory = obtain_factory();
    let fv = factory.vtable();
    assert_eq!(crate::DllCanUnloadNow(), S_FALSE);
    // SAFETY: factory is live; raw callback pointers and out storage are valid.
    unsafe {
        assert_eq!((fv.base__.AddRef)(factory.as_raw()), 2);
        assert_eq!((fv.base__.Release)(factory.as_raw()), 1);
        assert_eq!(
            (fv.CreateInstance)(
                factory.as_raw(),
                factory.as_raw(),
                &IExplorerCommand::IID,
                &mut raw
            ),
            CLASS_E_NOAGGREGATION
        );
        assert!(raw.is_null());
        assert_eq!(
            (fv.CreateInstance)(factory.as_raw(), ptr::null_mut(), &GUID::zeroed(), &mut raw),
            E_NOINTERFACE
        );
        assert_eq!(
            (fv.CreateInstance)(factory.as_raw(), ptr::null_mut(), ptr::null(), &mut raw),
            E_POINTER
        );
        assert_eq!(
            (fv.CreateInstance)(
                factory.as_raw(),
                ptr::null_mut(),
                &IExplorerCommand::IID,
                ptr::null_mut()
            ),
            E_POINTER
        );
        assert_eq!((fv.LockServer)(factory.as_raw(), BOOL(0)), E_INVALIDARG);
        assert_eq!((fv.LockServer)(factory.as_raw(), BOOL(1)), S_OK);
    }
    let command = command(&factory);
    let cv = command.vtable();
    // SAFETY: live command interface and writable storage, including deliberate
    // null pointers only in positions checked by the callbacks before access.
    unsafe {
        assert_eq!(
            (cv.base__.QueryInterface)(command.as_raw(), &GUID::zeroed(), &mut raw),
            E_NOINTERFACE
        );
        assert!(raw.is_null());
        assert_eq!(
            (cv.base__.QueryInterface)(command.as_raw(), ptr::null(), &mut raw),
            E_POINTER
        );
        assert_eq!(
            (cv.base__.QueryInterface)(command.as_raw(), &IUnknown::IID, ptr::null_mut()),
            E_POINTER
        );
        assert_eq!(
            (cv.base__.QueryInterface)(command.as_raw(), &IUnknown::IID, &mut raw),
            S_OK
        );
        assert_eq!(raw, command.as_raw());
        assert_eq!((cv.base__.Release)(raw), 1);
        assert_eq!((cv.base__.AddRef)(command.as_raw()), 2);
        assert_eq!((cv.base__.Release)(command.as_raw()), 1);
        assert_eq!(
            (cv.GetTitle)(command.as_raw(), ptr::null_mut(), ptr::null_mut()),
            E_POINTER
        );
        let mut title = PWSTR::null();
        assert_eq!(
            (cv.GetTitle)(command.as_raw(), ptr::null_mut(), &mut title),
            S_OK
        );
        assert_eq!(
            title.as_wide(),
            crate::COMMAND_TITLE.encode_utf16().collect::<Vec<_>>()
        );
        CoTaskMemFree(Some(title.0.cast()));
        let mut no_icon = PWSTR(ptr::dangling_mut());
        assert_eq!(
            (cv.GetIcon)(command.as_raw(), ptr::null_mut(), &mut no_icon),
            E_NOTIMPL
        );
        assert!(no_icon.is_null());
        assert_eq!(
            (cv.GetIcon)(command.as_raw(), ptr::null_mut(), ptr::null_mut()),
            E_POINTER
        );
        assert_eq!(
            (cv.GetToolTip)(command.as_raw(), ptr::null_mut(), &mut no_icon),
            E_NOTIMPL
        );
        assert_eq!(
            (cv.GetToolTip)(command.as_raw(), ptr::null_mut(), ptr::null_mut()),
            E_POINTER
        );
        let mut canonical = GUID::zeroed();
        assert_eq!(
            (cv.GetCanonicalName)(command.as_raw(), &mut canonical),
            S_OK
        );
        assert_eq!(canonical, crate::COMMAND_CLSID);
        assert_eq!(
            (cv.GetCanonicalName)(command.as_raw(), ptr::null_mut()),
            E_POINTER
        );
        let mut flags = 99;
        assert_eq!((cv.GetFlags)(command.as_raw(), &mut flags), S_OK);
        assert_eq!(flags, 0);
        assert_eq!((cv.GetFlags)(command.as_raw(), ptr::null_mut()), E_POINTER);
        assert_eq!((cv.EnumSubCommands)(command.as_raw(), &mut raw), E_NOTIMPL);
        assert!(raw.is_null());
        assert_eq!(
            (cv.EnumSubCommands)(command.as_raw(), ptr::null_mut()),
            E_POINTER
        );
        let mut state = 99;
        assert_eq!(
            (cv.GetState)(command.as_raw(), ptr::null_mut(), BOOL(0), &mut state),
            S_OK
        );
        assert_eq!(state, ECS_HIDDEN.0 as u32);
        assert_eq!(
            (cv.GetState)(command.as_raw(), ptr::null_mut(), BOOL(0), ptr::null_mut()),
            E_POINTER
        );
        assert_eq!(
            (cv.Invoke)(command.as_raw(), ptr::null_mut(), ptr::null_mut()),
            E_INVALIDARG
        );
    }
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let selected = selection(&source);
    let mut state = 99;
    // SAFETY: real Shell selection and live command; outputs are valid storage.
    unsafe {
        assert_eq!(
            (cv.GetState)(command.as_raw(), selected.as_raw(), BOOL(0), &mut state),
            HRESULT(0x8000000A_u32 as i32)
        );
        assert_eq!(
            (cv.GetState)(command.as_raw(), selected.as_raw(), BOOL(1), &mut state),
            S_OK
        );
        assert_eq!(state, ECS_ENABLED.0 as u32);
    }
    std::fs::remove_dir(&source).unwrap();
    // SAFETY: array is still live after the source vanishes. Invoke revalidation
    // must fail before resolving/launching the GUI, so no process can start.
    unsafe {
        assert!((cv.Invoke)(command.as_raw(), selected.as_raw(), ptr::null_mut()).is_err());
        assert!((cv.GetState)(command.as_raw(), selected.as_raw(), BOOL(1), &mut state).is_err());
        assert_eq!(state, ECS_HIDDEN.0 as u32);
    }
    drop(factory);
    assert_eq!(crate::DllCanUnloadNow(), S_FALSE);
    // A server lock persists even when its original factory was released.
    drop(command);
    assert_eq!(crate::DllCanUnloadNow(), S_FALSE);
    let factory = obtain_factory();
    // SAFETY: live factory; release the balanced lock held above.
    unsafe { factory.LockServer(false) }.unwrap();
    drop(factory);
    assert_eq!(crate::DllCanUnloadNow(), S_OK);
    assert_eq!(crate::com::catches_panic(), E_FAIL);
}

#[test]
fn real_shell_selections_validate_folder_file_mixed_root_and_virtual() {
    let _apartment = Apartment::new();
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("Папка & пробіл 🙂");
    std::fs::create_dir(&directory).unwrap();
    let folder_selection = selection(&directory);
    let returned = crate::selection::selection_path(&folder_selection).unwrap();
    assert_eq!(
        std::fs::canonicalize(returned).unwrap(),
        std::fs::canonicalize(&directory).unwrap()
    );
    let file = temp.path().join("file.txt");
    std::fs::write(&file, b"fixture").unwrap();
    assert_eq!(
        crate::selection::selection_path(&selection(&file)),
        Err(E_INVALIDARG)
    );
    let drive_root: PathBuf = directory.components().take(2).collect();
    assert_eq!(
        crate::selection::selection_path(&selection(&drive_root)),
        Err(E_INVALIDARG)
    );
    // SAFETY: native Shell item inputs are live; CoTaskMem allocations are freed
    // after the multi-selection array has taken its own data/reference ownership.
    unsafe {
        let folder_pidl = SHGetIDListFromObject(&item(&directory)).unwrap();
        let file_pidl = SHGetIDListFromObject(&item(&file)).unwrap();
        let mixed = SHCreateShellItemArrayFromIDLists(&[folder_pidl, file_pidl]).unwrap();
        CoTaskMemFree(Some(folder_pidl.cast()));
        CoTaskMemFree(Some(file_pidl.cast()));
        assert_eq!(crate::selection::selection_path(&mixed), Err(E_INVALIDARG));
        let virtual_name: Vec<_> = "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}"
            .encode_utf16()
            .chain([0])
            .collect();
        let computer: IShellItem =
            SHCreateItemFromParsingName(PCWSTR(virtual_name.as_ptr()), None).unwrap();
        let virtual_selection: IShellItemArray =
            SHCreateShellItemArrayFromShellItem(&computer).unwrap();
        assert!(crate::selection::selection_path(&virtual_selection).is_err());
    }
    // Delete after the prior validation: Invoke must not trust cached menu state.
    std::fs::remove_dir(&directory).unwrap();
    assert!(crate::selection::selection_path(&folder_selection).is_err());
}

#[test]
fn module_locator_returns_its_own_image() {
    let module = crate::launch::module_path().unwrap();
    assert!(module.is_absolute());
    // Here the code is linked into the test executable. The DLL smoke check uses
    // LoadLibrary/GetProcAddress separately, outside the COM registration system.
    assert!(
        wide(module.file_name().unwrap()).starts_with(&wide(OsStr::new("vartacopy_explorer-")))
    );
}

#[test]
fn reparse_folder_and_reparse_ancestor_are_rejected() {
    use std::{
        fs::OpenOptions,
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    };
    use windows::Win32::{Foundation::HANDLE, System::IO::DeviceIoControl};

    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target");
    std::fs::create_dir_all(target.join("child")).unwrap();
    let junction = temp.path().join("junction");
    std::fs::create_dir(&junction).unwrap();
    let file = OpenOptions::new()
        .access_mode(0x40000000) // GENERIC_WRITE
        .custom_flags(0x02000000 | 0x00200000) // BACKUP_SEMANTICS | OPEN_REPARSE_POINT
        .open(&junction)
        .unwrap();
    let canonical = std::fs::canonicalize(&target).unwrap();
    let native = wide(canonical.as_os_str());
    assert!(native.starts_with(&[92, 92, 63, 92]));
    let substitute: Vec<u16> = [92, 63, 63, 92]
        .into_iter()
        .chain(native[4..].iter().copied())
        .chain([0])
        .collect();
    let print_name: Vec<u16> = native[4..].iter().copied().chain([0]).collect();
    let mut buffer = Vec::new();
    buffer.extend_from_slice(&0xa0000003u32.to_le_bytes()); // IO_REPARSE_TAG_MOUNT_POINT
    buffer
        .extend_from_slice(&(8 + ((substitute.len() + print_name.len()) * 2) as u16).to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(&(((substitute.len() - 1) * 2) as u16).to_le_bytes());
    buffer.extend_from_slice(&((substitute.len() * 2) as u16).to_le_bytes());
    buffer.extend_from_slice(&(((print_name.len() - 1) * 2) as u16).to_le_bytes());
    for unit in substitute.into_iter().chain(print_name) {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }
    let mut returned = 0;
    // SAFETY: handle owns the newly created fixture directory opened with
    // OPEN_REPARSE_POINT. The byte buffer is the documented mount-point reparse
    // layout; this changes only that synthetic directory, with no registration.
    unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle()),
            0x000900a4,
            Some(buffer.as_ptr().cast()),
            buffer.len() as u32,
            None,
            0,
            Some(&mut returned),
            None,
        )
    }
    .unwrap();
    drop(file);
    assert_eq!(
        crate::selection::ordinary_local_folder(&wide(junction.as_os_str())),
        Err(E_INVALIDARG)
    );
    assert_eq!(
        crate::selection::ordinary_local_folder(&wide(junction.join("child").as_os_str())),
        Err(E_INVALIDARG)
    );
    std::fs::remove_dir(&junction).unwrap();
    assert!(target.join("child").is_dir());
}
