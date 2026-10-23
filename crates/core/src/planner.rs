//! A bounded, non-following scanner. Plans select entries; they are not filesystem snapshots.
use crate::{
    Error, Result, check_cancel,
    diagnostics::{self, io as dio},
    source_file, validate_path,
};
use serde::{Deserialize, Serialize, Serializer};
use std::collections::HashMap;
use std::fs::{self, File, ReadDir};
use std::io;
use std::mem::size_of;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use vartacopy_platform_windows as platform;

pub const MAX_ENTRIES: usize = 100_000;
pub const MAX_DEPTH: usize = 256;
pub const MAX_PLAN_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReparsePolicy {
    #[default]
    Reject,
    Skip,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ScanLimits {
    pub max_entries: usize,
    pub max_depth: usize,
    /// Conservative retained-allocation accounting, not a process RSS guarantee.
    pub max_plan_bytes: usize,
}
impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_entries: MAX_ENTRIES,
            max_depth: 128,
            max_plan_bytes: MAX_PLAN_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ScanOptions {
    pub reparse: ReparsePolicy,
    pub limits: ScanLimits,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Directory,
    Reparse,
    Unsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub identity: platform::Identity,
    modified: u64,
    length: u64,
    attributes: u32,
    pub(crate) streams: Vec<platform::Stream>,
}
impl Snapshot {
    pub(crate) fn validate_cleanup_directory(&self, file: &File, path: &Path) -> Result<()> {
        // Removing our own children changes LastWriteTime. Identity, attributes
        // and ADS must remain unchanged; Windows enforces empty-only deletion.
        if self.identity != platform::source_identity(file)?
            || self.attributes != file.metadata()?.file_attributes()
            || self.streams != platform::streams(path)?
        {
            return Err(Error::Verification(
                "source directory changed during cleanup".into(),
            ));
        }
        Ok(())
    }
    pub(crate) fn validate(&self, file: &File, path: &Path, directory: bool) -> Result<()> {
        let metadata = file.metadata()?;
        if self.identity != platform::source_identity(file)?
            || self.modified != metadata.last_write_time()
            || (!directory && self.length != metadata.len())
            || self.attributes != metadata.file_attributes()
            || self.streams != platform::streams(path)?
        {
            return Err(Error::Verification(format!(
                "source changed since scan: {}",
                path.display()
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct PlannedEntry {
    #[serde(serialize_with = "serialize_path")]
    pub(crate) relative: PathBuf,
    pub(crate) depth: usize,
    pub(crate) kind: EntryKind,
    pub(crate) bytes: u64,
    pub(crate) streams: usize,
    /// Index of an earlier alias; data will be materialized, not linked.
    pub(crate) hardlink_of: Option<usize>,
    pub(crate) problem: Option<String>,
    #[serde(skip)]
    pub(crate) snapshot: Option<Snapshot>,
}
impl PlannedEntry {
    pub fn relative_path(&self) -> &Path {
        &self.relative
    }
    pub fn kind(&self) -> EntryKind {
        self.kind
    }
    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }
}

#[derive(Debug, Default, Serialize)]
pub struct ScanTotals {
    pub files: usize,
    pub directories: usize,
    pub reparse_points: usize,
    pub unsupported: usize,
    pub hardlink_copies: usize,
    pub bytes: u64,
    pub streams: usize,
}

#[derive(Debug, Serialize)]
pub struct OperationPlan {
    pub(crate) status: &'static str,
    #[serde(serialize_with = "serialize_path")]
    pub(crate) source: PathBuf,
    #[serde(serialize_with = "serialize_path")]
    pub(crate) destination: PathBuf,
    pub totals: ScanTotals,
    pub limits: ScanLimits,
    pub reparse_policy: ReparsePolicy,
    pub hardlink_policy: &'static str,
    pub metadata_policy: &'static str,
    pub accounted_plan_bytes: usize,
    pub(crate) entries: Vec<PlannedEntry>,
    #[serde(skip)]
    pub(crate) destination_parent_identity: platform::Identity,
}
impl OperationPlan {
    pub fn entries(&self) -> &[PlannedEntry] {
        &self.entries
    }
    pub fn is_ready(&self) -> bool {
        self.status == "ready"
    }
    pub(crate) fn source_path(&self, entry: &PlannedEntry) -> PathBuf {
        if entry.relative.as_os_str().is_empty() {
            self.source.clone()
        } else {
            self.source.join(&entry.relative)
        }
    }
}

pub(crate) fn serialize_path<S: Serializer>(
    path: &Path,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    platform::PathRecord::from(path).serialize(serializer)
}

struct Budget {
    used: usize,
    limit: usize,
}
impl Budget {
    fn charge(&mut self, bytes: usize) -> Result<()> {
        self.used = self.used.checked_add(bytes).ok_or(Error::ScanLimit {
            resource: "plan allocation bytes",
            limit: self.limit,
        })?;
        if self.used > self.limit {
            return Err(Error::ScanLimit {
                resource: "plan allocation bytes",
                limit: self.limit,
            });
        }
        Ok(())
    }
}

struct Frame {
    _guard: File,
    relative: PathBuf,
    reader: ReadDir,
    entry: usize,
}

fn inspect(
    source: &Path,
    relative: PathBuf,
    depth: usize,
) -> Result<(PlannedEntry, Option<(File, ReadDir)>)> {
    let path = if relative.as_os_str().is_empty() {
        source.to_path_buf()
    } else {
        source.join(&relative)
    };
    let mut entry = PlannedEntry {
        relative,
        depth,
        kind: EntryKind::Unsupported,
        bytes: 0,
        streams: 0,
        hardlink_of: None,
        problem: None,
        snapshot: None,
    };
    let initial = dio("symlink_metadata", &path, fs::symlink_metadata(&path))?;
    if platform::is_reparse(&initial) {
        entry.kind = EntryKind::Reparse;
        entry.problem = Some("reparse point was not followed".into());
        return Ok((entry, None));
    }
    let directory = initial.is_dir();
    let file = if directory {
        dio(
            "open_directory",
            &path,
            platform::open_directory(&path, false),
        )?
    } else {
        source_file(&path).inspect_err(|error| {
            diagnostics::failure(
                "scan.open_source",
                error,
                serde_json::json!({"path":diagnostics::path(&path)}),
            )
        })?
    };
    let metadata = dio("metadata", &path, file.metadata())?;
    if metadata.file_attributes() & 0x4000 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "EFS requires a dedicated policy",
        )
        .into());
    }
    let streams = dio(
        "FindFirstStreamW/FindNextStreamW",
        &path,
        platform::streams(&path),
    )?;
    if directory && !streams.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "directory ADS are not supported yet; refusing to omit them",
        )
        .into());
    }
    entry.kind = if directory {
        EntryKind::Directory
    } else {
        EntryKind::File
    };
    entry.bytes = streams.iter().try_fold(0u64, |total, stream| {
        total
            .checked_add(stream.size)
            .ok_or_else(|| io::Error::other("stream byte total overflow"))
    })?;
    entry.streams = streams.len();
    entry.snapshot = Some(Snapshot {
        identity: dio(
            "source_identity(FileIdInfo/legacy FAT)",
            &path,
            platform::source_identity(&file),
        )?,
        modified: metadata.last_write_time(),
        length: metadata.len(),
        attributes: metadata.file_attributes(),
        streams,
    });
    let frame = if directory {
        Some((file, dio("read_dir", &path, fs::read_dir(&path))?))
    } else {
        None
    };
    Ok((entry, frame))
}

