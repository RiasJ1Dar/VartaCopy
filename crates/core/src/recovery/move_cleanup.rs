//! Copy commitment and source removal are deliberately separate durable facts.
use super::*;

fn destination_guard(
    journal: &Journal,
    job: &Job,
    entry: &Entry,
    control: &Control,
    observer: &mut Observer<'_>,
) -> Result<(Vec<File>, platform::CleanupOwner)> {
    let target = required(entry.target.as_deref(), "cleanup target missing")?;
    let expected = required(
        entry.target_identity.as_ref(),
        "cleanup target identity missing",
    )?;
    let parents = platform::guard_parents(target)?;
    let guard =
        platform::CleanupOwner::open(target, expected, entry.kind == EntryKind::Directory, false)?;
    if platform::identity(guard.file()?)? != *expected {
        return Err(Error::Verification("cleanup destination replaced".into()));
    }
    if entry.kind == EntryKind::File {
        verify_recorded(journal, job.id, entry, target, None, control, observer)?;
    } else if !platform::streams(target)?.is_empty() {
        return Err(Error::Verification(
            "unexpected destination directory streams".into(),
        ));
    }
    Ok((parents, guard))
}

fn verify_source(
    journal: &Journal,
    job: &Job,
    entry: &Entry,
    path: &Path,
    owner: &File,
    control: &Control,
) -> Result<()> {
    let snapshot = required(entry.snapshot.as_ref(), "cleanup source snapshot missing")?;
    snapshot.validate(owner, path, false)?;
    // The owner denies WRITE and DELETE sharing; ADS readers must share its DELETE access.
    for (index, stream) in snapshot.streams.iter().enumerate() {
        let mut input = if stream.name.is_empty() {
            owner.try_clone()?
        } else {
            platform::open_cleanup_stream(&platform::stream_path(path, &stream.name))?
        };
        input.seek(SeekFrom::Start(0))?;
        let mut count = 0u64;
        let mut buffer = vec![0; CHUNK_BYTES];
        for chunk in journal.chunks(job.id, entry.ordinal, index)? {
            control.checkpoint()?;
            if chunk.offset != count {
                return Err(Error::Journal("noncontiguous cleanup hashes".into()));
            }
            input.read_exact(&mut buffer[..chunk.length])?;
            if blake3::hash(&buffer[..chunk.length]).as_bytes() != &chunk.hash {
                return Err(Error::Verification("cleanup source bytes changed".into()));
            }
            count += chunk.length as u64;
        }
        if count != stream.size || input.metadata()?.len() != stream.size {
            return Err(Error::Verification(
                "cleanup source stream size changed".into(),
            ));
        }
    }
    snapshot.validate(owner, path, false)
}

