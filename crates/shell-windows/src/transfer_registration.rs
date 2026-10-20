//! Native, ownership-checked Explorer transfer registration shared by CLI/GUI/setup.
mod journal;
mod registry;
use registry::{Registry, Snapshot, Values};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read, Write},
    os::windows::ffi::OsStrExt,
    path::{Component, Path, PathBuf, Prefix},
};
use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;

pub const CLSID: &str = "{91DD2708-74C6-4498-87B6-09420A94F66D}";
const LABEL: &str = "VartaCopy Explorer transfer";
const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Shell Extensions\Approved";
fn base() -> String {
    format!(r"Software\Classes\CLSID\{CLSID}")
}
fn units(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().collect()
}
fn plain_path(path: &Path) -> io::Result<PathBuf> {
    let value = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if !path.is_absolute()
        || value.contains(&0)
        || value.len() > 32767
        || path.components().any(|c| c == Component::ParentDir)
        || !matches!(path.components().next(),Some(Component::Prefix(p)) if matches!(p.kind(),Prefix::Disk(_)|Prefix::VerbatimDisk(_)))
    {
        return Err(io::Error::other(
            "Integration requires absolute local paths",
        ));
    }
    // Remove only the verbatim local-drive prefix for native registry spelling.
    let verbatim = value.starts_with(&units(r"\\?\"));
    if verbatim && value.contains(&(b'/' as u16)) {
        return Err(io::Error::other(
            "Mixed separators in a verbatim integration path",
        ));
    }
    let mut value = if verbatim { value[4..].to_vec() } else { value };
    for unit in &mut value {
        if *unit == b'/' as u16 {
            *unit = b'\\' as u16;
        }
    }
    Ok(registry::native(&value))
}
fn long_spelling(path: &Path) -> io::Result<PathBuf> {
    use windows_sys::Win32::Storage::FileSystem::GetLongPathNameW;
    let path = plain_path(path)?;
    let mut buffer = vec![0; 32768];
    // SAFETY: Existing validated local path and writable UTF-16 buffer. This expands
    // 8.3 names but does NOT resolve junctions or package filesystem redirection.
    let count = unsafe {
        GetLongPathNameW(
            crate::wide(&path).as_ptr(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
    } as usize;
    if count == 0 {
        return Err(io::Error::last_os_error());
    }
    if count >= buffer.len() {
        return Err(io::Error::other("Integration path is too long"));
    }
    plain_path(&registry::native(&buffer[..count]))
}
#[derive(Debug, Clone)]
pub struct Installation {
    pub gui: PathBuf,
    pub dll: PathBuf,
}
impl Installation {
    pub fn new(gui: &Path, dll: &Path) -> io::Result<Self> {
        let gui = long_spelling(gui)?;
        let dll = long_spelling(dll)?;
        for (file, name) in [
            (&gui, "vartacopy-gui.exe"),
            (&dll, "vartacopy_transfer.dll"),
        ] {
            if !file
                .file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
                || !file.is_file()
            {
                return Err(io::Error::other(format!(
                    "Missing integration component: {}",
                    file.display()
                )));
            }
        }
        // Resolve identity without accepting redirected package-local copies.
        for file in [&gui, &dll] {
            if !same_path(&plain_path(&file.canonicalize()?)?, file) {
                return Err(io::Error::other(
                    "Integration path is redirected or linked; use the physical installation directory",
                ));
            }
        }
        Ok(Self { gui, dll })
    }
    pub fn beside(gui: &Path) -> io::Result<Self> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Locator {
            version: u32,
            dll: String,
        }
        let locator = gui.with_file_name("integration-install.json");
        match fs::File::open(locator) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(4097).read_to_end(&mut bytes)?;
                if bytes.len() > 4096 {
                    return Err(io::Error::other(
                        "Integration location metadata is oversized",
                    ));
                }
                let locator: Locator = serde_json::from_slice(&bytes)?;
                let relative = PathBuf::from(locator.dll);
                if locator.version != 1
                    || relative.as_os_str().is_empty()
                    || relative
                        .components()
                        .any(|c| !matches!(c, Component::Normal(_)))
                {
                    return Err(io::Error::other("Invalid integration location metadata"));
                }
                let root = gui
                    .parent()
                    .ok_or_else(|| io::Error::other("GUI has no installation directory"))?;
                return Self::new(gui, &root.join(relative));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Self::new(gui, &gui.with_file_name("vartacopy_transfer.dll"))
    }
    fn config(&self) -> PathBuf {
        self.dll.with_file_name("explorer-transfer.json")
    }
    fn bytes(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec(&Config {
            version: 1,
            enabled: true,
            gui: units(&self.gui),
            journal: None,
            diagnostics: None,
        })
        .map_err(io::Error::other)
    }
}
fn same_path(a: &Path, b: &Path) -> bool {
    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
    let a = units(a);
    let b = units(b);
    // SAFETY: Bounded live UTF-16 inputs; ordinal comparison preserves unpaired surrogates.
    unsafe {
        CompareStringOrdinal(a.as_ptr(), a.len() as i32, b.as_ptr(), b.len() as i32, 1)
            == CSTR_EQUAL
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    version: u32,
    enabled: bool,
    gui: Vec<u16>,
    journal: Option<Vec<u16>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diagnostics: Option<bool>,
}
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    PendingRecovery { path: PathBuf },
    Disabled,
    Enabled { dll: PathBuf },
    OtherInstallation { dll: PathBuf },
    Incomplete { reason: String },
}
fn expected(dll: &Path) -> BTreeMap<String, Snapshot> {
    let root = base();
    let mut result = BTreeMap::new();
    let value = |name: &str, value: Vec<u16>| Values::from([(name.to_owned(), value)]);
    result.insert(
        root.clone(),
        Snapshot {
            values: value("", units(LABEL)),
            children: BTreeSet::from(["InprocServer32".into(), "shellex".into()]),
        },
    );
    result.insert(
        format!(r"{root}\InprocServer32"),
        Snapshot {
            values: Values::from([
                ("".into(), units(dll)),
                ("ThreadingModel".into(), units("Apartment")),
            ]),
            children: BTreeSet::new(),
        },
    );
    result.insert(
        format!(r"{root}\shellex"),
        Snapshot {
            values: Values::new(),
            children: BTreeSet::from(["MayChangeDefaultMenu".into()]),
        },
    );
    result.insert(
        format!(r"{root}\shellex\MayChangeDefaultMenu"),
        Snapshot {
            values: Values::new(),
            children: BTreeSet::new(),
        },
    );
    for class in ["Directory", "Folder", "*", "Drive", "AllFilesystemObjects"] {
        result.insert(
            format!(r"Software\Classes\{class}\shellex\DragDropHandlers\{CLSID}"),
            Snapshot {
                values: value("", units(CLSID)),
                children: BTreeSet::new(),
            },
        );
    }
    result
}
fn inspect(registry: &Registry, installation: &Installation) -> io::Result<Status> {
    let keys = expected(&installation.dll);
    let snapshots = keys
        .keys()
        .map(|key| registry.snapshot(key).map(|s| (key.clone(), s)))
        .collect::<io::Result<BTreeMap<_, _>>>()?;
    let approval = registry.value(APPROVED, CLSID)?;
    if snapshots.values().all(Option::is_none) && approval.is_none() {
        return Ok(Status::Disabled);
    }
    let Some(dll) = registry.value(&format!(r"{}\InprocServer32", base()), "")? else {
        return Ok(Status::Incomplete {
            reason: "Missing COM server path".into(),
        });
    };
    let dll = plain_path(&registry::native(&dll))?;
    if !dll
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case("vartacopy_transfer.dll"))
    {
        return Ok(Status::Incomplete {
            reason: "Unexpected COM server name".into(),
        });
    }
    for (key, wanted) in expected(&dll) {
        if snapshots.get(&key).and_then(Option::as_ref) != Some(&wanted) {
            return Ok(Status::Incomplete {
                reason: format!("Changed or incomplete integration key: {key}"),
            });
        }
    }
    if approval != Some(units(LABEL)) {
        return Ok(Status::Incomplete {
            reason: "Changed integration approval".into(),
        });
    }
    Ok(if same_path(&dll, &installation.dll) {
        Status::Enabled { dll }
    } else {
        Status::OtherInstallation { dll }
    })
}
fn notify() {
    use windows_sys::Win32::UI::Shell::*;
    // SAFETY: Association-change broadcast has no item payload and does not restart Explorer.
    unsafe {
        SHChangeNotify(
            SHCNE_ASSOCCHANGED as i32,
            SHCNF_IDLIST | SHCNF_FLUSH,
            std::ptr::null(),
            std::ptr::null(),
        );
    }
}
pub fn status(installation: &Installation) -> io::Result<Status> {
    let registry = Registry::read(HKEY_LOCAL_MACHINE, String::new());
    if journal::pending(&registry, installation)? {
        return Ok(Status::PendingRecovery {
            path: journal::location(installation),
        });
    }
    let status = inspect(&registry, installation)?;
    if matches!(status, Status::Enabled { .. }) && !config_owned(installation)? {
        return Ok(Status::Incomplete {
            reason: "Integration configuration is missing".into(),
        });
    }
    Ok(status)
}
pub fn preflight(installation: &Installation) -> io::Result<Status> {
    let status = status(installation)?;
    if let Status::Incomplete { reason } = &status {
        return Err(io::Error::other(reason.clone()));
    }
    Ok(status)
}
fn config_owned(installation: &Installation) -> io::Result<bool> {
    let path = installation.config();
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(512 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 512 * 1024
        || bytes != installation.bytes()?
        || !same_path(&plain_path(&path.canonicalize()?)?, &path)
    {
        return Err(io::Error::other(
            "Integration configuration differs or is redirected; retained for inspection",
        ));
    }
    Ok(true)
}
fn install_at(
    installation: &Installation,
    hive: windows_sys::Win32::System::Registry::HKEY,
    prefix: String,
) -> io::Result<()> {
    install_with_hook(installation, hive, prefix, || Ok(()))
}
fn install_with_hook(
    installation: &Installation,
    hive: windows_sys::Win32::System::Registry::HKEY,
    prefix: String,
    mut checkpoint: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let registry = Registry::begin(hive, prefix)?;
    journal::recover(&registry, installation)?;
    if let Status::Incomplete { reason } = inspect(&registry, installation)? {
        return Err(io::Error::other(reason));
    }
    let created = if config_owned(installation)? {
        false
    } else {
        let config = installation.config();
        let mut file = tempfile::NamedTempFile::new_in(
            config
                .parent()
                .ok_or_else(|| io::Error::other("Configuration has no directory"))?,
        )?;
        file.write_all(&installation.bytes()?)?;
        file.as_file().sync_all()?;
        file.persist_noclobber(config)
            .map_err(|error| error.error)?;
        true
    };
    let operation = (|| {
        let update =
            journal::Update::begin(&registry, installation, journal::Action::Install, !created)?;
        for (key, wanted) in expected(&installation.dll) {
            registry.create(&key)?;
            for (name, value) in wanted.values {
                registry.set(&key, &name, &value)?;
                checkpoint()?;
            }
        }
        registry.set(APPROVED, CLSID, &units(LABEL))?;
        checkpoint()?;
        if !matches!(inspect(&registry, installation)?, Status::Enabled { .. }) {
            return Err(io::Error::other("Integration readback mismatch"));
        }
        update.commit()
    })();
    if let Err(error) = operation {
        if created {
            let cleanup = config_owned(installation).and_then(|owned| {
                if owned {
                    fs::remove_file(installation.config())
                } else {
                    Ok(())
                }
            });
            if let Err(cleanup) = cleanup {
                return Err(io::Error::other(format!(
                    "Integration install failed: {error}; configuration cleanup also failed: {cleanup}"
                )));
            }
        }
        return Err(error);
    }
    Ok(())
}
pub fn install(installation: &Installation) -> io::Result<Status> {
    let _guard = crate::setup::configuration_lock()?;
    install_at(installation, HKEY_LOCAL_MACHINE, String::new())?;
    notify();
    status(installation)
}
fn remove_at(
    installation: &Installation,
    hive: windows_sys::Win32::System::Registry::HKEY,
    prefix: String,
) -> io::Result<()> {
    remove_with_hook(installation, hive, prefix, || Ok(()))
}
fn remove_with_hook(
    installation: &Installation,
    hive: windows_sys::Win32::System::Registry::HKEY,
    prefix: String,
    mut checkpoint: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let registry = Registry::begin(hive, prefix)?;
    journal::recover(&registry, installation)?;
    match inspect(&registry, installation)? {
        Status::Disabled => return Ok(()),
        Status::Enabled { .. } => {}
        Status::OtherInstallation { .. } => {
            return Err(io::Error::other(
                "Another installation owns Explorer integration",
            ));
        }
        Status::Incomplete { reason } => return Err(io::Error::other(reason)),
        Status::PendingRecovery { .. } => {
            return Err(io::Error::other("Integration recovery is required"));
        }
    }
    let owned = config_owned(installation)?;
    let update = journal::Update::begin(&registry, installation, journal::Action::Remove, owned)?;
    let mut keys = expected(&installation.dll).into_keys().collect::<Vec<_>>();
    keys.sort_by_key(|key| std::cmp::Reverse(key.matches('\\').count()));
    registry.delete_value(APPROVED, CLSID)?;
    checkpoint()?;
    for key in keys {
        registry.delete_key(&key)?;
        checkpoint()?;
    }
    if owned && config_owned(installation)? {
        fs::remove_file(installation.config())?;
        checkpoint()?;
    }
    update.commit()
}
pub fn remove(installation: &Installation) -> io::Result<Status> {
    let _guard = crate::setup::configuration_lock()?;
    remove_at(installation, HKEY_LOCAL_MACHINE, String::new())?;
    notify();
    status(installation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{HKEY_CURRENT_USER, RegDeleteTreeW},
    };
    struct Fixture {
        dir: tempfile::TempDir,
        prefix: String,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let prefix = format!(
                r"Software\VartaCopy\IntegrationTests\{}-{}",
                std::process::id(),
                dir.path().file_name().unwrap().to_string_lossy()
            );
            Self { dir, prefix }
        }
        fn installation(&self, name: &str) -> Installation {
            let root = self.dir.path().join(name);
            fs::create_dir(&root).unwrap();
            let gui = root.join("vartacopy-gui.exe");
            let dll = root.join("vartacopy_transfer.dll");
            fs::write(&gui, b"test fixture only").unwrap();
            fs::write(&dll, b"test fixture only").unwrap();
            Installation::new(&gui, &dll).unwrap()
        }
        fn registry(&self) -> Registry {
            Registry::read(HKEY_CURRENT_USER, self.prefix.clone())
        }
        fn transaction(&self) -> Registry {
            Registry::begin(HKEY_CURRENT_USER, self.prefix.clone()).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // SAFETY: Unique nonce under a private test namespace; never Classes/CLSID or the real shell hive.
            let code =
                unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, crate::wide(&self.prefix).as_ptr()) };
            if code != ERROR_SUCCESS && code != windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND
            {
                eprintln!("Test registry cleanup failed: {code}");
            }
        }
    }
    #[test]
    fn native_install_is_idempotent_migrates_owned_entries_and_preserves_other_approvals() {
        let fixture = Fixture::new();
        let first = fixture.installation("old");
        let next = fixture.installation("new");
        let tx = fixture.transaction();
        tx.set(APPROVED, "Other product", &units("Keep me"))
            .unwrap();
        tx.commit().unwrap();
        install_at(&first, HKEY_CURRENT_USER, fixture.prefix.clone()).unwrap();
        install_at(&first, HKEY_CURRENT_USER, fixture.prefix.clone()).unwrap();
        assert!(matches!(
            inspect(&fixture.registry(), &first).unwrap(),
            Status::Enabled { .. }
        ));
        assert!(matches!(
            inspect(&fixture.registry(), &next).unwrap(),
            Status::OtherInstallation { .. }
        ));
        install_at(&next, HKEY_CURRENT_USER, fixture.prefix.clone()).unwrap();
        assert!(matches!(
            inspect(&fixture.registry(), &next).unwrap(),
            Status::Enabled { .. }
        ));
        assert!(first.config().exists()); // An older installation's file is never deleted during migration.
        assert!(remove_at(&first, HKEY_CURRENT_USER, fixture.prefix.clone()).is_err());
        remove_at(&next, HKEY_CURRENT_USER, fixture.prefix.clone()).unwrap();
        remove_at(&next, HKEY_CURRENT_USER, fixture.prefix.clone()).unwrap();
        assert!(!next.config().exists());
        assert!(matches!(
            inspect(&fixture.registry(), &next).unwrap(),
            Status::Disabled
        ));
        assert_eq!(
            fixture.registry().value(APPROVED, "Other product").unwrap(),
            Some(units("Keep me"))
        );
    }
    #[test]
    fn injected_registry_failure_rolls_back_every_key_and_removes_only_new_configuration() {
        for fail_at in [1, 5, 9] {
            let fixture = Fixture::new();
            let installation = fixture.installation("app");
            let mut writes = 0;
            let result = install_with_hook(
                &installation,
                HKEY_CURRENT_USER,
                fixture.prefix.clone(),
                || {
                    writes += 1;
                    if writes == fail_at {
                        Err(io::Error::other("Injected registry failure"))
                    } else {
                        Ok(())
                    }
                },
            );
            assert!(result.is_err());
            assert!(matches!(
                inspect(&fixture.registry(), &installation).unwrap(),
                Status::Disabled
            ));
            assert!(!installation.config().exists());
        }
    }
    #[test]
    fn changed_keys_or_foreign_children_block_install_and_removal_without_deleting_data() {
        let fixture = Fixture::new();
        let installation = fixture.installation("app");
        install_at(&installation, HKEY_CURRENT_USER, fixture.prefix.clone()).unwrap();
        let tx = fixture.transaction();
        tx.set(&format!(r"{}\Foreign", base()), "Other", &units("Keep"))
            .unwrap();
        tx.commit().unwrap();
        assert!(matches!(
            inspect(&fixture.registry(), &installation).unwrap(),
            Status::Incomplete { .. }
        ));
        assert!(install_at(&installation, HKEY_CURRENT_USER, fixture.prefix.clone()).is_err());
        assert!(remove_at(&installation, HKEY_CURRENT_USER, fixture.prefix.clone()).is_err());
        assert_eq!(
            fixture
                .registry()
                .value(&format!(r"{}\Foreign", base()), "Other")
                .unwrap(),
            Some(units("Keep"))
        );
        assert!(installation.config().exists());
    }
    #[test]
    fn existing_configuration_is_preserved_on_failure_and_foreign_settings_are_refused() {
        let fixture = Fixture::new();
        let installation = fixture.installation("app");
        let bytes = installation.bytes().unwrap();
        fs::write(installation.config(), &bytes).unwrap();
        assert!(
            install_with_hook(
                &installation,
                HKEY_CURRENT_USER,
                fixture.prefix.clone(),
                || Err(io::Error::other("Injected"))
            )
            .is_err()
        );
        assert_eq!(fs::read(installation.config()).unwrap(), bytes);
        fs::write(installation.config(), b"foreign settings").unwrap();
        assert!(install_at(&installation, HKEY_CURRENT_USER, fixture.prefix.clone()).is_err());
        assert_eq!(
            fs::read(installation.config()).unwrap(),
            b"foreign settings"
        );
        assert!(matches!(
            inspect(&fixture.registry(), &installation).unwrap(),
            Status::Disabled
        ));
    }
    #[test]
    fn installed_locator_selects_the_versioned_dll_and_rejects_escape_paths() {
        let fixture = Fixture::new();
        let installation = fixture.installation("app");
        let root = installation.gui.parent().unwrap();
        let version = root.join("shell").join("beta1-build");
        fs::create_dir_all(&version).unwrap();
        let dll = version.join("vartacopy_transfer.dll");
        fs::write(&dll, b"test only").unwrap();
        let locator = root.join("integration-install.json");
        fs::write(
            &locator,
            r#"{"version":1,"dll":"shell/beta1-build/vartacopy_transfer.dll"}"#,
        )
        .unwrap();
        assert_eq!(Installation::beside(&installation.gui).unwrap().dll, dll);
        for path in [
            "../vartacopy_transfer.dll",
            "D:/other/vartacopy_transfer.dll",
            "",
        ] {
            fs::write(
                &locator,
                serde_json::json!({"version":1,"dll":path}).to_string(),
            )
            .unwrap();
            assert!(Installation::beside(&installation.gui).is_err());
        }
    }
    #[test]
    fn short_names_are_expanded_but_junctions_are_still_refused() {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
        let fixture = Fixture::new();
        let installation = fixture.installation("long directory name");
        let mut short = vec![0; 32768];
        // SAFETY: Existing path and bounded UTF-16 output. Volumes without 8.3
        // names may return the original spelling, which must also remain valid.
        let count = unsafe {
            GetShortPathNameW(
                crate::wide(&installation.gui).as_ptr(),
                short.as_mut_ptr(),
                short.len() as u32,
            )
        } as usize;
        assert!(count > 0 && count < short.len());
        assert_eq!(
            Installation::new(&registry::native(&short[..count]), &installation.dll)
                .unwrap()
                .gui,
            installation.gui
        );
        let alias = fixture.dir.path().join("junction");
        let created = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&alias)
            .arg(installation.gui.parent().unwrap())
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
            .output()
            .unwrap();
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
        let refused =
            Installation::new(&alias.join("vartacopy-gui.exe"), &installation.dll).is_err();
        fs::remove_dir(&alias).unwrap(); // Unlink only the junction itself, never recurse through it.
        assert!(refused);
    }
    // Executed normally as a no-op; parent tests launch this exact test in a new
    // process, then exit without unwinding after a journaled mutation checkpoint.
    #[test]
    fn interrupted_process_probe() {
        let Some(payload) = std::env::var_os("VARTACOPY_INTEGRATION_CRASH_PROBE") else {
            return;
        };
        #[derive(Deserialize)]
        struct Probe {
            gui: PathBuf,
            dll: PathBuf,
            prefix: String,
            remove: bool,
            at: usize,
        }
        let probe: Probe = serde_json::from_str(&payload.to_string_lossy()).unwrap();
        assert!(
            probe
                .prefix
                .starts_with(r"Software\VartaCopy\IntegrationTests\")
        );
        let installation = Installation::new(&probe.gui, &probe.dll).unwrap();
        let mut count = 0;
        let hook = || {
            count += 1;
            if count == probe.at {
                std::process::exit(73);
            }
            Ok(())
        };
        let result = if probe.remove {
            remove_with_hook(&installation, HKEY_CURRENT_USER, probe.prefix, hook)
        } else {
            install_with_hook(&installation, HKEY_CURRENT_USER, probe.prefix, hook)
        };
        panic!("Crash checkpoint was not reached: {result:?}");
    }
    fn interrupt(fixture: &Fixture, installation: &Installation, remove: bool, at: usize) {
        use std::os::windows::process::CommandExt;
        let payload=serde_json::json!({"gui":installation.gui,"dll":installation.dll,"prefix":fixture.prefix,"remove":remove,"at":at}).to_string();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "transfer_registration::tests::interrupted_process_probe",
                "--nocapture",
            ])
            .env("VARTACOPY_INTEGRATION_CRASH_PROBE", payload)
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
            .output()
            .unwrap();
        assert_eq!(
            child.status.code(),
            Some(73),
            "{} {}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(journal::pending(&fixture.registry(), installation).unwrap());
    }
    #[test]
    fn interrupted_install_and_upgrade_restore_the_verified_prior_state() {
        for upgrade in [false, true] {
            let fixture = Fixture::new();
            let prior = fixture.installation("old");
            let next = fixture.installation("next");
            if upgrade {
                install_at(&prior, HKEY_CURRENT_USER, fixture.prefix.clone()).unwrap();
            }
            interrupt(&fixture, &next, false, 4);
            journal::recover(&fixture.transaction(), &next).unwrap();
            assert!(!journal::location(&next).exists());
            assert!(!next.config().exists());
            if upgrade {
                assert!(matches!(
                    inspect(&fixture.registry(), &prior).unwrap(),
                    Status::Enabled { .. }
                ));
                assert!(prior.config().exists());
            } else {
                assert!(matches!(
                    inspect(&fixture.registry(), &next).unwrap(),
                    Status::Disabled
                ));
            }
        }
    }
    #[test]
    fn interrupted_uninstall_restores_keys_and_configuration_then_retries_safely() {
        for at in [3, 11] {
            let fixture = Fixture::new();
            let installation = fixture.installation("app");
            install_at(&installation, HKEY_CURRENT_USER, fixture.prefix.clone()).unwrap();
            interrupt(&fixture, &installation, true, at);
            journal::recover(&fixture.transaction(), &installation).unwrap();
            assert!(matches!(
                inspect(&fixture.registry(), &installation).unwrap(),
                Status::Enabled { .. }
            ));
            assert!(config_owned(&installation).unwrap());
            remove_at(&installation, HKEY_CURRENT_USER, fixture.prefix.clone()).unwrap();
            assert!(matches!(
                inspect(&fixture.registry(), &installation).unwrap(),
                Status::Disabled
            ));
        }
    }
    #[test]
    fn recovery_refuses_unexpected_changes_and_keeps_its_evidence() {
        let fixture = Fixture::new();
        let installation = fixture.installation("app");
        interrupt(&fixture, &installation, false, 4);
        let registry = fixture.transaction();
        registry.set(&base(), "Foreign", &units("Keep me")).unwrap();
        registry.commit().unwrap();
        assert!(journal::recover(&fixture.transaction(), &installation).is_err());
        assert_eq!(
            fixture.registry().value(&base(), "Foreign").unwrap(),
            Some(units("Keep me"))
        );
        assert!(journal::location(&installation).exists());
    }
}
