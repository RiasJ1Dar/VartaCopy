//! Windows-only verified copy engine, bounded planning, and directory traversal.
//! ACL/owner are inherited at the destination. Move and durable resume are not implemented.
pub mod planner;
pub mod tree;
use serde::Serialize;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use vartacopy_platform_windows as platform;

const BUFFER_SIZE: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("operation cancelled")]
    Cancelled,
    #[error("verification failed: {0}")]
    Verification(String),
    #[error("scan limit exceeded: {resource} (limit {limit})")]
    ScanLimit {
        resource: &'static str,
        limit: usize,
    },
    #[error("operation failed: {primary}; staging cleanup also failed: {cleanup}")]
    Cleanup {
        primary: Box<Error>,
        cleanup: io::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy)]
pub enum Conflict {
    Error,
    Skip,
    KeepBoth,
}

#[derive(Debug, Serialize)]
pub struct Plan {
    pub source: platform::PathRecord,
    pub destination: platform::PathRecord,
    pub bytes: u64,
    pub streams: usize,
    pub metadata_policy: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Outcome {
    pub status: &'static str,
    pub destination: platform::PathRecord,
    pub bytes: u64,
    pub streams_verified: usize,
    pub metadata_policy: &'static str,
}

const METADATA_POLICY: &str = "data + ADS + basic file times/flags; destination inherits ACL/owner";

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

fn validate_path(path: &Path) -> Result<()> {
    for component in path.components() {
        if let Component::Normal(name) = component {
            use std::os::windows::ffi::OsStrExt;
            if name.encode_wide().any(|c| c == b':' as u16) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "explicit stream paths are refused; pass the base file",
                )
                .into());
            }
        }
    }
    Ok(())
}

fn paths(source: &Path, destination_dir: &Path) -> Result<(PathBuf, PathBuf, Vec<File>)> {
    validate_path(source)?;
    validate_path(destination_dir)?;
    let source = std::path::absolute(source)?;
    let destination_dir = std::path::absolute(destination_dir)?;
    let mut guards = platform::guard_parents(&source)?;
    // Include destination itself among protected parents.
    guards.extend(platform::guard_parents(
        &destination_dir.join(".vartacopy-parent-guard"),
    )?);
    // Keep the final component locked before resolving its spelling; refuse reparse points.
    guards.push(source_file(&source)?);
    let source = source.canonicalize()?;
    let destination_dir = destination_dir.canonicalize()?;
    if !destination_dir.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "destination must be an existing directory",
        )
        .into());
    }
    let name = source
        .file_name()
        .ok_or_else(|| io::Error::other("source has no file name"))?;
    let destination = destination_dir.join(name);
    Ok((source, destination, guards))
}

fn source_file(path: &Path) -> Result<File> {
    let file = platform::open_source(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this initial build supports regular files only",
        )
        .into());
    }
    use std::os::windows::fs::MetadataExt;
    // EFS would otherwise silently decrypt into a destination without encryption.
    if metadata.file_attributes() & 0x4000 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "EFS files require a dedicated metadata policy",
        )
        .into());
    }
    Ok(file)
}

pub fn plan(source: &Path, destination_dir: &Path) -> Result<Plan> {
    let (source, destination, _guards) = paths(source, destination_dir)?;
    let _file = source_file(&source)?;
    let streams = platform::streams(&source)?;
    Ok(Plan {
        source: source.as_path().into(),
        destination: destination.as_path().into(),
        bytes: streams.iter().map(|s| s.size).sum(),
        streams: streams.len(),
        metadata_policy: METADATA_POLICY,
    })
}

fn hash(file: &mut File, cancel: &AtomicBool) -> Result<(blake3::Hash, u64)> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; BUFFER_SIZE];
    let mut bytes = 0;
    loop {
        check_cancel(cancel)?;
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        bytes += n as u64;
    }
    Ok((hasher.finalize(), bytes))
}

fn transfer(
    input: &mut File,
    output: &mut File,
    cancel: &AtomicBool,
) -> Result<(blake3::Hash, u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; BUFFER_SIZE];
    let mut bytes = 0;
    loop {
        check_cancel(cancel)?;
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        output.write_all(&buffer[..n])?;
        hasher.update(&buffer[..n]);
        bytes += n as u64;
    }
    output.sync_all()?;
    Ok((hasher.finalize(), bytes))
}

