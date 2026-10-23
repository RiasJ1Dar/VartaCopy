//! Verified whole-root publication for filesystems without persistent file IDs.
//! Only freshly created handles authorize writes. Recovery verifies a published
//! root or starts a new private root; it never adopts/deletes an old FAT stage.
use super::*;
use std::{collections::HashMap, fs};

fn entries(journal: &Journal, job: &Job) -> Result<Vec<Entry>> {
    (0..job.entry_count)
        .map(|n| journal.entry(job.id, n))
        .collect()
}

fn candidate(job: &Job, number: u32, directory: bool) -> Result<PathBuf> {
    if number == 0 {
        return Ok(job.destination.clone());
    }
    if !directory {
        return Ok(crate::alternative(&job.destination, number));
    }
    let mut name = required(job.destination.file_name(), "FAT output name missing")?.to_os_string();
    name.push(format!(" ({number})"));
    Ok(job.destination.with_file_name(name))
}

fn private_parents(root: &Path, path: &Path) -> Result<Vec<File>> {
    let parent = required(path.parent(), "private FAT parent missing")?;
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| Error::Journal("private FAT path escaped its fresh root".into()))?;
    let mut current = root.to_path_buf();
    let mut guards = Vec::new();
    // The root is already pinned by its freshly created Stage handle (DELETE
    // access). Do not reopen it using incompatible ordinary sharing rules.
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(Error::Journal("invalid private FAT component".into()));
        }
        current.push(component);
        guards.push(platform::open_directory(&current, false)?);
    }
    Ok(guards)
}

fn copy_file(
    journal: &mut Journal,
    job: &Job,
    entry: &Entry,
    stage: &mut platform::Stage,
    control: &Control,
    observer: &mut Observer<'_>,
) -> Result<()> {
    let source = source_path(job, entry);
    let _parents = platform::guard_parents(&source)?;
    let mut input = crate::source_file(&source)?;
    let snapshot = required(entry.snapshot.as_ref(), "FAT source snapshot missing")?;
    snapshot.validate(&input, &source, false)?;
    if snapshot.streams.len() != 1
        || !snapshot.streams[0].name.is_empty()
        || snapshot.streams[0].size > u32::MAX as u64
    {
        return Err(Error::Journal(
            "FAT copy requires one unnamed stream of at most 4 GiB - 1 byte".into(),
        ));
    }
    transfer_stream(
        journal,
        StreamTask {
            job: job.id,
            ordinal: entry.ordinal,
            stream_index: 0,
            size: snapshot.streams[0].size,
        },
        &mut input,
        stage.file(),
        control,
        observer,
    )?;
    snapshot.validate(&input, &source, false)?;
    platform::copy_basic_metadata(&input, stage.file())?;
    journal.transition(
        job.id,
        entry.ordinal,
        EntryState::Copying,
        EntryState::Verified,
    )?;
    notify(observer, job.id, entry.ordinal, Phase::Verified, 0, 0)
}

