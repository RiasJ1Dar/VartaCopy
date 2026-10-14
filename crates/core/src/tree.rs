//! Sequential execution of a fixed plan. Completed files remain after failure/cancellation.
//! Destination directory trees are never merged; a conflicting root is error/skip/keep-both.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::{ScanOptions, scan};
    use std::ffi::OsStr;
    use std::sync::atomic::Ordering;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let dest = root.path().join("dest");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&dest).unwrap();
        for i in 0..3 {
            fs::write(source.join(format!("file-{i}")), b"data").unwrap();
        }
        (root, source, dest)
    }

    #[test]
    fn cancellation_keeps_completed_files_and_reports_remaining_entries() {
        let (_root, source, dest) = fixture();
        let stop = AtomicBool::new(false);
        let plan = scan(&source, &dest, ScanOptions::default(), &stop).unwrap();
        let report = execute_with_hook(&plan, Conflict::Error, &stop, |index| {
            if index == 2 {
                stop.store(true, Ordering::Relaxed);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(report.status, "cancelled", "{report:?}");
        assert_eq!(report.exit_code(), 130);
        assert_eq!(report.files_copied, 1);
        assert_eq!(report.entries_not_attempted, 2);
        let output = dest.join("source");
        assert_eq!(fs::read_dir(&output).unwrap().count(), 1);
        assert!(
            fs::read_dir(&output).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".part"))
        );
    }

    #[test]
    fn added_directory_stream_prevents_success_at_finalization() {
        let (_root, source, dest) = fixture();
        let stop = AtomicBool::new(false);
        let plan = scan(&source, &dest, ScanOptions::default(), &stop).unwrap();
        let report = execute_with_hook(&plan, Conflict::Error, &stop, |index| {
            if index == 2 {
                fs::write(
                    platform::stream_path(&source, OsStr::new(":new-stream")),
                    b"metadata",
                )?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(report.status, "partial", "{report:?}");
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.entries_failed, 1);
        assert_eq!(report.files_copied, 3);
        assert_eq!(report.details[0].status, "directory_metadata_failed");
    }

    #[test]
    fn destination_child_collision_blocks_only_that_branch() {
        let (_root, source, dest) = fixture();
        fs::create_dir(source.join("child")).unwrap();
        fs::write(source.join("child/file"), b"child").unwrap();
        let stop = AtomicBool::new(false);
        let plan = scan(&source, &dest, ScanOptions::default(), &stop).unwrap();
        let child_index = plan
            .entries
            .iter()
            .position(|e| e.relative == Path::new("child"))
            .unwrap();
        let report = execute_with_hook(&plan, Conflict::KeepBoth, &stop, |index| {
            if index == child_index {
                fs::create_dir(dest.join("source/child"))?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(report.status, "partial", "{report:?}");
        assert_eq!(report.entries_failed, 2);
        assert_eq!(report.files_copied, 3);
        assert_eq!(fs::read_dir(dest.join("source/child")).unwrap().count(), 0);
    }

    #[test]
    fn problem_details_are_bounded_and_omission_is_explicit() {
        let (_root, source, dest) = fixture();
        for index in 0..300 {
            fs::write(source.join(format!("extra-{index}")), b"x").unwrap();
        }
        let stop = AtomicBool::new(false);
        let plan = scan(&source, &dest, ScanOptions::default(), &stop).unwrap();
        let report = execute_with_hook(&plan, Conflict::Error, &stop, |index| {
            if index > 0 {
                return Err(io::Error::other("injected per-file failure"));
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(report.entries_failed, 303);
        assert_eq!(report.details.len(), DETAIL_LIMIT);
        assert_eq!(report.details_omitted, 303 - DETAIL_LIMIT);
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.files_copied, 0);
    }
}

use crate::planner::{EntryKind, OperationPlan, ReparsePolicy};
use crate::{Conflict, Error, Result, check_cancel, copy, source_file};
use serde::Serialize;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use vartacopy_platform_windows as platform;

const DETAIL_LIMIT: usize = 256;

#[derive(Debug, Serialize)]
pub struct EntryProblem {
    pub path: platform::PathRecord,
    pub status: &'static str,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct CopyReport {
    pub status: &'static str,
    pub destination: platform::PathRecord,
    pub files_copied: usize,
    pub directories_created: usize,
    pub files_skipped: usize,
    pub entries_skipped: usize,
    pub entries_failed: usize,
    pub entries_not_attempted: usize,
    pub bytes: u64,
    pub streams_verified: usize,
    pub details: Vec<EntryProblem>,
    pub details_omitted: usize,
    pub metadata_policy: &'static str,
    pub hardlink_policy: &'static str,
}
impl CopyReport {
    fn new(plan: &OperationPlan) -> Self {
        Self {
            status: "failed",
            destination: plan.destination.as_path().into(),
            files_copied: 0,
            directories_created: 0,
            files_skipped: 0,
            entries_skipped: 0,
            entries_failed: 0,
            entries_not_attempted: plan.entries.len(),
            bytes: 0,
            streams_verified: 0,
            details: Vec::new(),
            details_omitted: 0,
            metadata_policy: plan.metadata_policy,
            hardlink_policy: plan.hardlink_policy,
        }
    }
    fn detail(&mut self, path: &Path, status: &'static str, message: impl Into<String>) {
        if self.details.len() < DETAIL_LIMIT {
            self.details.push(EntryProblem {
                path: path.into(),
                status,
                message: message.into(),
            });
        } else {
            self.details_omitted += 1;
        }
    }
    pub fn exit_code(&self) -> u8 {
        match self.status {
            "copied_verified" => 0,
            "cancelled" => 130,
            "failed" => 1,
            _ => 2,
        }
    }
}

struct Directory {
    index: usize,
    source: File,
    destination: File,
}

enum Root {
    Created(PathBuf, File),
    Skipped,
}

fn create_directory(path: &Path) -> Result<File> {
    fs::create_dir(path)?;
    platform::open_directory(path, true).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "created directory {} but could not guard it; directory retained: {error}",
                path.display()
            ),
        )
        .into()
    })
}

fn allocate_root(path: &Path, conflict: Conflict, cancel: &AtomicBool) -> Result<Root> {
    let mut target = path.to_path_buf();
    for number in 0..=10_000 {
        check_cancel(cancel)?;
        if number > 0 {
            let mut name = path
                .file_name()
                .ok_or_else(|| io::Error::other("missing directory name"))?
                .to_os_string();
            name.push(format!(" ({number})"));
            target = path.with_file_name(name);
        }
        match create_directory(&target) {
            Ok(file) => return Ok(Root::Created(target, file)),
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => match conflict
            {
                Conflict::Error => return Err(error.into()),
                Conflict::Skip => return Ok(Root::Skipped),
                Conflict::KeepBoth => continue,
            },
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other("keep-both directory candidate limit reached").into())
}

fn finish_directory(plan: &OperationPlan, directory: Directory, report: &mut CopyReport) {
    let entry = &plan.entries[directory.index];
    let path = plan.source_path(entry);
    let result = (|| {
        let snapshot = entry
            .snapshot
            .as_ref()
            .ok_or_else(|| io::Error::other("directory snapshot missing"))?;
        snapshot.validate(&directory.source, &path, true)?;
        platform::copy_directory_metadata(&directory.source, &directory.destination)?;
        Ok::<(), Error>(())
    })();
    if let Err(error) = result {
        report.entries_failed += 1;
        report.detail(&path, "directory_metadata_failed", error.to_string());
    }
}

fn file_copy(
    plan: &OperationPlan,
    index: usize,
    target_root: &Path,
    conflict: Conflict,
    cancel: &AtomicBool,
    report: &mut CopyReport,
) -> Result<()> {
    let entry = &plan.entries[index];
    let source = plan.source_path(entry);
    let target = target_root.join(&entry.relative);
    let source_guard = source_file(&source)?;
    let snapshot = entry
        .snapshot
        .as_ref()
        .ok_or_else(|| io::Error::other("file snapshot missing"))?;
    snapshot.validate(&source_guard, &source, false)?;
    let outcome = copy(
        &source,
        target
            .parent()
            .ok_or_else(|| io::Error::other("destination has no parent"))?,
        conflict,
        cancel,
    )?;
    if outcome.status == "copied_verified" {
        report.files_copied += 1;
        report.bytes += outcome.bytes;
        report.streams_verified += outcome.streams_verified;
        if outcome.destination.windows_utf16
            != platform::PathRecord::from(target.as_path()).windows_utf16
        {
            report.detail(
                &target,
                "renamed",
                format!("kept both; output: {}", outcome.destination.display),
            );
        }
    } else {
        report.files_skipped += 1;
        report.entries_skipped += 1;
        report.detail(
            &target,
            "skipped_unverified",
            "existing destination was not verified",
        );
    }
    Ok(())
}

pub fn execute(
    plan: &OperationPlan,
    conflict: Conflict,
    cancel: &AtomicBool,
) -> Result<CopyReport> {
    execute_with_hook(plan, conflict, cancel, |_| Ok(()))
}

fn execute_with_hook(
    plan: &OperationPlan,
    conflict: Conflict,
    cancel: &AtomicBool,
    mut before_entry: impl FnMut(usize) -> io::Result<()>,
) -> Result<CopyReport> {
    check_cancel(cancel)?;
    let mut report = CopyReport::new(plan);
    if !plan.is_ready() {
        for entry in &plan.entries {
            if entry.kind == EntryKind::Unsupported
                || (entry.kind == EntryKind::Reparse
                    && plan.reparse_policy == ReparsePolicy::Reject)
            {
                report.entries_failed += 1;
                report.entries_not_attempted -= 1;
                report.detail(
                    &plan.source_path(entry),
                    "unsupported",
                    entry
                        .problem
                        .as_deref()
                        .unwrap_or("unsupported source entry"),
                );
            }
        }
        return Ok(report);
    }
    let _source_parents = platform::guard_parents(&plan.source)?;
    let parent = plan
        .destination
        .parent()
        .ok_or_else(|| io::Error::other("destination parent missing"))?;
    let _target_parents = platform::guard_parents(&parent.join(".vartacopy-guard"))?;
    let target_parent = platform::open_directory(parent, false)?;
    if platform::identity(&target_parent)? != plan.destination_parent_identity {
        return Err(Error::Verification(
            "destination directory changed since scan".into(),
        ));
    }
    let root = plan
        .entries
        .first()
        .ok_or_else(|| io::Error::other("empty operation plan"))?;
    if root.kind == EntryKind::File {
        before_entry(0)?;
        let source_guard = source_file(&plan.source)?;
        root.snapshot
            .as_ref()
            .ok_or_else(|| io::Error::other("root file snapshot missing"))?
            .validate(&source_guard, &plan.source, false)?;
        let outcome = copy(&plan.source, parent, conflict, cancel)?;
        report.status = outcome.status;
        report.destination = outcome.destination;
        report.bytes = outcome.bytes;
        report.streams_verified = outcome.streams_verified;
        report.files_copied = usize::from(outcome.status == "copied_verified");
        report.files_skipped = usize::from(outcome.status == "skipped_unverified");
        report.entries_skipped = report.files_skipped;
        report.entries_not_attempted = 0;
        return Ok(report);
    }
    let source_root = platform::open_directory(&plan.source, false)?;
    root.snapshot
        .as_ref()
        .ok_or_else(|| io::Error::other("root directory snapshot missing"))?
        .validate(&source_root, &plan.source, true)?;
    let (target_root, target_guard) = match allocate_root(&plan.destination, conflict, cancel)? {
        Root::Created(path, guard) => (path, guard),
        Root::Skipped => {
            report.status = "skipped_unverified";
            report.files_skipped = plan.totals.files;
            report.entries_skipped = plan.entries.len();
            report.entries_not_attempted = 0;
            report.detail(
                &plan.destination,
                "skipped_unverified",
                "entire existing directory was skipped; no merge or verification",
            );
            return Ok(report);
        }
    };
    report.destination = target_root.as_path().into();
    report.directories_created = 1;
    let mut root_guard = Some(target_guard);
    let mut directories: Vec<Directory> = Vec::new();
    let mut blocked_depth = None;
    let mut cancelled = false;
    for (index, entry) in plan.entries.iter().enumerate() {
        if check_cancel(cancel).is_err() {
            cancelled = true;
            break;
        }
        if let Err(error) = before_entry(index) {
            report.entries_failed += 1;
            report.entries_not_attempted -= 1;
            report.detail(&plan.source_path(entry), "failed", error.to_string());
            if entry.kind == EntryKind::Directory {
                blocked_depth = Some(entry.depth);
            }
            continue;
        }
        if check_cancel(cancel).is_err() {
            cancelled = true;
            break;
        }
        report.entries_not_attempted -= 1;
        if blocked_depth.is_some_and(|depth| entry.depth > depth) {
            report.entries_failed += 1;
            report.detail(
                &plan.source_path(entry),
                "blocked",
                "parent directory failed",
            );
            continue;
        }
        blocked_depth = None;
        while directories
            .last()
            .is_some_and(|directory| plan.entries[directory.index].depth >= entry.depth)
        {
            let directory = directories
                .pop()
                .ok_or_else(|| io::Error::other("directory stack empty"))?;
            finish_directory(plan, directory, &mut report);
        }
        let source = plan.source_path(entry);
        let result = match entry.kind {
            EntryKind::Directory => (|| {
                let guard = platform::open_directory(&source, false)?;
                entry
                    .snapshot
                    .as_ref()
                    .ok_or_else(|| io::Error::other("directory snapshot missing"))?
                    .validate(&guard, &source, true)?;
                let destination = if index == 0 {
                    root_guard
                        .take()
                        .ok_or_else(|| io::Error::other("root directory handle missing"))?
                } else {
                    let directory = create_directory(&target_root.join(&entry.relative))?;
                    report.directories_created += 1;
                    directory
                };
                directories.push(Directory {
                    index,
                    source: guard,
                    destination,
                });
                Ok(())
            })(),
            EntryKind::File => file_copy(plan, index, &target_root, conflict, cancel, &mut report),
            EntryKind::Reparse => {
                report.entries_skipped += 1;
                report.detail(
                    &source,
                    "skipped_reparse",
                    "reparse point was not followed or copied",
                );
                Ok(())
            }
            EntryKind::Unsupported => {
                Err(io::Error::other("unsupported entry reached execution").into())
            }
        };
        if let Err(error) = result {
            if matches!(error, Error::Cancelled) {
                cancelled = true;
                report.entries_not_attempted += 1;
                break;
            }
            report.entries_failed += 1;
            report.detail(&source, "failed", error.to_string());
            if entry.kind == EntryKind::Directory {
                blocked_depth = Some(entry.depth);
            }
        }
    }
    if !cancelled {
        while let Some(directory) = directories.pop() {
            finish_directory(plan, directory, &mut report);
        }
    }
    report.status = if cancelled {
        "cancelled"
    } else if report.entries_failed > 0 || report.entries_skipped > 0 {
        "partial"
    } else {
        "copied_verified"
    };
    Ok(report)
}