fn absent(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

fn prepare_directory_intent(
    journal: &mut Journal,
    job: &Job,
    entry: &Entry,
    control: &Control,
    observer: &mut Observer<'_>,
) -> Result<()> {
    if entry.kind != EntryKind::Directory || entry.state != EntryState::Committed {
        return Ok(());
    }
    let state = journal.cleanup_state(job.id, entry.ordinal)?;
    if state.is_some() {
        let path = source_path(job, entry);
        if state.as_deref() == Some("intent") && absent(&path)? {
            let snapshot = required(
                entry.snapshot.as_ref(),
                "directory cleanup snapshot missing",
            )?;
            confirm_identity_removed(&path, &snapshot.identity)?;
        }
        return Ok(());
    }
    let _destination = destination_guard(journal, job, entry, control, observer)?;
    let path = source_path(job, entry);
    let _parents = platform::guard_parents(&path)?;
    let snapshot = required(
        entry.snapshot.as_ref(),
        "directory cleanup snapshot missing",
    )?;
    let owner = platform::CleanupOwner::open(&path, &snapshot.identity, true, true)?;
    snapshot.validate(owner.file()?, &path, true)?;
    control.checkpoint()?;
    journal.cleanup_transition(job.id, entry.ordinal, false)?;
    drop(owner);
    notify(observer, job.id, entry.ordinal, Phase::CleanupIntent, 0, 0)
}

fn confirm_identity_removed(path: &Path, identity: &platform::Identity) -> Result<()> {
    if platform::identity_exists_on_source_volume(path, identity)? {
        return Err(Error::Verification(
            "source identity remains live under another name; cleanup needs reconciliation".into(),
        ));
    }
    Ok(())
}

fn cleanup_entry(
    journal: &mut Journal,
    job: &Job,
    entry: &Entry,
    control: &Control,
    observer: &mut Observer<'_>,
) -> Result<()> {
    if entry.state == EntryState::Skipped {
        return Ok(());
    }
    if entry.state != EntryState::Committed {
        return Err(Error::Verification(
            "copy is not committed; source retained".into(),
        ));
    }
    let _destination = destination_guard(journal, job, entry, control, observer)?;
    let path = source_path(job, entry);
    let state = journal.cleanup_state(job.id, entry.ordinal)?;
    if state.as_deref() == Some("removed") {
        if !absent(&path)? {
            return Err(Error::Verification(
                "source name recreated after cleanup; preserved".into(),
            ));
        }
        journal.clear_entry_error(job.id, entry.ordinal)?;
        return Ok(());
    }
    let snapshot = required(entry.snapshot.as_ref(), "cleanup snapshot missing")?;
    if absent(&path)? {
        if state.as_deref() != Some("intent") {
            return Err(Error::Verification(
                "source missing without cleanup intent".into(),
            ));
        }
        confirm_identity_removed(&path, &snapshot.identity)?;
    } else {
        let _parents = platform::guard_parents(&path)?;
        let mut owner = platform::CleanupOwner::open(
            &path,
            &snapshot.identity,
            entry.kind == EntryKind::Directory,
            true,
        )?;
        if entry.kind == EntryKind::Directory {
            snapshot.validate_cleanup_directory(owner.file()?, &path)?;
        } else {
            verify_source(journal, job, entry, &path, owner.file()?, control)?;
        }
        if state.is_none() {
            control.checkpoint()?;
            journal.cleanup_transition(job.id, entry.ordinal, false)?;
            notify(observer, job.id, entry.ordinal, Phase::CleanupIntent, 0, 0)?;
        }
        control.checkpoint()?;
        owner.prepare_removal(&snapshot.identity)?;
        notify(
            observer,
            job.id,
            entry.ordinal,
            Phase::SourceDeletePrepared,
            0,
            0,
        )?;
        control.checkpoint()?;
        owner.commit_removal()?;
        notify(observer, job.id, entry.ordinal, Phase::SourceRemoved, 0, 0)?;
        if !absent(&path)? {
            return Err(Error::Verification(
                "source removal did not complete".into(),
            ));
        }
    }
    journal.cleanup_transition(job.id, entry.ordinal, true)?;
    notify(
        observer,
        job.id,
        entry.ordinal,
        Phase::CleanupCommitted,
        0,
        0,
    )?;
    journal.clear_entry_error(job.id, entry.ordinal)?;
    Ok(())
}

fn persist_failure(
    journal: &mut Journal,
    job: &Job,
    entry: &Entry,
    error: Error,
    report: &mut RunReport,
) -> Error {
    diagnostics::failure(
        "job.cleanup_error",
        &error,
        serde_json::json!({"job_id":job.id,"entry":entry.ordinal,"source":diagnostics::path(&source_path(job,entry)),"target":entry.target.as_deref().map(diagnostics::path)}),
    );
    if matches!(error, Error::Cancelled) {
        return error;
    }
    let message = format!(
        "source cleanup entry {} ({}): {error}",
        entry.ordinal,
        source_path(job, entry).display()
    );
    if report.issues.len() < 256 {
        report.issues.push(crate::tree::EntryProblem {
            path: source_path(job, entry).as_path().into(),
            status: "failed",
            message: message.clone(),
        });
    } else {
        report.issues_omitted += 1;
    }
    match journal.entry_error(job.id, entry.ordinal, &message) {
        Ok(()) => Error::Verification(message),
        Err(save) => Error::Journal(format!("{message}; cannot persist cleanup issue: {save}")),
    }
}

pub(super) fn run(
    journal: &mut Journal,
    job: &Job,
    control: &Control,
    observer: &mut Observer<'_>,
    report: &mut RunReport,
) -> Result<()> {
    // Persist directory intents before deleting descendants, while original
    // directory timestamps still match the scan. Recovery then permits only
    // our own child removals to change directory time, never identity or ADS.
    for ordinal in 0..job.entry_count {
        control.checkpoint()?;
        let entry = journal.entry(job.id, ordinal)?;
        if let Err(error) = prepare_directory_intent(journal, job, &entry, control, observer) {
            return Err(persist_failure(journal, job, &entry, error, report));
        }
    }
    // The recorded plan is preorder; reverse order removes directories last.
    for ordinal in (0..job.entry_count).rev() {
        control.checkpoint()?;
        let entry = journal.entry(job.id, ordinal)?;
        if let Err(error) = cleanup_entry(journal, job, &entry, control, observer) {
            return Err(persist_failure(journal, job, &entry, error, report));
        }
    }
    Ok(())
}