/// A read-only proof of contents, never authority to reopen a stage for writes
/// or remove a source. The stable NTFS ownership guards remain untouched.
fn verify_output(
    journal: &Journal,
    job: &Job,
    target: &Path,
    control: &Control,
    observer: &mut Observer<'_>,
) -> Result<(u64, Vec<(usize, platform::Identity)>)> {
    let saved = entries(journal, job)?;
    let expected: HashMap<_, _> = saved
        .iter()
        .filter(|e| e.state != EntryState::Skipped)
        .map(|e| (e.relative.clone(), e))
        .collect();
    if expected.len()
        != saved
            .iter()
            .filter(|e| e.state != EntryState::Skipped)
            .count()
    {
        return Err(Error::Journal("duplicate FAT output names".into()));
    }
    let root = required(expected.get(Path::new("")), "FAT root entry missing")?;
    let _parents = platform::guard_parents(target)?;
    let mut witnesses = Vec::with_capacity(expected.len());
    let mut bytes = 0;
    if root.kind == EntryKind::File {
        let guard = crate::source_file(target)?;
        bytes = verify_recorded(journal, job.id, root, target, None, control, observer)?;
        witnesses.push((root.ordinal, platform::source_identity(&guard)?));
        return Ok((bytes, witnesses));
    }
    let guard = platform::open_directory(target, false)?;
    if !platform::streams(target)?.is_empty() {
        return Err(Error::Verification(
            "unexpected FAT directory streams".into(),
        ));
    }
    witnesses.push((0, platform::source_identity(&guard)?));
    let mut stack = vec![(PathBuf::new(), fs::read_dir(target)?, guard)];
    while let Some((relative, reader, _)) = stack.last_mut() {
        control.checkpoint()?;
        let Some(item) = reader.next() else {
            stack.pop();
            continue;
        };
        let item = item?;
        let relative = relative.join(item.file_name());
        let entry = expected.get(&relative).ok_or_else(|| {
            Error::Verification(format!(
                "unexpected file in FAT output: {}",
                item.path().display()
            ))
        })?;
        if witnesses.len() >= expected.len() {
            return Err(Error::Verification(
                "FAT output entry limit exceeded".into(),
            ));
        }
        let path = target.join(&relative);
        match entry.kind {
            EntryKind::Directory => {
                let guard = platform::open_directory(&path, false)?;
                if !platform::streams(&path)?.is_empty() {
                    return Err(Error::Verification(
                        "unexpected FAT directory streams".into(),
                    ));
                }
                witnesses.push((entry.ordinal, platform::source_identity(&guard)?));
                stack.push((relative, fs::read_dir(&path)?, guard));
            }
            EntryKind::File => {
                let guard = crate::source_file(&path)?;
                bytes += verify_recorded(journal, job.id, entry, &path, None, control, observer)?;
                witnesses.push((entry.ordinal, platform::source_identity(&guard)?));
            }
            _ => {
                return Err(Error::Verification(
                    "unexpected FAT output entry kind".into(),
                ));
            }
        }
    }
    if witnesses.len() != expected.len() {
        return Err(Error::Verification("FAT output is incomplete".into()));
    }
    Ok((bytes, witnesses))
}

fn finish(
    journal: &mut Journal,
    job: &Job,
    attempt: &crate::journal::FatAttempt,
    control: &Control,
    observer: &mut Observer<'_>,
    report: &mut RunReport,
) -> Result<()> {
    let (bytes, witnesses) = verify_output(journal, job, &attempt.target, control, observer)?;
    journal.fat_commit(job.id, attempt.id, &witnesses)?;
    report.verified_bytes = bytes;
    for (ordinal, _) in witnesses {
        let phase = if journal.entry(job.id, ordinal)?.kind == EntryKind::Directory {
            Phase::DirectoryCommitted
        } else {
            Phase::Committed
        };
        notify(observer, job.id, ordinal, phase, 0, 0)?;
    }
    Ok(())
}