fn alternative(destination: &Path, number: u32) -> PathBuf {
    let mut name = destination
        .file_stem()
        .unwrap_or(OsStr::new("file"))
        .to_os_string();
    name.push(format!(" ({number})"));
    if let Some(extension) = destination.extension() {
        name.push(".");
        name.push(extension);
    }
    destination.with_file_name(name)
}

pub fn copy(
    source: &Path,
    destination_dir: &Path,
    conflict: Conflict,
    cancel: &AtomicBool,
) -> Result<Outcome> {
    copy_with_hook(source, destination_dir, conflict, cancel, |_| Ok(()))
}

fn copy_with_hook(
    source: &Path,
    destination_dir: &Path,
    conflict: Conflict,
    cancel: &AtomicBool,
    before_verify: impl FnOnce(&mut platform::Stage) -> io::Result<()>,
) -> Result<Outcome> {
    check_cancel(cancel)?;
    let (source, destination, _guards) = paths(source, destination_dir)?;
    let mut input = source_file(&source)?;
    let stamp = input.metadata()?;
    let manifest = platform::streams(&source)?;
    let source_identity = platform::identity(&input)?;
    // Open every named source stream before writing anything.
    let mut named_inputs = Vec::new();
    for stream in &manifest {
        if !stream.name.is_empty() {
            named_inputs.push((
                stream,
                platform::open_source(&platform::stream_path(&source, &stream.name))?,
            ));
        }
    }
    let mut stage = platform::Stage::create(
        destination
            .parent()
            .ok_or_else(|| io::Error::other("no destination parent"))?,
    )?;
    let result = (|| {
        let primary = transfer(&mut input, stage.file(), cancel)?;
        let mut outputs = Vec::new();
        for (stream, input) in &mut named_inputs {
            let mut output = stage.create_stream(&stream.name)?;
            let expected = transfer(input, &mut output, cancel)?;
            if expected.1 != stream.size {
                return Err(Error::Verification("source stream size changed".into()));
            }
            outputs.push((output, expected));
        }
        before_verify(&mut stage)?;
        if primary != hash(stage.file(), cancel)? {
            return Err(Error::Verification(
                "unnamed data stream differs after readback".into(),
            ));
        }
        for (output, expected) in &mut outputs {
            if *expected != hash(output, cancel)? {
                return Err(Error::Verification(
                    "named data stream differs after readback".into(),
                ));
            }
        }
        drop(outputs);
        let after = input.metadata()?;
        if primary.1 != stamp.len()
            || after.len() != stamp.len()
            || after.modified()? != stamp.modified()?
            || source_identity != platform::identity(&input)?
            || platform::streams(&source)? != manifest
        {
            return Err(Error::Verification("source changed during transfer".into()));
        }
        platform::copy_basic_metadata(&input, stage.file())?;
        check_cancel(cancel)?;
        let mut target = destination.clone();
        let mut number = 0;
        loop {
            check_cancel(cancel)?;
            match stage.publish(&target) {
                Ok(()) => break,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => match conflict {
                    Conflict::Error => return Err(error.into()),
                    Conflict::Skip => {
                        stage.discard()?;
                        return Ok(Outcome {
                            status: "skipped_unverified",
                            destination: target.as_path().into(),
                            bytes: 0,
                            streams_verified: 0,
                            metadata_policy: METADATA_POLICY,
                        });
                    }
                    Conflict::KeepBoth => {
                        number += 1;
                        if number > 10_000 {
                            return Err(
                                io::Error::other("keep-both candidate limit reached").into()
                            );
                        }
                        target = alternative(&destination, number);
                    }
                },
                Err(error) => return Err(error.into()),
            }
        }
        Ok(Outcome {
            status: "copied_verified",
            destination: target.as_path().into(),
            bytes: manifest.iter().map(|s| s.size).sum(),
            streams_verified: manifest.len(),
            metadata_policy: METADATA_POLICY,
        })
    })();
    match result {
        Err(primary) => match stage.discard() {
            Ok(()) => Err(primary),
            Err(cleanup) => Err(Error::Cleanup {
                primary: Box::new(primary),
                cleanup,
            }),
        },
        ok => ok,
    }
}