fn unsupported(relative: PathBuf, depth: usize, error: Error) -> PlannedEntry {
    diagnostics::failure(
        "scan.unsupported_entry",
        &error,
        serde_json::json!({"relative":diagnostics::path(&relative),"depth":depth}),
    );
    PlannedEntry {
        relative,
        depth,
        kind: EntryKind::Unsupported,
        bytes: 0,
        streams: 0,
        hardlink_of: None,
        problem: Some(error.to_string()),
        snapshot: None,
    }
}

fn append(
    plan: &mut OperationPlan,
    mut entry: PlannedEntry,
    frame: Option<(File, ReadDir)>,
    stack: &mut Vec<Frame>,
    identities: &mut HashMap<platform::Identity, usize>,
    budget: &mut Budget,
) -> Result<()> {
    // Account for Vec/HashMap growth, path copies held by DFS frames, and stream names.
    let mut bytes = 2 * size_of::<PlannedEntry>() + 128 + 2 * entry.relative.capacity();
    if let Some(problem) = &entry.problem {
        bytes += problem.capacity();
    }
    if let Some(snapshot) = &entry.snapshot {
        bytes += snapshot.streams.capacity() * size_of::<platform::Stream>();
        bytes += snapshot
            .streams
            .iter()
            .map(|s| s.name.capacity())
            .sum::<usize>();
    }
    budget.charge(bytes)?;
    let index = plan.entries.len();
    match entry.kind {
        EntryKind::File => {
            plan.totals.files += 1;
            let snapshot = entry
                .snapshot
                .as_ref()
                .ok_or_else(|| io::Error::other("file snapshot missing"))?;
            if let Some(first) = identities.get(&snapshot.identity) {
                entry.hardlink_of = Some(*first);
                plan.totals.hardlink_copies += 1;
            } else {
                identities.insert(snapshot.identity.clone(), index);
            }
            plan.totals.bytes = plan
                .totals
                .bytes
                .checked_add(entry.bytes)
                .ok_or_else(|| io::Error::other("plan byte total overflow"))?;
            plan.totals.streams = plan
                .totals
                .streams
                .checked_add(entry.streams)
                .ok_or_else(|| io::Error::other("plan stream count overflow"))?;
        }
        EntryKind::Directory => plan.totals.directories += 1,
        EntryKind::Reparse => plan.totals.reparse_points += 1,
        EntryKind::Unsupported => plan.totals.unsupported += 1,
    }
    if let Some((guard, reader)) = frame {
        stack.push(Frame {
            _guard: guard,
            relative: entry.relative.clone(),
            reader,
            entry: index,
        });
    }
    plan.entries.push(entry);
    Ok(())
}

