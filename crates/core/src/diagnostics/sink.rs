use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
};
use vartacopy_platform_windows as platform;
const FORMAT: &str = "vartacopy.beta-diagnostics";
pub(super) const LIMIT: u64 = 4 * 1024 * 1024;
const MAX_RECORD: usize = 512 * 1024;
pub(super) struct Sink {
    file: Option<File>,
    path: PathBuf,
    previous: PathBuf,
    header: Vec<u8>,
    bytes: u64,
    sequence: u64,
    pub failure: Option<String>,
    // Struct fields drop in declaration order: close the log before releasing
    // writer ownership, including during an immediate cross-process restart.
    _lock: File,
}
fn suffix(path: &Path, suffix: &str) -> io::Result<PathBuf> {
    let mut name = path
        .file_name()
        .ok_or_else(|| io::Error::other("Journal filename missing"))?
        .to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}
fn ordinary(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || platform::is_reparse(&metadata) || platform::link_count(file)? != 1 {
        return Err(io::Error::other(
            "Diagnostic requires an ordinary single-link file",
        ));
    }
    Ok(())
}
fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .share_mode(1)
        .custom_flags(0x00200000); // OPEN_REPARSE_POINT
    options
}
fn validate(file: &mut File, header: &[u8]) -> io::Result<()> {
    ordinary(file)?;
    if file.metadata()?.len() > LIMIT {
        return Err(io::Error::other("Diagnostic exceeds its limit"));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut line = Vec::new();
    BufReader::new(file.take((MAX_RECORD + 1) as u64)).read_until(b'\n', &mut line)?;
    if line != header {
        return Err(io::Error::other(
            "Refusing to replace an unowned diagnostic file",
        ));
    }
    Ok(())
}
impl Sink {
    pub fn open(journal: &Path) -> io::Result<(Self, PathBuf)> {
        let absolute = std::path::absolute(journal)?;
        let journal = absolute
            .parent()
            .ok_or_else(|| io::Error::other("Journal parent missing"))?
            .canonicalize()?
            .join(
                absolute
                    .file_name()
                    .ok_or_else(|| io::Error::other("Journal filename missing"))?,
            );
        let mut header = serde_json::to_vec(&json!({"format":FORMAT,"version":1,
            "journal_utf16":journal.as_os_str().encode_wide().collect::<Vec<_>>()}))?;
        header.push(b'\n');
        if header.len() > MAX_RECORD {
            return Err(io::Error::other("Diagnostic header exceeds limit"));
        }
        let lock_path = suffix(&journal, ".diagnostics.lock")?;
        let mut lock_options = options();
        lock_options.share_mode(0);
        let mut lock = match lock_options.create_new(true).open(&lock_path) {
            Ok(mut file) => {
                ordinary(&file)?;
                file.write_all(&header)?;
                file.sync_all()?;
                file
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let mut file = options().share_mode(0).open(&lock_path)?;
                validate(&mut file, &header)?;
                if file.metadata()?.len() != header.len() as u64 {
                    return Err(io::Error::other("Unowned diagnostic lock contents"));
                }
                file
            }
            Err(error) => return Err(error),
        };
        lock.seek(SeekFrom::Start(0))?;
        let path = suffix(&journal, ".diagnostics.jsonl")?;
        let previous = suffix(&journal, ".diagnostics.previous.jsonl")?;
        let mut file = match options().create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(&header)?;
                file.sync_all()?;
                file
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let mut file = options().open(&path)?;
                validate(&mut file, &header)?;
                file
            }
            Err(error) => return Err(error),
        };
        // Preserve complete JSONL records after a crash; never concatenate onto a torn line.
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let complete = bytes
            .iter()
            .rposition(|b| *b == b'\n')
            .map(|i| i + 1)
            .ok_or_else(|| io::Error::other("Missing diagnostic header"))?;
        let discarded = bytes.len() - complete;
        if discarded > 0 {
            file.set_len(complete as u64)?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::End(0))?;
        let mut sink = Self {
            _lock: lock,
            file: Some(file),
            path: path.clone(),
            previous,
            header,
            bytes: complete as u64,
            sequence: 0,
            failure: None,
        };
        if discarded > 0 {
            sink.record(
                json!({"event":"log.tail_repaired","discarded_bytes":discarded}),
                true,
            );
        }
        Ok((sink, path))
    }
    fn write(&mut self, mut record: Value, durable: bool) -> io::Result<()> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Diagnostic sequence overflow"))?;
        record["sequence"] = json!(self.sequence);
        let mut bytes = serde_json::to_vec(&record)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_RECORD {
            return Err(io::Error::other("Diagnostic record exceeds limit"));
        }
        if self.bytes + bytes.len() as u64 > LIMIT {
            match options().open(&self.previous) {
                Ok(mut previous) => {
                    validate(&mut previous, &self.header)?;
                    drop(previous);
                    fs::remove_file(&self.previous)?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            if let Some(file) = self.file.take() {
                file.sync_all()?;
                drop(file);
            }
            fs::rename(&self.path, &self.previous)?;
            let mut file = options().create_new(true).open(&self.path)?;
            file.write_all(&self.header)?;
            self.bytes = self.header.len() as u64;
            self.file = Some(file);
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("Diagnostic file unavailable"))?;
        file.write_all(&bytes)?;
        self.bytes += bytes.len() as u64;
        if durable {
            file.sync_all()?;
        }
        Ok(())
    }
    pub fn record(&mut self, record: Value, durable: bool) {
        if self.failure.is_some() {
            return;
        }
        if let Err(error) = self.write(record, durable) {
            let message = format!("{}: {error}", self.path.display());
            eprintln!("VartaCopy diagnostic logging failed: {message}");
            self.failure = Some(message);
        }
    }
}
