//! Persisted directory identity and post-order metadata recovery.
use super::*;
use crate::tree::EntryProblem;

struct Directory {
    ordinal: usize,
    relative: PathBuf,
    target: PathBuf,
    source: Option<File>,
    destination: File,
}

fn issue(
    journal: &mut Journal,
    job: &Job,
    entry: &Entry,
    error: Error,
    report: &mut RunReport,
) -> Result<()> {
    diagnostics::failure(
        "job.entry_error",
        &error,
        serde_json::json!({"job_id":job.id,"entry":entry.ordinal,"source":diagnostics::path(&source_path(job,entry)),"target":entry.target.as_deref().map(diagnostics::path)}),
    );
    if matches!(
        error,
        Error::Database(_) | Error::Json(_) | Error::Journal(_) | Error::Cancelled
    ) {
        return Err(error);
    }
    let message = error.to_string();
    journal
        .entry_error(job.id, entry.ordinal, &message)
        .map_err(|save| {
            Error::Journal(format!("{message}; cannot persist entry failure: {save}"))
        })?;
    report.entries_failed += 1;
    if report.issues.len() < 256 {
        report.issues.push(EntryProblem {
            path: source_path(job, entry).as_path().into(),
            status: "failed",
            message,
        });
    } else {
        report.issues_omitted += 1;
    }
    Ok(())
}

fn cleanup_skipped(job: &Job, entry: &Entry) -> Result<()> {
    if let (Some(path), Some(identity)) = (&entry.stage, &entry.stage_identity) {
        if entry
            .snapshot
            .as_ref()
            .is_some_and(|s| s.identity == *identity)
        {
            return Err(Error::Journal("recorded staging aliases source".into()));
        }
        if path.try_exists()? {
            let _parents = platform::guard_parents(path)?;
            let mut stage = if entry.kind == EntryKind::Directory {
                if path.parent() != job.destination.parent() {
                    return Err(Error::Journal(
                        "directory staging outside job volume root".into(),
                    ));
                }
                platform::Stage::reopen_directory(path, identity)?
            } else {
                platform::Stage::reopen(path, identity)?
            };
            stage.discard()?;
        }
    }
    Ok(())
}

fn skip_subtree(journal: &mut Journal, job: &Job, entry: &Entry) -> Result<()> {
    let mut end = entry.ordinal + 1;
    while end < job.entry_count {
        let next = journal.entry(job.id, end)?;
        if !next.relative.starts_with(&entry.relative) {
            break;
        }
        end += 1;
    }
    journal.skip_range(job.id, entry.ordinal, end)
}

fn directory_candidate(target: &Path, number: u32) -> Result<PathBuf> {
    let mut name = required(target.file_name(), "directory name missing")?.to_os_string();
    name.push(format!(" ({number})"));
    Ok(target.with_file_name(name))
}