pub fn verify(left: &Path, right: &Path, cancel: &AtomicBool) -> Result<usize> {
    validate_path(left)?;
    validate_path(right)?;
    let left = std::path::absolute(left)?;
    let right = std::path::absolute(right)?;
    let _left_guards = platform::guard_parents(&left)?;
    let _right_guards = platform::guard_parents(&right)?;
    let mut a = source_file(&left)?;
    let mut b = source_file(&right)?;
    let manifest = platform::streams(&left)?;
    if manifest != platform::streams(&right)? {
        return Err(Error::Verification("stream names or lengths differ".into()));
    }
    if hash(&mut a, cancel)? != hash(&mut b, cancel)? {
        return Err(Error::Verification("unnamed data stream differs".into()));
    }
    for stream in &manifest {
        if stream.name.is_empty() {
            continue;
        }
        let mut a = platform::open_source(&platform::stream_path(&left, &stream.name))?;
        let mut b = platform::open_source(&platform::stream_path(&right, &stream.name))?;
        if hash(&mut a, cancel)? != hash(&mut b, cancel)? {
            return Err(Error::Verification("named data stream differs".into()));
        }
    }
    if manifest != platform::streams(&left)? || manifest != platform::streams(&right)? {
        return Err(Error::Verification(
            "stream manifest changed during verification".into(),
        ));
    }
    Ok(manifest.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::time::{Duration, SystemTime};

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().expect("fixture");
        let source = root.path().join("джерело.bin");
        let destination = root.path().join("ціль");
        fs::create_dir(&destination).expect("destination");
        fs::write(&source, b"hello").expect("source");
        (root, source, destination)
    }

    fn cancelled() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn assert_no_stage(directory: &Path) {
        for entry in fs::read_dir(directory).unwrap() {
            assert!(
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".part")
            );
        }
    }

    #[test]
    fn unicode_multibuffer_and_empty_copy() {
        let (_root, source, dest) = fixture();
        let data: Vec<u8> = (0..BUFFER_SIZE * 2 + 17).map(|n| (n % 251) as u8).collect();
        fs::write(&source, &data).unwrap();
        let result = copy(&source, &dest, Conflict::Error, &cancelled()).unwrap();
        assert_eq!(result.status, "copied_verified");
        assert_eq!(
            fs::read(dest.join(source.file_name().unwrap())).unwrap(),
            data
        );
        fs::write(&source, []).unwrap();
        copy(&source, &dest, Conflict::KeepBoth, &cancelled()).unwrap();
        assert_eq!(fs::metadata(dest.join("джерело (1).bin")).unwrap().len(), 0);
        assert_no_stage(&dest);
    }

    #[test]
    fn copies_and_verifies_named_streams() {
        let (_root, source, dest) = fixture();
        let stream = platform::stream_path(&source, OsStr::new(":коментар:$DATA"));
        fs::write(&stream, b"alternate contents").unwrap();
        let result = copy(&source, &dest, Conflict::Error, &cancelled()).unwrap();
        let output = dest.join(source.file_name().unwrap());
        assert_eq!(result.streams_verified, 2);
        assert_eq!(
            fs::read(platform::stream_path(
                &output,
                OsStr::new(":коментар:$DATA")
            ))
            .unwrap(),
            b"alternate contents"
        );
        assert_eq!(verify(&source, &output, &cancelled()).unwrap(), 2);
        fs::write(
            platform::stream_path(&output, OsStr::new(":коментар:$DATA")),
            b"different contents",
        )
        .unwrap();
        assert!(verify(&source, &output, &cancelled()).is_err());
    }

    #[test]
    fn conflicts_never_replace_and_skip_is_unverified() {
        let (_root, source, dest) = fixture();
        let target = dest.join(source.file_name().unwrap());
        fs::write(&target, b"existing").unwrap();
        assert!(copy(&source, &dest, Conflict::Error, &cancelled()).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"existing");
        let result = copy(&source, &dest, Conflict::Skip, &cancelled()).unwrap();
        assert_eq!(result.status, "skipped_unverified");
        assert_eq!(result.streams_verified, 0);
        fs::write(dest.join("джерело (1).bin"), b"also existing").unwrap();
        copy(&source, &dest, Conflict::KeepBoth, &cancelled()).unwrap();
        assert_eq!(
            fs::read(dest.join("джерело (1).bin")).unwrap(),
            b"also existing"
        );
        assert_eq!(fs::read(dest.join("джерело (2).bin")).unwrap(), b"hello");
        assert_no_stage(&dest);
    }

    #[test]
    fn collision_created_after_copy_does_not_replace() {
        let (_root, source, dest) = fixture();
        let target = dest.join(source.file_name().unwrap());
        let result = copy_with_hook(&source, &dest, Conflict::Error, &cancelled(), |_| {
            fs::write(&target, b"racing writer")
        });
        assert!(result.is_err());
        assert_eq!(fs::read(target).unwrap(), b"racing writer");
        assert_no_stage(&dest);
    }

    #[test]
    fn corrupted_readback_is_never_published() {
        let (_root, source, dest) = fixture();
        let result = copy_with_hook(&source, &dest, Conflict::Error, &cancelled(), |stage| {
            stage.file().seek(SeekFrom::Start(0))?;
            stage.file().write_all(b"WRONG")
        });
        assert!(matches!(result, Err(Error::Verification(_))), "{result:?}");
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
    }

    #[test]
    fn cancellation_cleans_unpublished_data() {
        let (_root, source, dest) = fixture();
        let cancel = cancelled();
        let result = copy_with_hook(&source, &dest, Conflict::Error, &cancel, |_| {
            cancel.store(true, Ordering::Relaxed);
            Ok(())
        });
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
    }

    #[test]
    fn same_size_and_mtime_still_require_matching_bytes() {
        let (_root, source, dest) = fixture();
        let target = dest.join("other");
        fs::write(&target, b"wrong").unwrap();
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        for path in [&source, &target] {
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(time)
                .unwrap();
        }
        assert_eq!(
            fs::metadata(&source).unwrap().len(),
            fs::metadata(&target).unwrap().len()
        );
        assert_eq!(
            fs::metadata(&source).unwrap().modified().unwrap(),
            fs::metadata(&target).unwrap().modified().unwrap()
        );
        assert!(matches!(
            verify(&source, &target, &cancelled()),
            Err(Error::Verification(_))
        ));
    }

    #[test]
    fn dry_run_writes_nothing_and_rejects_explicit_ads() {
        let (_root, source, dest) = fixture();
        assert_eq!(plan(&source, &dest).unwrap().bytes, 5);
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
        assert!(plan(&platform::stream_path(&source, OsStr::new(":ads")), &dest).is_err());
    }

    #[test]
    fn source_cannot_be_its_own_destination() {
        let (root, source, _dest) = fixture();
        assert!(copy(&source, root.path(), Conflict::Error, &cancelled()).is_err());
        assert_eq!(fs::read(source).unwrap(), b"hello");
        assert_no_stage(root.path());
    }

    #[test]
    #[allow(
        clippy::permissions_set_readonly_false,
        reason = "Windows-only fixture: clear the NTFS readonly attribute"
    )]
    fn basic_timestamp_and_readonly_cleanup() {
        let (_root, source, dest) = fixture();
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_modified(time)
            .unwrap();
        let mut permissions = fs::metadata(&source).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&source, permissions).unwrap();
        let result = copy(&source, &dest, Conflict::Error, &cancelled()).unwrap();
        let target = PathBuf::from(result.destination.display);
        assert_eq!(fs::metadata(&target).unwrap().modified().unwrap(), time);
        assert!(fs::metadata(&target).unwrap().permissions().readonly());
        assert!(copy(&source, &dest, Conflict::Error, &cancelled()).is_err());
        assert_no_stage(&dest);
        // Windows permits clearing readonly; this is an isolated test fixture.
        for path in [&source, &target] {
            let mut p = fs::metadata(path).unwrap().permissions();
            p.set_readonly(false);
            fs::set_permissions(path, p).unwrap();
        }
    }

    #[test]
    fn source_is_locked_against_writes_and_rename() {
        let (_root, source, dest) = fixture();
        copy_with_hook(&source, &dest, Conflict::Error, &cancelled(), |_| {
            assert!(OpenOptions::new().write(true).open(&source).is_err());
            assert!(fs::rename(&source, source.with_extension("moved")).is_err());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn sqlite_spike_rolls_back_uncommitted_work_after_reopen() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("spike.sqlite");
        {
            let mut db = rusqlite::Connection::open(&path).unwrap();
            db.pragma_update(None, "journal_mode", "WAL").unwrap();
            db.pragma_update(None, "synchronous", "FULL").unwrap();
            db.execute_batch(
                "CREATE TABLE jobs(id INTEGER PRIMARY KEY, state TEXT NOT NULL);
                INSERT INTO jobs VALUES (1, 'planned');",
            )
            .unwrap();
            let transaction = db.transaction().unwrap();
            transaction
                .execute("UPDATE jobs SET state = 'published' WHERE id = 1", [])
                .unwrap();
            transaction.rollback().unwrap();
        }
        let db = rusqlite::Connection::open(&path).unwrap();
        let state: String = db
            .query_row("SELECT state FROM jobs WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "planned");
        // This checks SQLite rollback only, not coordinated filesystem crash recovery.
    }
}