/// Scan without creating a destination or following links. Root reparse points are always refused.
/// ReparsePolicy::Skip applies only to entries within a regular source directory.
pub fn scan(
    source: &Path,
    destination_dir: &Path,
    options: ScanOptions,
    cancel: &AtomicBool,
) -> Result<OperationPlan> {
    diagnostics::event(
        "scan.start",
        serde_json::json!({"source":diagnostics::path(source),"destination":diagnostics::path(destination_dir),"reparse":options.reparse,"limits":options.limits}),
    );
    check_cancel(cancel)?;
    let limits = options.limits;
    if limits.max_entries == 0
        || limits.max_entries > MAX_ENTRIES
        || limits.max_depth > MAX_DEPTH
        || limits.max_plan_bytes == 0
        || limits.max_plan_bytes > MAX_PLAN_BYTES
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid scan limits").into());
    }
    validate_path(source)?;
    validate_path(destination_dir)?;
    let source = std::path::absolute(source)?;
    let destination_dir = std::path::absolute(destination_dir)?;
    let _source_parents = dio(
        "guard_source_parents",
        &source,
        platform::guard_parents(&source),
    )?;
    let _dest_parents = dio(
        "guard_destination_parents",
        &destination_dir,
        platform::guard_parents(&destination_dir.join(".vartacopy-guard")),
    )?;
    let metadata = dio("symlink_metadata", &source, fs::symlink_metadata(&source))?;
    let root_guard = if metadata.is_dir() {
        dio(
            "open_source_directory",
            &source,
            platform::open_directory(&source, false),
        )?
    } else {
        source_file(&source).inspect_err(|e| {
            diagnostics::failure(
                "scan.open_source",
                e,
                serde_json::json!({"source":diagnostics::path(&source)}),
            )
        })?
    };
    let destination_guard = dio(
        "open_destination_directory",
        &destination_dir,
        platform::open_directory(&destination_dir, false),
    )?;
    diagnostics::volume("source", &root_guard, &source);
    diagnostics::volume("destination", &destination_guard, &destination_dir);
    let source = dio("canonicalize_source", &source, source.canonicalize())?;
    let destination_dir = dio(
        "canonicalize_destination",
        &destination_dir,
        destination_dir.canonicalize(),
    )?;
    let source_identity = dio(
        "source_identity(FileIdInfo/legacy FAT):source",
        &source,
        platform::source_identity(&root_guard),
    )?;
    diagnostics::event(
        "scan.source_identity_policy",
        serde_json::json!({
            "source":diagnostics::path(&source),"kind":source_identity.kind,
            "path_scoped_read_only":source_identity.kind == platform::IdentityKind::FatSource,
        }),
    );
    if metadata.is_dir() {
        // Identity, not spelling/case, detects a destination inside the source.
        for ancestor in destination_dir.ancestors() {
            let guard = dio(
                "open_overlap_ancestor",
                ancestor,
                platform::open_directory(ancestor, false),
            )?;
            if dio(
                "GetFileInformationByHandleEx(FileIdInfo):ancestor",
                ancestor,
                platform::source_identity(&guard),
            )? == source_identity
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "source/destination overlap: destination is inside source",
                )
                .into());
            }
        }
    }
    let name = source.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "copying volume roots is not supported",
        )
    })?;
    let destination = destination_dir.join(name);
    let mut budget = Budget {
        used: 0,
        limit: limits.max_plan_bytes,
    };
    budget.charge(2 * (source.capacity() + destination.capacity()) + size_of::<OperationPlan>())?;
    let mut plan = OperationPlan {
        status: "ready",
        source,
        destination,
        totals: ScanTotals::default(),
        limits,
        reparse_policy: options.reparse,
        hardlink_policy: "materialize each name as an independent file",
        metadata_policy: "file data + ADS + basic times/flags; directory times/flags after children; inherited ACL/owner; directory ADS refused",
        accounted_plan_bytes: 0,
        entries: Vec::new(),
        destination_parent_identity: dio(
            "GetFileInformationByHandleEx(FileIdInfo):destination",
            &destination_dir,
            platform::source_identity(&destination_guard),
        )?,
    };
    let mut stack = Vec::new();
    let mut identities = HashMap::new();
    let (entry, frame) = match inspect(&plan.source, PathBuf::new(), 0) {
        Ok(value) => value,
        Err(error) => (unsupported(PathBuf::new(), 0, error), None),
    };
    append(
        &mut plan,
        entry,
        frame,
        &mut stack,
        &mut identities,
        &mut budget,
    )?;
    while let Some(frame) = stack.last_mut() {
        check_cancel(cancel)?;
        let Some(next) = frame.reader.next() else {
            stack.pop();
            continue;
        };
        let child = match next {
            Ok(child) => child,
            Err(error) => {
                diagnostics::failure(
                    "scan.enumeration_error",
                    &error,
                    serde_json::json!({"source":diagnostics::path(&plan.source),"relative":diagnostics::path(&frame.relative)}),
                );
                let index = frame.entry;
                let problem = format!("directory enumeration failed: {error}");
                budget.charge(problem.len())?;
                plan.entries[index].problem = Some(problem);
                plan.entries[index].kind = EntryKind::Unsupported;
                plan.totals.directories -= 1;
                plan.totals.unsupported += 1;
                stack.pop();
                continue;
            }
        };
        if plan.entries.len() >= limits.max_entries {
            return Err(Error::ScanLimit {
                resource: "entries",
                limit: limits.max_entries,
            });
        }
        let relative = frame.relative.join(child.file_name());
        let depth = relative.components().count();
        if depth > limits.max_depth {
            return Err(Error::ScanLimit {
                resource: "directory depth",
                limit: limits.max_depth,
            });
        }
        let (entry, child_frame) = match inspect(&plan.source, relative.clone(), depth) {
            Ok(value) => value,
            Err(error) => (unsupported(relative, depth, error), None),
        };
        append(
            &mut plan,
            entry,
            child_frame,
            &mut stack,
            &mut identities,
            &mut budget,
        )?;
    }
    plan.accounted_plan_bytes = budget.used;
    validate_fat_destination(&plan)?;
    if plan.totals.unsupported > 0
        || (plan.totals.reparse_points > 0 && options.reparse == ReparsePolicy::Reject)
    {
        plan.status = "blocked";
    }
    diagnostics::event(
        "scan.finished",
        serde_json::json!({"status":plan.status,"source":diagnostics::path(&plan.source),"destination":diagnostics::path(&plan.destination),"totals":plan.totals,"plan_bytes":plan.accounted_plan_bytes}),
    );
    Ok(plan)
}

