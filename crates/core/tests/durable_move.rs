use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
use vartacopy_core::{
    Conflict, Error,
    control::Control,
    journal::{Journal, OperationKind},
    planner::{ScanOptions, scan},
    recovery::{self, Phase},
};
use vartacopy_platform_windows as platform;

struct Fixture {
    root: tempfile::TempDir,
    source: PathBuf,
    destination: PathBuf,
    db: PathBuf,
}
impl Fixture {
    fn new(tree: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        if tree {
            fs::create_dir_all(source.join("nested/empty")).unwrap();
            fs::write(source.join("nested/file"), b"payload").unwrap();
        } else {
            fs::write(&source, b"payload").unwrap();
        }
        let file = if tree {
            source.join("nested/file")
        } else {
            source.clone()
        };
        fs::write(platform::stream_path(&file, ":ads".as_ref()), b"alternate").unwrap();
        let destination = root.path().join("destination");
        fs::create_dir(&destination).unwrap();
        let db = root.path().join("journal.sqlite");
        Self {
            root,
            source,
            destination,
            db,
        }
    }
    fn enqueue(&self, conflict: Conflict) -> (Journal, i64) {
        let plan = scan(
            &self.source,
            &self.destination,
            ScanOptions::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let mut journal = Journal::open(&self.db).unwrap();
        let id = journal
            .enqueue_with_operation(&plan, conflict, OperationKind::Move)
            .unwrap();
        assert_eq!(journal.job(id).unwrap().operation, OperationKind::Move);
        (journal, id)
    }
    fn target(&self) -> PathBuf {
        self.destination.join("source")
    }
}

#[test]
fn move_cleanup_recovery_is_idempotent_and_retains_ambiguous_deleted_identity() {
    for tree in [false, true] {
        for phase in [
            Phase::CleanupIntent,
            Phase::SourceDeletePrepared,
            Phase::SourceRemoved,
            Phase::CleanupCommitted,
        ] {
            let f = Fixture::new(tree);
            let (mut journal, id) = f.enqueue(Conflict::Error);
            let mut hit = false;
            let mut stopped_entry = 0;
            let report = recovery::run(&mut journal, id, &Control::default(), &mut |p| {
                if p.phase == phase && !hit {
                    hit = true;
                    stopped_entry = p.entry;
                    return Err(Error::Journal("injected cleanup crash".into()));
                }
                Ok(())
            })
            .unwrap();
            assert!(hit, "{phase:?} {tree} {report:?}");
            assert_eq!(report.status, "partial", "{report:?}");
            drop(journal);
            let mut journal = Journal::open(&f.db).unwrap();
            let stopped = journal.entry(id, stopped_entry).unwrap();
            let ambiguous = phase == Phase::SourceRemoved
                && platform::identity_exists_on_source_volume(
                    &f.source.join(stopped.relative),
                    &stopped.snapshot.unwrap().identity,
                )
                .is_err();
            for _ in 0..2 {
                let report =
                    recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
                assert_eq!(
                    report.status,
                    if ambiguous { "partial" } else { "completed" },
                    "{phase:?} {tree} {report:?}"
                );
                if ambiguous {
                    assert!(report.entries_pending > 0);
                    assert!(report.error.is_some());
                }
            }
            if !ambiguous {
                assert!(!f.source.exists());
            }
            let target = if tree {
                f.target().join("nested/file")
            } else {
                f.target()
            };
            assert_eq!(fs::read(&target).unwrap(), b"payload");
            assert_eq!(
                fs::read(platform::stream_path(&target, ":ads".as_ref())).unwrap(),
                b"alternate"
            );
        }
    }
}

fn interrupt_before_cleanup(f: &Fixture) -> (Journal, i64) {
    let (mut journal, id) = f.enqueue(Conflict::Error);
    let report = recovery::run(&mut journal, id, &Control::default(), &mut |p| {
        if p.phase == Phase::Committed {
            return Err(Error::Journal("injected after copy".into()));
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(report.status, "partial");
    (journal, id)
}

#[test]
fn replaced_sources_destinations_and_changed_ads_are_preserved() {
    for victim in ["source", "destination", "ads"] {
        let f = Fixture::new(false);
        let (mut journal, id) = interrupt_before_cleanup(&f);
        match victim {
            "source" => {
                fs::rename(&f.source, f.root.path().join("original")).unwrap();
                fs::write(&f.source, b"foreign").unwrap();
            }
            "destination" => {
                fs::rename(f.target(), f.root.path().join("original")).unwrap();
                fs::write(f.target(), b"foreign").unwrap();
            }
            _ => {
                fs::write(
                    platform::stream_path(&f.source, ":ads".as_ref()),
                    b"different",
                )
                .unwrap();
            }
        }
        let report = recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
        assert_eq!(report.status, "partial", "{victim}: {report:?}");
        assert!(f.source.exists());
        assert!(report.error.is_some());
    }
}

#[test]
fn readonly_failure_and_skip_retain_source() {
    for skip in [false, true] {
        let f = Fixture::new(false);
        if skip {
            fs::write(f.target(), b"existing").unwrap();
        } else {
            let mut permissions = fs::metadata(&f.source).unwrap().permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&f.source, permissions).unwrap();
        }
        let (mut journal, id) = f.enqueue(if skip {
            Conflict::Skip
        } else {
            Conflict::Error
        });
        let report = recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
        assert_eq!(report.status, "partial", "{report:?}");
        assert!(f.source.exists());
        if !skip {
            assert!(fs::metadata(&f.source).unwrap().permissions().readonly());
        }
    }
}

#[test]
fn late_child_is_not_deleted_and_cancel_after_intent_stops_removal() {
    let f = Fixture::new(true);
    let (mut journal, id) = f.enqueue(Conflict::Error);
    let mut added = false;
    let report = recovery::run(&mut journal, id, &Control::default(), &mut |p| {
        if p.phase == Phase::CleanupIntent && !added {
            fs::write(f.source.join("late"), b"late").unwrap();
            added = true;
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(report.status, "partial", "{report:?}");
    assert_eq!(fs::read(f.source.join("late")).unwrap(), b"late");
    let f = Fixture::new(false);
    let (mut journal, id) = f.enqueue(Conflict::Error);
    let control = Control::default();
    let report = recovery::run(&mut journal, id, &control, &mut |p| {
        if p.phase == Phase::CleanupIntent {
            control.cancel();
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(report.status, "cancelled");
    assert!(f.source.exists());
    assert!(f.target().exists());
}

#[test]
fn source_cleanup_owner_rejects_writes_rename_and_nonempty_directory() {
    let f = Fixture::new(false);
    let identity = platform::identity(&platform::open_source(&f.source).unwrap()).unwrap();
    let owner = platform::CleanupOwner::open(&f.source, &identity, false, true).unwrap();
    assert!(fs::write(&f.source, b"changed").is_err());
    assert!(fs::rename(&f.source, f.root.path().join("renamed")).is_err());
    assert!(
        fs::write(
            platform::stream_path(&f.source, ":ads".as_ref()),
            b"changed"
        )
        .is_err()
    );
    assert!(fs::write(platform::stream_path(&f.source, ":new".as_ref()), b"new").is_err());
    drop(owner);
    // Guard teardown rolls back and releases the whole-file lock without
    // changing either stream or source attributes.
    assert_eq!(fs::read(&f.source).unwrap(), b"payload");
    assert_eq!(
        fs::read(platform::stream_path(&f.source, ":ads".as_ref())).unwrap(),
        b"alternate"
    );
    fs::write(platform::stream_path(&f.source, ":new".as_ref()), b"new").unwrap();
    let path: &Path = f.destination.as_ref();
    fs::write(path.join("late"), b"late").unwrap();
    let identity = platform::identity(&platform::open_directory(path, false).unwrap()).unwrap();
    let mut owner = platform::CleanupOwner::open(path, &identity, true, true).unwrap();
    assert!(owner.prepare_removal(&identity).is_err());
    assert!(path.join("late").exists());
}

#[test]
fn destination_all_streams_remain_frozen_until_source_commit() {
    let f = Fixture::new(false);
    let (mut journal, id) = f.enqueue(Conflict::Error);
    let mut hit = false;
    let report = recovery::run(&mut journal, id, &Control::default(), &mut |p| {
        if p.phase == Phase::CleanupIntent {
            hit = true;
            assert!(fs::write(f.target(), b"changed").is_err());
            assert!(fs::rename(f.target(), f.root.path().join("replaced")).is_err());
            assert!(
                fs::write(
                    platform::stream_path(&f.target(), ":ads".as_ref()),
                    b"changed"
                )
                .is_err()
            );
            assert!(
                fs::write(platform::stream_path(&f.target(), ":new".as_ref()), b"new").is_err()
            );
            assert!(fs::write(platform::stream_path(&f.source, ":new".as_ref()), b"new").is_err());
        }
        Ok(())
    })
    .unwrap();
    assert!(hit);
    assert_eq!(report.status, "completed", "{report:?}");
    assert!(!f.source.exists());
}

#[test]
fn lost_destination_and_source_recreation_after_intent_never_delete_source() {
    for lost_destination in [false, true] {
        let f = Fixture::new(false);
        let (mut journal, id) = f.enqueue(Conflict::Error);
        let report = recovery::run(&mut journal, id, &Control::default(), &mut |p| {
            if p.phase == Phase::CleanupIntent {
                return Err(Error::Journal("injected intent crash".into()));
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(report.status, "partial");
        if lost_destination {
            fs::remove_file(f.target()).unwrap();
        } else {
            fs::rename(&f.source, f.root.path().join("original")).unwrap();
            fs::write(&f.source, b"new source").unwrap();
        }
        let before = fs::read(&f.source).unwrap();
        let report = recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
        assert_eq!(report.status, "partial", "{report:?}");
        assert_eq!(fs::read(&f.source).unwrap(), before);
    }
}

#[test]
fn transacted_disposition_rolls_back_until_explicit_commit() {
    let f = Fixture::new(false);
    let identity = platform::identity(&platform::open_source(&f.source).unwrap()).unwrap();
    {
        let mut owner = platform::CleanupOwner::open(&f.source, &identity, false, true).unwrap();
        owner.prepare_removal(&identity).unwrap();
        // Emulate failure between disposition and transaction commit.
        drop(owner);
    }
    assert_eq!(fs::read(&f.source).unwrap(), b"payload");
    assert_eq!(
        fs::read(platform::stream_path(&f.source, ":ads".as_ref())).unwrap(),
        b"alternate"
    );
    let mut owner = platform::CleanupOwner::open(&f.source, &identity, false, true).unwrap();
    owner.prepare_removal(&identity).unwrap();
    owner.commit_removal().unwrap();
    assert!(!f.source.exists());
}

#[test]
fn renamed_source_after_intent_is_partial_with_durable_issue_and_pending_cleanup() {
    for tree in [false, true] {
        let f = Fixture::new(tree);
        let (mut journal, id) = f.enqueue(Conflict::Error);
        let mut ordinal = None;
        let report = recovery::run(&mut journal, id, &Control::default(), &mut |p| {
            if p.phase == Phase::CleanupIntent {
                ordinal = Some(p.entry);
                return Err(Error::Journal("intent interruption".into()));
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(report.status, "partial");
        let renamed = f.root.path().join("renamed-source");
        fs::rename(&f.source, &renamed).unwrap();
        drop(journal);
        let mut journal = Journal::open(&f.db).unwrap();
        let report = recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
        assert_eq!(report.status, "partial", "{tree}: {report:?}");
        assert!(!report.issues.is_empty());
        assert!(renamed.exists());
        assert!(!f.source.exists());
        let details = journal.details(id, 256, 0).unwrap();
        assert!(details.entries_pending > 0, "{details:?}");
        assert_eq!(details.entries_pending, report.entries_pending);
        assert_eq!(details.entries_committed, details.job.entry_count);
        assert!(!details.issues.is_empty(), "{details:?}");
        assert!(
            details
                .issues
                .iter()
                .any(|issue| issue.message.contains("identity remains live")),
            "{details:?}"
        );
        assert!(ordinal.is_some());
        assert!(
            journal
                .entry_page(id, 128, 0)
                .unwrap()
                .entries
                .iter()
                .any(|entry| entry
                    .error
                    .as_ref()
                    .is_some_and(|error| error.contains("identity remains live")))
        );
    }
}

#[test]
fn cleanup_tree_issue_identifies_exact_file_ordinal_and_survives_reopen() {
    let f = Fixture::new(true);
    let (mut journal, id) = f.enqueue(Conflict::Error);
    let mut failed_ordinal = None;
    let report = recovery::run(&mut journal, id, &Control::default(), &mut |p| {
        if p.phase == Phase::CleanupIntent && p.entry > 0 {
            // Stop at a child-directory intent; no source has been removed yet.
            return Err(Error::Journal("intent interruption".into()));
        }
        if p.phase == Phase::Committed {
            failed_ordinal = Some(p.entry);
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(report.status, "partial");
    let file = f.source.join("nested/file");
    let previous_time = fs::metadata(&file).unwrap().modified().unwrap();
    fs::write(&file, b"changed").unwrap();
    fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(previous_time)
        .unwrap();
    let report = recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "partial", "{report:?}");
    drop(journal);
    let journal = Journal::open(&f.db).unwrap();
    let details = journal.details(id, 256, 0).unwrap();
    let issue = details
        .issues
        .iter()
        .find(|issue| issue.relative == Path::new("nested/file"))
        .expect("exact file cleanup issue");
    assert!(report.issues.iter().any(|problem| {
        problem
            .message
            .contains(&format!("entry {}", issue.ordinal))
    }));
    assert_eq!(Some(issue.ordinal), failed_ordinal);
    assert!(
        issue.message.contains("cleanup source bytes changed"),
        "{issue:?}"
    );
    assert_eq!(details.entries_pending, report.entries_pending);
    assert_eq!(details.entries_committed, details.job.entry_count);
}

#[test]
fn move_recheck_rejects_without_scheduling_or_changing_source_and_job() {
    let f = Fixture::new(false);
    let (mut journal, id) = interrupt_before_cleanup(&f);
    let before = journal.job(id).unwrap();
    assert_eq!(before.status, "partial");
    let error = journal.recheck(id).unwrap_err();
    assert!(error.to_string().contains("Move jobs cannot be rechecked"));
    let after = journal.job(id).unwrap();
    assert_eq!(after.status, before.status);
    assert_eq!(after.last_error, before.last_error);
    assert!(journal.queue(256, 0).unwrap().is_empty());
    assert_eq!(fs::read(&f.source).unwrap(), b"payload");
    assert_eq!(fs::read(f.target()).unwrap(), b"payload");
    assert_eq!(journal.cleanup_state(id, 0).unwrap(), None);
    drop(journal);
    let journal = Journal::open(&f.db).unwrap();
    assert_eq!(journal.job(id).unwrap().status, "partial");
    assert!(journal.queue(256, 0).unwrap().is_empty());
}

#[test]
fn explicit_schedule_resumes_partial_move_durably_and_rejects_partial_copy() {
    let f = Fixture::new(false);
    let (mut journal, id) = interrupt_before_cleanup(&f);
    assert!(f.source.exists());
    journal.schedule(id).unwrap();
    assert_eq!(journal.job(id).unwrap().status, "queued");
    assert_eq!(journal.job(id).unwrap().last_error, None);
    assert_eq!(journal.queue(256, 0).unwrap()[0].id, id);
    drop(journal);
    let mut journal = Journal::open(&f.db).unwrap();
    assert_eq!(journal.job(id).unwrap().operation, OperationKind::Move);
    assert_eq!(journal.queue(256, 0).unwrap()[0].id, id);
    let report = recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "completed", "{report:?}");
    assert!(!f.source.exists());
    assert_eq!(fs::read(f.target()).unwrap(), b"payload");
    assert_eq!(
        fs::read(platform::stream_path(&f.target(), ":ads".as_ref())).unwrap(),
        b"alternate"
    );

    let copy = Fixture::new(false);
    fs::write(copy.target(), b"foreign destination").unwrap();
    let plan = scan(
        &copy.source,
        &copy.destination,
        ScanOptions::default(),
        &AtomicBool::new(false),
    )
    .unwrap();
    let mut journal = Journal::open(&copy.db).unwrap();
    let id = journal.enqueue(&plan, Conflict::Skip).unwrap();
    let report = recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "partial");
    let before = journal.job(id).unwrap();
    assert_eq!(before.operation, OperationKind::Copy);
    assert!(journal.schedule(id).is_err());
    assert_eq!(journal.job(id).unwrap().status, before.status);
    assert_eq!(journal.job(id).unwrap().last_error, before.last_error);
    assert!(journal.queue(256, 0).unwrap().is_empty());
    assert_eq!(fs::read(&copy.source).unwrap(), b"payload");
    assert_eq!(fs::read(copy.target()).unwrap(), b"foreign destination");
    drop(journal);
    let journal = Journal::open(&copy.db).unwrap();
    assert_eq!(journal.job(id).unwrap().status, "partial");
    assert!(journal.queue(256, 0).unwrap().is_empty());
}