pub(super) fn copy_root(
    journal: &mut Journal,
    job: &Job,
    control: &Control,
    observer: &mut Observer<'_>,
    report: &mut RunReport,
) -> Result<()> {
    if job.operation != OperationKind::Copy {
        return Err(Error::Journal("FAT destination Move is not enabled".into()));
    }
    let parent = required(job.destination.parent(), "FAT destination parent missing")?;
    let _parent_chain = platform::guard_parents(&parent.join(".vartacopy-guard"))?;
    let _parent = platform::open_directory(parent, false)?;
    if platform::source_identity(&_parent)? != job.parent_identity {
        return Err(Error::Verification(
            "FAT destination parent replaced".into(),
        ));
    }
    let previous = journal.fat_attempt(job.id)?;
    if let Some(attempt) = &previous {
        if attempt.stage.parent() != Some(parent) || attempt.target.parent() != Some(parent) {
            return Err(Error::Journal(
                "FAT attempt outside selected destination".into(),
            ));
        }
        if attempt.state == "committed" {
            return finish(journal, job, attempt, control, observer, report);
        }
        // A missing stage plus durable publish intent permits a read-only
        // complete-hash reconciliation, never stage adoption or source deletion.
        if attempt.state == "publishing"
            && !attempt.stage.try_exists()?
            && attempt.target.try_exists()?
        {
            return finish(journal, job, attempt, control, observer, report);
        }
    }
    let saved = entries(journal, job)?;
    if saved[0].state == EntryState::Skipped {
        return Ok(());
    }
    let directory = saved[0].kind == EntryKind::Directory;
    if !directory && (saved[0].kind != EntryKind::File || saved.len() != 1) {
        return Err(Error::Journal("invalid FAT root kind".into()));
    }
    let mut number = 0;
    let mut selected = candidate(job, number, directory)?;
    while selected.try_exists()? {
        match job.conflict {
            Conflict::Error => return Err(io::Error::from(io::ErrorKind::AlreadyExists).into()),
            Conflict::Skip => {
                journal.fat_skip(job.id)?;
                return Ok(());
            }
            Conflict::KeepBoth => {
                number += 1;
                if number > 10_000 {
                    return Err(Error::Journal(
                        "FAT keep-both candidate limit reached".into(),
                    ));
                }
                selected = candidate(job, number, directory)?;
            }
        }
    }
    // Validate all sources before recording a new attempt or creating any data.
    for entry in &saved {
        control.checkpoint()?;
        if entry.kind == EntryKind::Reparse {
            continue;
        }
        let source = source_path(job, entry);
        let _parents = platform::guard_parents(&source)?;
        let guard = if entry.kind == EntryKind::Directory {
            platform::open_directory(&source, false)?
        } else {
            crate::source_file(&source)?
        };
        required(entry.snapshot.as_ref(), "FAT source snapshot missing")?.validate(
            &guard,
            &source,
            entry.kind == EntryKind::Directory,
        )?;
        if previous.is_some() && entry.kind == EntryKind::File {
            // A restart writes a fresh root, but must still reject changes to
            // bytes already witnessed by the interrupted copy's checkpoints.
            let mut input = guard;
            let mut buffer = vec![0; CHUNK_BYTES];
            for chunk in journal.chunks(job.id, entry.ordinal, 0)? {
                control.checkpoint()?;
                input.seek(SeekFrom::Start(chunk.offset))?;
                input.read_exact(&mut buffer[..chunk.length])?;
                if blake3::hash(&buffer[..chunk.length]).as_bytes() != &chunk.hash {
                    return Err(Error::Verification(
                        "source changed after interrupted FAT copy".into(),
                    ));
                }
                notify(
                    observer,
                    job.id,
                    entry.ordinal,
                    Phase::Checking,
                    chunk.offset + chunk.length as u64,
                    0,
                )?;
            }
        }
    }
    if let Some(attempt) = &previous {
        diagnostics::durable(
            "fat.restart_from_source",
            serde_json::json!({"job_id":job.id,"retained_stage":diagnostics::path(&attempt.stage),"old_stage_reused":false}),
        );
        if report.issues.len() < 256 {
            report.issues.push(crate::tree::EntryProblem{path:attempt.stage.as_path().into(),status:"retained_partial",
                message:"Незавершене джерело на FAT32 копіюється заново. Попередні тимчасові дані збережено без змін; вони не використовуються для запису або видалення.".into()});
        }
    }
    let mut root = if directory {
        platform::Stage::create_directory(parent)?
    } else {
        platform::Stage::create(parent)?
    };
    let attempt_id = journal.fat_begin(job.id, root.path(), &selected)?;
    root.preserve();
    diagnostics::durable(
        "fat.fresh_stage",
        serde_json::json!({"job_id":job.id,"stage":diagnostics::path(root.path()),"recovery":"fresh_root_or_read_only_hash_reconciliation"}),
    );
    notify(observer, job.id, 0, Phase::StageRecorded, 0, 0)?;
    if directory {
        // Pin each fresh directory until all writes/metadata are finished.
        // A later pathname lookup must not adopt a replacement ordinary folder.
        let mut created = HashMap::new();
        for entry in saved.iter().skip(1) {
            control.checkpoint()?;
            if entry.kind == EntryKind::Reparse {
                continue;
            }
            let path = root.path().join(&entry.relative);
            let _parents = private_parents(root.path(), &path)?;
            if entry.kind == EntryKind::Directory {
                fs::create_dir(&path)?;
                created.insert(entry.ordinal, platform::open_directory(&path, true)?);
            } else {
                let mut file =
                    platform::Stage::create(required(path.parent(), "FAT file parent missing")?)?;
                copy_file(journal, job, entry, &mut file, control, observer)?;
                file.publish(&path)?;
            }
        }
        // Apply directory metadata after children, through freshly created,
        // pinned paths only. No persisted FAT ID authorizes these writes.
        for entry in saved
            .iter()
            .rev()
            .filter(|e| e.kind == EntryKind::Directory)
        {
            let source = source_path(job, entry);
            let _source_parents = platform::guard_parents(&source)?;
            let input = platform::open_directory(&source, false)?;
            required(entry.snapshot.as_ref(), "FAT directory snapshot missing")?
                .validate(&input, &source, true)?;
            if entry.ordinal == 0 {
                platform::copy_directory_metadata(&input, root.file())?;
            } else {
                let output = required(
                    created.get(&entry.ordinal),
                    "fresh FAT directory handle missing",
                )?;
                platform::copy_directory_metadata(&input, output)?;
            }
        }
        // No further writes use descendant pathnames after releasing these.
        // Some filesystems refuse renaming a directory with open descendants.
        drop(created);
    } else {
        copy_file(journal, job, &saved[0], &mut root, control, observer)?;
    }
    loop {
        control.checkpoint()?;
        journal.fat_publish_intent(job.id, attempt_id, &selected)?;
        notify(observer, job.id, 0, Phase::PublishIntent, 0, 0)?;
        match root.publish(&selected) {
            Ok(()) => break,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => match job.conflict {
                Conflict::Error => return Err(error.into()),
                Conflict::Skip => {
                    journal.fat_skip(job.id)?;
                    return Ok(());
                }
                Conflict::KeepBoth => {
                    number += 1;
                    if number > 10_000 {
                        return Err(Error::Journal(
                            "FAT keep-both candidate limit reached".into(),
                        ));
                    }
                    selected = candidate(job, number, directory)?;
                }
            },
            Err(error) => return Err(error.into()),
        }
    }
    notify(observer, job.id, 0, Phase::Published, 0, 0)?;
    drop(root);
    let attempt = required(journal.fat_attempt(job.id)?, "FAT attempt disappeared")?;
    finish(journal, job, &attempt, control, observer, report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::{self, ScanOptions};
    use std::sync::atomic::AtomicBool;

    fn report(id: i64, path: &Path) -> RunReport {
        RunReport {
            job_id: id,
            destination: path.into(),
            status: "running".into(),
            entries_committed: 0,
            entries_skipped: 0,
            entries_failed: 0,
            entries_pending: 0,
            resumed_bytes: 0,
            verified_bytes: 0,
            error: None,
            issues: Vec::new(),
            issues_omitted: 0,
        }
    }
    fn execute(journal: &mut Journal, id: i64, observer: &mut Observer<'_>) -> Result<RunReport> {
        let job = journal.job(id)?;
        let mut report = report(id, &job.destination);
        copy_root(journal, &job, &Control::default(), observer, &mut report)?;
        Ok(report)
    }
    fn fixture(directory: bool) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, i64) {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join(if directory {
            "дерево-😀"
        } else {
            "дані-😀.txt"
        });
        let destination = root.path().join("destination");
        fs::create_dir(&destination).unwrap();
        if directory {
            fs::create_dir(&source).unwrap();
            fs::create_dir(source.join("порожня")).unwrap();
            fs::write(source.join("нуль.txt"), []).unwrap();
            fs::write(source.join("дані.bin"), vec![71; CHUNK_BYTES + 17]).unwrap();
        } else {
            fs::write(&source, vec![71; CHUNK_BYTES + 17]).unwrap();
        }
        let path = root.path().join("jobs.sqlite");
        let mut journal = Journal::open(&path).unwrap();
        let plan = planner::scan(
            &source,
            &destination,
            ScanOptions::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let id = journal.enqueue(&plan, Conflict::Error).unwrap();
        drop(journal);
        (root, source, destination, path, id)
    }

    #[test]
    fn fresh_root_protocol_recovers_each_boundary_without_adopting_old_stages() {
        for directory in [false, true] {
            for phase in [
                Phase::StageRecorded,
                Phase::CheckpointCommitted,
                Phase::Verified,
                Phase::PublishIntent,
                Phase::Published,
            ] {
                let (_root, source, destination, path, id) = fixture(directory);
                let mut journal = Journal::open(&path).unwrap();
                let mut stopped = false;
                let result = execute(&mut journal, id, &mut |event| {
                    if event.phase == phase && !stopped {
                        stopped = true;
                        return Err(Error::Cancelled);
                    }
                    Ok(())
                });
                assert!(stopped && result.is_err(), "{directory}/{phase:?}");
                let old = journal.fat_attempt(id).unwrap().unwrap();
                let old_exists = old.stage.exists();
                drop(journal);
                let mut journal = Journal::open(&path).unwrap();
                let done = execute(&mut journal, id, &mut |_| Ok(())).unwrap();
                assert_eq!(done.verified_bytes, CHUNK_BYTES as u64 + 17);
                assert_eq!(done.resumed_bytes, 0);
                let output = destination.join(source.file_name().unwrap());
                let file = if directory {
                    output.join("дані.bin")
                } else {
                    output.clone()
                };
                assert_eq!(fs::read(file).unwrap(), vec![71; CHUNK_BYTES + 17]);
                if directory {
                    assert!(output.join("порожня").is_dir());
                    assert_eq!(fs::metadata(output.join("нуль.txt")).unwrap().len(), 0);
                }
                let count = journal.counts(id).unwrap();
                assert_eq!(count, (if directory { 4 } else { 1 }, 0));
                if old_exists {
                    assert!(old.stage.exists(), "an old private stage was removed");
                    assert!(!done.issues.is_empty());
                }
                execute(&mut journal, id, &mut |_| Ok(())).unwrap(); // Read-only idempotent recheck.
            }
        }
    }

    #[test]
    fn replacement_stage_is_retained_and_corrupt_published_output_is_never_rewritten() {
        let (_root, source, destination, path, id) = fixture(false);
        let mut journal = Journal::open(&path).unwrap();
        assert!(
            execute(
                &mut journal,
                id,
                &mut |p| if p.phase == Phase::StageRecorded {
                    Err(Error::Cancelled)
                } else {
                    Ok(())
                }
            )
            .is_err()
        );
        let stage = journal.fat_attempt(id).unwrap().unwrap().stage;
        fs::remove_file(&stage).unwrap();
        fs::write(&stage, b"foreign replacement must stay").unwrap();
        execute(&mut journal, id, &mut |_| Ok(())).unwrap();
        assert_eq!(fs::read(&stage).unwrap(), b"foreign replacement must stay");
        let output = destination.join(source.file_name().unwrap());
        fs::write(&output, b"changed destination").unwrap();
        assert!(execute(&mut journal, id, &mut |_| Ok(())).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"changed destination");
        assert_eq!(fs::read(&stage).unwrap(), b"foreign replacement must stay");
    }

    #[test]
    fn published_tree_reconciles_without_source_and_rejects_unrecorded_children() {
        let (_root, source, destination, path, id) = fixture(true);
        let mut journal = Journal::open(&path).unwrap();
        assert!(
            execute(&mut journal, id, &mut |p| if p.phase == Phase::Published {
                Err(Error::Cancelled)
            } else {
                Ok(())
            })
            .is_err()
        );
        fs::rename(&source, source.with_extension("moved-source")).unwrap();
        execute(&mut journal, id, &mut |_| Ok(())).unwrap();
        let foreign = destination
            .join(source.file_name().unwrap())
            .join("unrecorded.txt");
        fs::write(&foreign, b"keep unrelated data").unwrap();
        assert!(execute(&mut journal, id, &mut |_| Ok(())).is_err());
        assert_eq!(fs::read(&foreign).unwrap(), b"keep unrelated data");
    }

    #[test]
    fn restart_rehashes_previous_source_checkpoints_and_exports_retained_attempts() {
        let (root, source, _destination, path, id) = fixture(false);
        let mut journal = Journal::open(&path).unwrap();
        assert!(
            execute(
                &mut journal,
                id,
                &mut |p| if p.phase == Phase::CheckpointCommitted {
                    Err(Error::Cancelled)
                } else {
                    Ok(())
                }
            )
            .is_err()
        );
        let attempt = journal.fat_attempt(id).unwrap().unwrap();
        let modified = fs::metadata(&source).unwrap().modified().unwrap();
        let mut replacement = vec![71; CHUNK_BYTES + 17];
        replacement[0] ^= 1;
        fs::write(&source, &replacement).unwrap();
        fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert!(execute(&mut journal, id, &mut |_| Ok(())).is_err());
        assert_eq!(journal.fat_attempt(id).unwrap().unwrap().id, attempt.id);
        assert!(attempt.stage.exists());
        fs::write(&source, vec![71; CHUNK_BYTES + 17]).unwrap();
        fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        execute(&mut journal, id, &mut |_| Ok(())).unwrap();
        let export = root.path().join("fat-report.json");
        journal.export_report(id, &export).unwrap();
        let report: serde_json::Value = serde_json::from_slice(&fs::read(export).unwrap()).unwrap();
        assert_eq!(report["fat_attempts"][0]["state"], "abandoned");
        assert_eq!(report["fat_attempts"][1]["state"], "committed");
        assert_eq!(
            report["fat_attempts"][0]["old_stage_write_or_delete_authority"],
            false
        );
        assert!(attempt.stage.exists());
    }

    #[test]
    fn conflicts_never_merge_or_replace_and_matching_foreign_output_is_not_a_publication() {
        let (_root, source, destination, path, id) = fixture(false);
        let mut journal = Journal::open(&path).unwrap();
        let target = destination.join(source.file_name().unwrap());
        let original = fs::read(&source).unwrap();
        let mut injected = false;
        assert!(
            execute(&mut journal, id, &mut |p| {
                if p.phase == Phase::PublishIntent && !injected {
                    injected = true;
                    fs::write(&target, &original)?;
                }
                Ok(())
            })
            .is_err()
        );
        assert!(execute(&mut journal, id, &mut |_| Ok(())).is_err());
        assert_eq!(journal.counts(id).unwrap().0, 0);
        assert_eq!(fs::read(&target).unwrap(), original);
        for conflict in [Conflict::Skip, Conflict::KeepBoth] {
            let plan = planner::scan(
                &source,
                &destination,
                ScanOptions::default(),
                &AtomicBool::new(false),
            )
            .unwrap();
            let next = journal.enqueue(&plan, conflict).unwrap();
            execute(&mut journal, next, &mut |_| Ok(())).unwrap();
            assert_eq!(fs::read(&target).unwrap(), original);
            assert_eq!(
                journal.counts(next).unwrap(),
                if matches!(conflict, Conflict::Skip) {
                    (0, 1)
                } else {
                    (1, 0)
                }
            );
        }
        assert_eq!(
            fs::read_dir(&destination)
                .unwrap()
                .map(|p| p.unwrap())
                .filter(|p| !p.file_name().to_string_lossy().starts_with(".vartacopy-"))
                .count(),
            2
        );
    }
}
