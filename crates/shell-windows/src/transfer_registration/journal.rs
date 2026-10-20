//! Write-ahead record for only our fixed registry keys. No dependency on Windows TxR.
//! A process interruption is recoverable; unexpected third-party edits stop recovery.
use super::*;
const LIMIT: u64 = 2 * 1024 * 1024;
#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum Action {
    Install,
    Remove,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    scope: String,
    namespace: String,
    version: u32,
    gui: Vec<u16>,
    dll: Vec<u16>,
    action: Action,
    before: BTreeMap<String, Option<Snapshot>>,
    approval: Option<Vec<u16>>,
    config_existed: bool,
}
pub fn location(installation: &Installation) -> PathBuf {
    installation
        .gui
        .with_file_name("explorer-integration-update.json")
}
fn read_bytes(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err(io::Error::other("Integration recovery record is oversized"));
    }
    if !same_path(&long_spelling(path)?, &plain_path(&path.canonicalize()?)?) {
        return Err(io::Error::other(
            "Integration recovery record is redirected",
        ));
    }
    Ok(Some(bytes))
}
fn create_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(
        path.parent()
            .ok_or_else(|| io::Error::other("Recovery path has no parent"))?,
    )?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|error| error.error)?;
    Ok(())
}
impl Record {
    fn installation(&self, current: &Installation) -> io::Result<Installation> {
        let gui = plain_path(&registry::native(&self.gui))?;
        let dll = plain_path(&registry::native(&self.dll))?;
        let parent = current
            .gui
            .parent()
            .ok_or_else(|| io::Error::other("GUI has no directory"))?;
        if self.version != 1
            || !same_path(&gui, &current.gui)
            || !dll.starts_with(parent)
            || !dll
                .file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case("vartacopy_transfer.dll"))
        {
            return Err(io::Error::other(
                "Recovery record belongs to another installation",
            ));
        }
        Ok(Installation { gui, dll })
    }
    fn after(&self, installation: &Installation) -> BTreeMap<String, Option<Snapshot>> {
        expected(&installation.dll)
            .into_iter()
            .map(|(key, value)| {
                (
                    key,
                    match self.action {
                        Action::Install => Some(value),
                        Action::Remove => None,
                    },
                )
            })
            .collect()
    }
    fn approval_after(&self) -> Option<Vec<u16>> {
        match self.action {
            Action::Install => Some(units(LABEL)),
            Action::Remove => None,
        }
    }
    fn validate(&self, installation: &Installation) -> io::Result<()> {
        let keys = expected(&installation.dll);
        if !keys.keys().eq(self.before.keys()) {
            return Err(io::Error::other(
                "Unexpected registry scope in recovery record",
            ));
        }
        if self.before.values().all(Option::is_none) && self.approval.is_none() {
            return Ok(());
        }
        let prior = self
            .before
            .get(&format!(r"{}\InprocServer32", base()))
            .and_then(Option::as_ref)
            .and_then(|s| s.values.get(""))
            .ok_or_else(|| io::Error::other("Recovery original state is incomplete"))?;
        let prior = plain_path(&registry::native(prior))?;
        if !prior
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("vartacopy_transfer.dll"))
            || self.approval != Some(units(LABEL))
        {
            return Err(io::Error::other("Recovery original state is not owned"));
        }
        for (key, value) in expected(&prior) {
            if self.before.get(&key).and_then(Option::as_ref) != Some(&value) {
                return Err(io::Error::other(
                    "Recovery original state contains unknown values",
                ));
            }
        }
        Ok(())
    }
    fn compatible(&self, registry: &Registry, installation: &Installation) -> io::Result<()> {
        let (scope, namespace) = registry.identity();
        if self.scope != scope || self.namespace != namespace {
            return Err(io::Error::other("Recovery record registry scope differs"));
        }
        let after = self.after(installation);
        for (key, before) in &self.before {
            let Some(current) = registry.snapshot(key)? else {
                continue;
            };
            let next = after.get(key).and_then(Option::as_ref);
            for (name, value) in &current.values {
                if before.as_ref().and_then(|s| s.values.get(name)) != Some(value)
                    && next.and_then(|s| s.values.get(name)) != Some(value)
                {
                    return Err(io::Error::other(format!(
                        "Recovery refused changed value: {key}"
                    )));
                }
            }
            for child in &current.children {
                if !before.as_ref().is_some_and(|s| s.children.contains(child))
                    && !next.is_some_and(|s| s.children.contains(child))
                {
                    return Err(io::Error::other(format!(
                        "Recovery refused foreign child: {key}"
                    )));
                }
            }
        }
        let approval = registry.value(APPROVED, CLSID)?;
        if approval != self.approval && approval != self.approval_after() {
            return Err(io::Error::other("Recovery refused changed approval"));
        }
        Ok(())
    }
    fn restore(&self, registry: &Registry, installation: &Installation) -> io::Result<()> {
        self.validate(installation)?;
        self.compatible(registry, installation)?;
        let config_present = config_owned(installation)?;
        if self.config_existed && !config_present {
            create_file(&installation.config(), &installation.bytes()?)?;
        }
        for (key, before) in &self.before {
            if let Some(before) = before {
                registry.create(key)?;
                for (name, value) in &before.values {
                    registry.set(key, name, value)?;
                }
            }
        }
        let mut absent = self
            .before
            .iter()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key)
            .collect::<Vec<_>>();
        absent.sort_by_key(|key| std::cmp::Reverse(key.matches('\\').count()));
        for key in absent {
            registry.delete_key(key)?;
        }
        if let Some(value) = &self.approval {
            registry.set(APPROVED, CLSID, value)?;
        } else {
            registry.delete_value(APPROVED, CLSID)?;
        }
        registry.flush(APPROVED)?;
        for (key, wanted) in &self.before {
            if registry.snapshot(key)? != *wanted {
                return Err(io::Error::other("Recovery readback mismatch"));
            }
        }
        if registry.value(APPROVED, CLSID)? != self.approval {
            return Err(io::Error::other("Recovery approval readback mismatch"));
        }
        if !self.config_existed && config_present && config_owned(installation)? {
            fs::remove_file(installation.config())?;
        }
        Ok(())
    }
}
pub fn pending(registry: &Registry, installation: &Installation) -> io::Result<bool> {
    let Some(bytes) = read_bytes(&location(installation))? else {
        return Ok(false);
    };
    let record: Record = serde_json::from_slice(&bytes)?;
    let installation = record.installation(installation)?;
    record.validate(&installation)?;
    record.compatible(registry, &installation)?;
    Ok(true)
}
pub fn recover(registry: &Registry, installation: &Installation) -> io::Result<()> {
    let path = location(installation);
    let Some(bytes) = read_bytes(&path)? else {
        return Ok(());
    };
    let record: Record = serde_json::from_slice(&bytes)?;
    let installation = record.installation(installation)?;
    record.restore(registry, &installation)?;
    if read_bytes(&path)?.as_ref() != Some(&bytes) {
        return Err(io::Error::other("Recovery record changed during repair"));
    }
    fs::remove_file(path)
}
pub struct Update<'a> {
    registry: &'a Registry,
    installation: &'a Installation,
    record: Record,
    bytes: Vec<u8>,
    active: bool,
}
impl<'a> Update<'a> {
    pub fn begin(
        registry: &'a Registry,
        installation: &'a Installation,
        action: Action,
        config_existed: bool,
    ) -> io::Result<Self> {
        // Recovery scope must remain within this physical installation directory.
        if !installation.dll.starts_with(
            installation
                .gui
                .parent()
                .ok_or_else(|| io::Error::other("GUI has no directory"))?,
        ) {
            return Err(io::Error::other(
                "Integration DLL must be inside the GUI installation directory",
            ));
        }
        let before = expected(&installation.dll)
            .into_keys()
            .map(|key| registry.snapshot(&key).map(|s| (key, s)))
            .collect::<io::Result<_>>()?;
        let record = Record {
            scope: registry.identity().0.into(),
            namespace: registry.identity().1.into(),
            version: 1,
            gui: units(&installation.gui),
            dll: units(&installation.dll),
            action,
            before,
            approval: registry.value(APPROVED, CLSID)?,
            config_existed,
        };
        record.validate(installation)?;
        let bytes = serde_json::to_vec(&record)?;
        create_file(&location(installation), &bytes)?;
        Ok(Self {
            registry,
            installation,
            record,
            bytes,
            active: true,
        })
    }
    fn remove_record(&self) -> io::Result<()> {
        let path = location(self.installation);
        if read_bytes(&path)?.as_ref() != Some(&self.bytes) {
            return Err(io::Error::other("Integration recovery record changed"));
        }
        fs::remove_file(path)
    }
    fn rollback(&self) -> io::Result<()> {
        self.record.restore(self.registry, self.installation)?;
        self.remove_record()
    }
    pub fn commit(mut self) -> io::Result<()> {
        let completed = (|| {
            for (key, wanted) in self.record.after(self.installation) {
                if self.registry.snapshot(&key)? != wanted {
                    return Err(io::Error::other(
                        "Integration final-state readback mismatch",
                    ));
                }
            }
            if self.registry.value(APPROVED, CLSID)? != self.record.approval_after() {
                return Err(io::Error::other("Integration final approval mismatch"));
            }
            self.registry.flush(APPROVED)?;
            self.remove_record()
        })();
        if let Err(error) = completed {
            let rollback = self.rollback();
            self.active = false;
            return Err(match rollback {
                Ok(()) => error,
                Err(rollback) => io::Error::other(format!(
                    "Integration commit failed: {error}; rollback also failed: {rollback}"
                )),
            });
        }
        self.active = false;
        Ok(())
    }
}
impl Drop for Update<'_> {
    fn drop(&mut self) {
        if self.active
            && let Err(error) = self.rollback()
        {
            eprintln!("VartaCopy integration rollback failed; recovery record retained: {error}");
        }
    }
}