pub(crate) fn validate_fat_destination(plan: &OperationPlan) -> Result<()> {
    if plan.destination_parent_identity.kind != platform::IdentityKind::FatSource {
        return Ok(());
    }
    for entry in &plan.entries {
        if let Some(snapshot) = &entry.snapshot {
            if snapshot.streams.iter().any(|s| !s.name.is_empty()) {
                return Err(io::Error::new(io::ErrorKind::Unsupported, format!(
                    "FAT32 не підтримує додаткові потоки NTFS (ADS): {}. Копіювання не розпочато, щоб не втратити ці дані.",
                    plan.source_path(entry).display())).into());
            }
            if snapshot.streams.iter().any(|s| s.size > u32::MAX as u64) {
                return Err(io::Error::new(io::ErrorKind::Unsupported, format!(
                    "FAT32 не підтримує файл понад 4 ГіБ − 1 байт: {}. Виберіть NTFS або exFAT.",
                    plan.source_path(entry).display())).into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod fat_policy_tests {
    use super::*;
    #[test]
    fn fat_size_boundary_and_named_streams_are_checked_before_acceptance() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.bin");
        fs::write(&source, b"fixture").unwrap();
        let destination = root.path().join("destination");
        fs::create_dir(&destination).unwrap();
        let mut plan = scan(
            &source,
            &destination,
            ScanOptions::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        plan.destination_parent_identity.kind = platform::IdentityKind::FatSource;
        plan.entries[0].snapshot.as_mut().unwrap().streams[0].size = u32::MAX as u64;
        validate_fat_destination(&plan).unwrap();
        plan.entries[0].snapshot.as_mut().unwrap().streams[0].size += 1;
        assert!(
            validate_fat_destination(&plan)
                .unwrap_err()
                .to_string()
                .contains("4 ГіБ")
        );
        plan.entries[0].snapshot.as_mut().unwrap().streams[0].size = 0;
        plan.entries[0]
            .snapshot
            .as_mut()
            .unwrap()
            .streams
            .push(platform::Stream {
                name: ":note:$DATA".into(),
                size: 1,
            });
        assert!(
            validate_fat_destination(&plan)
                .unwrap_err()
                .to_string()
                .contains("ADS")
        );
        let path = root.path().join("jobs.sqlite");
        let mut journal = crate::journal::Journal::open(&path).unwrap();
        assert!(journal.enqueue(&plan, crate::Conflict::Error).is_err());
        assert!(journal.jobs(10, 0).unwrap().is_empty());
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    }
}