fn ensure_directory(
    journal: &mut Journal,
    job: &Job,
    mut entry: Entry,
    target: &Path,
    need_source: bool,
    control: &Control,
    observer: &mut Observer<'_>,
) -> Result<Option<Directory>> {
    let source = source_path(job, &entry);
    let snapshot = entry
        .snapshot
        .as_ref()
        .ok_or_else(|| Error::Journal("directory snapshot missing".into()))?;
    let source_guard = if need_source || entry.state != EntryState::Committed {
        let guard = platform::open_directory(&source, false)?;
        snapshot.validate(&guard, &source, true)?;
        Some(guard)
    } else {
        None
    };
    let mut selected = entry.target.clone().unwrap_or_else(|| target.to_path_buf());
    if selected.parent() != target.parent() {
        return Err(Error::Journal(
            "directory target outside expected parent".into(),
        ));
    }
    if matches!(
        entry.state,
        EntryState::Committed | EntryState::DirectoryReady
    ) {
        let guard = platform::open_directory(&selected, true)?;
        let expected = entry
            .target_identity
            .as_ref()
            .ok_or_else(|| Error::Journal("directory identity missing".into()))?;
        if platform::identity(&guard)? != *expected || *expected == snapshot.identity {
            return Err(Error::Verification(
                "owned destination directory replaced or aliases source".into(),
            ));
        }
        if entry.state == EntryState::Committed && need_source {
            journal.reopen_directory_metadata(job.id, entry.ordinal)?;
        }
        return Ok(Some(Directory {
            ordinal: entry.ordinal,
            relative: entry.relative,
            target: selected,
            source: source_guard,
            destination: guard,
        }));
    }
    if !matches!(entry.state, EntryState::Planned | EntryState::DirIntent) {
        return Err(Error::Journal("invalid directory recovery state".into()));
    }
    if entry.state == EntryState::Planned
        && matches!(job.conflict, Conflict::Skip)
        && selected.try_exists()?
    {
        skip_subtree(journal, job, &entry)?;
        return Ok(None);
    }
    // Directory stages are siblings of the job root, on the same volume.
    // An unrecorded orphan cannot occupy a planned child name.
    let mut stage = if entry.state == EntryState::Planned {
        let mut stage = platform::Stage::create_directory(required(
            job.destination.parent(),
            "job parent missing",
        )?)?;
        let identity = platform::identity(stage.file())?;
        journal.record_directory(job.id, entry.ordinal, stage.path(), &selected, &identity)?;
        stage.preserve();
        notify(
            observer,
            job.id,
            entry.ordinal,
            Phase::DirectoryIntent,
            0,
            0,
        )?;
        entry = journal.entry(job.id, entry.ordinal)?;
        stage
    } else {
        let (path, identity) = stage_owned(&entry)?;
        if *identity == snapshot.identity {
            return Err(Error::Journal("staging directory aliases source".into()));
        }
        if path.parent() != job.destination.parent() {
            return Err(Error::Journal(
                "directory stage outside selected volume root".into(),
            ));
        }
        match platform::open_directory(&selected, true) {
            Ok(guard) if platform::identity(&guard)? == *identity => {
                journal.directory_ready(job.id, entry.ordinal, identity)?;
                notify(observer, job.id, entry.ordinal, Phase::DirectoryReady, 0, 0)?;
                return Ok(Some(Directory {
                    ordinal: entry.ordinal,
                    relative: entry.relative,
                    target: selected,
                    source: source_guard,
                    destination: guard,
                }));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        platform::Stage::reopen_directory(path, identity)?
    };
    let identity = platform::identity(stage.file())?;
    for number in 0..=10_000 {
        control.checkpoint()?;
        if number > 0 {
            selected = directory_candidate(target, number)?;
            journal.retarget_directory(job.id, entry.ordinal, &selected)?;
        }
        match stage.publish(&selected) {
            Ok(()) => {
                notify(
                    observer,
                    job.id,
                    entry.ordinal,
                    Phase::DirectoryPublished,
                    0,
                    0,
                )?;
                journal.directory_ready(job.id, entry.ordinal, &identity)?;
                notify(observer, job.id, entry.ordinal, Phase::DirectoryReady, 0, 0)?;
                drop(stage);
                // Stage handles have DELETE access; reopen a normal non-DELETE
                // directory guard so descendants can use standard sharing rules.
                let guard = platform::open_directory(&selected, true)?;
                if platform::identity(&guard)? != identity {
                    return Err(Error::Verification(
                        "directory changed after publication".into(),
                    ));
                }
                return Ok(Some(Directory {
                    ordinal: entry.ordinal,
                    relative: entry.relative,
                    target: selected,
                    source: source_guard,
                    destination: guard,
                }));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => match job.conflict {
                Conflict::Error => return Err(error.into()),
                Conflict::KeepBoth => continue,
                Conflict::Skip => {
                    skip_subtree(journal, job, &entry)?;
                    stage.discard()?;
                    return Ok(None);
                }
            },
            Err(error) => return Err(error.into()),
        }
    }
    Err(Error::Journal("directory candidate limit reached".into()))
}

fn finish(
    journal: &mut Journal,
    job: &Job,
    directory: Directory,
    control: &Control,
    observer: &mut Observer<'_>,
) -> Result<()> {
    control.checkpoint()?;
    let entry = journal.entry(job.id, directory.ordinal)?;
    if entry.state == EntryState::Committed {
        journal.clear_entry_error(job.id, entry.ordinal)?;
        return Ok(());
    }
    if entry.state != EntryState::DirectoryReady {
        return Err(Error::Journal("directory is not ready for metadata".into()));
    }
    let source = directory
        .source
        .as_ref()
        .ok_or_else(|| Error::Journal("directory source handle missing".into()))?;
    entry
        .snapshot
        .as_ref()
        .ok_or_else(|| Error::Journal("directory snapshot missing".into()))?
        .validate(source, &source_path(job, &entry), true)?;
    platform::copy_directory_metadata(source, &directory.destination)?;
    journal.transition(
        job.id,
        entry.ordinal,
        EntryState::DirectoryReady,
        EntryState::Committed,
    )?;
    notify(
        observer,
        job.id,
        entry.ordinal,
        Phase::DirectoryCommitted,
        0,
        0,
    )
}

pub(super) fn run_tree(
    journal: &mut Journal,
    job: &Job,
    control: &Control,
    observer: &mut Observer<'_>,
    report: &mut RunReport,
) -> Result<()> {
    let (committed, skipped) = journal.counts(job.id)?;
    let need_source = committed + skipped < job.entry_count;
    let mut directories: Vec<Directory> = Vec::new();
    let mut blocked: Option<PathBuf> = None;
    for ordinal in 0..job.entry_count {
        control.checkpoint()?;
        let entry = journal.entry(job.id, ordinal)?;
        if entry.state == EntryState::Skipped {
            cleanup_skipped(job, &entry)?;
            journal.clear_entry_error(job.id, ordinal)?;
            continue;
        }
        if blocked
            .as_ref()
            .is_some_and(|path| entry.relative.starts_with(path))
        {
            continue;
        }
        blocked = None;
        while directories.last().is_some_and(|d| {
            !entry.relative.starts_with(&d.relative) || entry.relative == d.relative
        }) {
            let directory = required(directories.pop(), "directory stack empty")?;
            let index = directory.ordinal;
            if let Err(error) = finish(journal, job, directory, control, observer) {
                issue(journal, job, &journal.entry(job.id, index)?, error, report)?;
            }
        }
        let target = if ordinal == 0 {
            job.destination.clone()
        } else {
            let parent = required(directories.last(), "missing persisted parent directory")?;
            if entry.relative.parent() != Some(parent.relative.as_path()) {
                return Err(Error::Journal(
                    "saved plan is not a valid preorder tree".into(),
                ));
            }
            parent
                .target
                .join(required(entry.relative.file_name(), "entry name missing")?)
        };
        match entry.kind {
            EntryKind::Directory => {
                let relative = entry.relative.clone();
                match ensure_directory(journal, job, entry, &target, need_source, control, observer)
                {
                    Ok(Some(directory)) => directories.push(directory),
                    Ok(None) => {}
                    Err(error) => {
                        issue(
                            journal,
                            job,
                            &journal.entry(job.id, ordinal)?,
                            error,
                            report,
                        )?;
                        blocked = Some(relative);
                    }
                }
            }
            EntryKind::File => match resume_file(journal, job, entry, &target, control, observer) {
                Ok((_, resumed, bytes)) => {
                    journal.clear_entry_error(job.id, ordinal)?;
                    report.resumed_bytes += resumed;
                    report.verified_bytes += bytes;
                }
                Err(error) => issue(
                    journal,
                    job,
                    &journal.entry(job.id, ordinal)?,
                    error,
                    report,
                )?,
            },
            EntryKind::Reparse => {
                return Err(Error::Journal(
                    "unskipped reparse entry in persisted plan".into(),
                ));
            }
            EntryKind::Unsupported => {
                return Err(Error::Journal("unsupported persisted entry".into()));
            }
        }
    }
    while let Some(directory) = directories.pop() {
        let index = directory.ordinal;
        if let Err(error) = finish(journal, job, directory, control, observer) {
            issue(journal, job, &journal.entry(job.id, index)?, error, report)?;
        }
    }
    Ok(())
}
