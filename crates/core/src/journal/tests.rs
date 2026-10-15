use super::*;
use crate::planner::{ScanOptions, scan};
use std::ffi::OsString;
use std::fs;
use std::sync::atomic::AtomicBool;

fn fixture() -> (tempfile::TempDir, OperationPlan) {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    fs::write(&source, b"data").unwrap();
    fs::create_dir(&destination).unwrap();
    let plan = scan(
        &source,
        &destination,
        ScanOptions::default(),
        &AtomicBool::new(false),
    )
    .unwrap();
    (root, plan)
}

#[test]
fn atomic_initialization_enqueue_reopen_and_pragmas() {
    let (root, plan) = fixture();
    let path = root.path().join("journal.sqlite");
    let id;
    {
        let mut journal = Journal::open(&path).unwrap();
        assert_eq!(
            journal
                .db
                .pragma_query_value(None, "journal_mode", |r| r.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
        assert_eq!(
            journal
                .db
                .pragma_query_value(None, "synchronous", |r| r.get::<_, i32>(0))
                .unwrap(),
            2
        );
        id = journal.enqueue(&plan, Conflict::KeepBoth).unwrap();
        assert_eq!(journal.job(id).unwrap().entry_count, 1);
        journal.flush().unwrap();
    }
    let journal = Journal::open(&path).unwrap();
    let job = journal.job(id).unwrap();
    assert_eq!(job.source, plan.source);
    assert_eq!(job.destination, plan.destination);
    assert_eq!(job.status, "queued");
    assert_eq!(journal.entry(id, 0).unwrap().state, EntryState::Planned);
    assert_eq!(journal.jobs(256, 0).unwrap().len(), 1);
    assert!(journal.jobs(257, 0).is_err());
}

#[test]
fn unrelated_and_future_databases_are_not_modified() {
    let (root, _) = fixture();
    let unrelated = root.path().join("unrelated.sqlite");
    {
        let db = Connection::open(&unrelated).unwrap();
        db.execute_batch("CREATE TABLE personal(data TEXT); INSERT INTO personal VALUES('keep');")
            .unwrap();
    }
    let before = fs::read(&unrelated).unwrap();
    assert!(Journal::open(&unrelated).is_err());
    assert_eq!(fs::read(&unrelated).unwrap(), before);
    let future = root.path().join("future.sqlite");
    let journal = Journal::open(&future).unwrap();
    assert!(journal.db.is_autocommit());
    journal.flush().unwrap();
    drop(journal);
    {
        let db = Connection::open(&future).unwrap();
        db.pragma_update(None, "user_version", 999).unwrap();
    }
    let before = fs::read(&future).unwrap();
    assert!(Journal::open(&future).is_err());
    assert_eq!(fs::read(&future).unwrap(), before);
}

#[test]
fn writer_lease_and_database_hardlink_are_refused() {
    let (root, _) = fixture();
    let path = root.path().join("journal.sqlite");
    let journal = Journal::open(&path).unwrap();
    assert!(Journal::open(&path).is_err());
    drop(journal);
    fs::hard_link(&path, root.path().join("alias.sqlite")).unwrap();
    assert!(Journal::open(&path).is_err());
}

#[test]
fn utf16_blob_and_snapshot_preserve_unpaired_surrogates_and_early_times() {
    let (root, _) = fixture();
    let source = root
        .path()
        .join(OsString::from_wide(&[b'x' as u16, 0xD800]));
    fs::write(&source, b"content").unwrap();
    let early = std::time::UNIX_EPOCH - std::time::Duration::from_secs(86400);
    File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_modified(early)
        .unwrap();
    let plan = scan(
        &source,
        &root.path().join("destination"),
        ScanOptions::default(),
        &AtomicBool::new(false),
    )
    .unwrap();
    let mut journal = Journal::open(&root.path().join("journal.sqlite")).unwrap();
    let id = journal.enqueue(&plan, Conflict::Error).unwrap();
    assert_eq!(journal.job(id).unwrap().source, plan.source);
    let entry = journal.entry(id, 0).unwrap();
    entry
        .snapshot
        .unwrap()
        .validate(&platform::open_source(&source).unwrap(), &source, false)
        .unwrap();
    assert!(decode_path(vec![0, 0], false).is_err());
    assert!(decode_path(vec![1], false).is_err());
    assert!(decode_path(encode_path(Path::new("../outside")), true).is_err());
}

#[test]
fn checkpoints_require_valid_states_and_contiguous_offsets() {
    let (root, plan) = fixture();
    let mut journal = Journal::open(&root.path().join("journal.sqlite")).unwrap();
    let id = journal.enqueue(&plan, Conflict::Error).unwrap();
    let mut stage = platform::Stage::create(&root.path().join("destination")).unwrap();
    let identity = platform::identity(stage.file()).unwrap();
    journal
        .record_stage(id, 0, stage.path(), &identity)
        .unwrap();
    stage.preserve();
    let chunk = Chunk {
        offset: 0,
        length: 4,
        hash: *blake3::hash(b"data").as_bytes(),
    };
    journal.checkpoint(id, 0, 0, &chunk).unwrap();
    assert!(journal.checkpoint(id, 0, 0, &chunk).is_err());
    let gap = Chunk {
        offset: 8,
        ..chunk.clone()
    };
    assert!(journal.checkpoint(id, 0, 0, &gap).is_err());
    assert!(
        journal
            .transition(id, 0, EntryState::Copying, EntryState::Committed)
            .is_err()
    );
    journal
        .transition(id, 0, EntryState::Copying, EntryState::Verified)
        .unwrap();
    assert!(
        journal
            .checkpoint(id, 0, 0, &Chunk { offset: 4, ..chunk })
            .is_err()
    );
    journal
        .publish_intent(id, 0, &root.path().join("destination/source"))
        .unwrap();
    journal.commit_file(id, 0, &identity).unwrap();
    assert_eq!(journal.entry(id, 0).unwrap().state, EntryState::Committed);
    assert_eq!(journal.chunks(id, 0, 0).unwrap().len(), 1);
    stage.discard().unwrap();
}

#[test]
fn corrupt_checkpoint_chain_fails_closed() {
    let (root, plan) = fixture();
    let mut journal = Journal::open(&root.path().join("journal.sqlite")).unwrap();
    let id = journal.enqueue(&plan, Conflict::Error).unwrap();
    let mut stage = platform::Stage::create(&root.path().join("destination")).unwrap();
    let identity = platform::identity(stage.file()).unwrap();
    journal
        .record_stage(id, 0, stage.path(), &identity)
        .unwrap();
    let chunk = Chunk {
        offset: 0,
        length: 4,
        hash: [7; 32],
    };
    journal.checkpoint(id, 0, 0, &chunk).unwrap();
    journal
        .db
        .execute("UPDATE chunks SET offset=4 WHERE job_id=?1", [id])
        .unwrap();
    assert!(journal.chunks(id, 0, 0).is_err());
}

#[test]
fn preserved_stage_can_be_reopened_only_by_recorded_identity() {
    let (root, _) = fixture();
    let mut stage = platform::Stage::create(root.path()).unwrap();
    stage.file().write_all(b"owned").unwrap();
    let identity = platform::identity(stage.file()).unwrap();
    let path = stage.path().to_path_buf();
    stage.preserve();
    drop(stage);
    let original = path.with_extension("original");
    fs::rename(&path, &original).unwrap();
    fs::write(&path, b"foreign").unwrap();
    assert!(platform::Stage::reopen(&path, &identity).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"foreign");
    let mut owned = platform::Stage::reopen(&original, &identity).unwrap();
    owned.discard().unwrap();
}

#[test]
fn partial_plan_insert_rolls_back() {
    let (root, mut plan) = fixture();
    let mut journal = Journal::open(&root.path().join("journal.sqlite")).unwrap();
    plan.entries[0].relative = PathBuf::from("a".repeat(40_000));
    assert!(journal.enqueue(&plan, Conflict::Error).is_err());
    assert!(journal.jobs(256, 0).unwrap().is_empty());
}

#[test]
fn peer_retains_exclusive_process_lease_and_queue_writes_while_copy_waits() {
    let (root, plan) = fixture();
    let path = root.path().join("jobs.sqlite");
    let mut journal = Journal::open(&path).unwrap();
    let first = journal.enqueue(&plan, Conflict::KeepBoth).unwrap();
    let mut peer = journal.peer().unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut stopped = false;
        crate::recovery::run(
            &mut peer,
            first,
            &crate::control::Control::default(),
            &mut |event| {
                if !stopped && event.phase == crate::recovery::Phase::StageRecorded {
                    stopped = true;
                    ready_tx.send(()).unwrap();
                    go_rx.recv().unwrap();
                }
                Ok(())
            },
        )
        .unwrap()
    });
    ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let second = journal.enqueue(&plan, Conflict::KeepBoth).unwrap();
    let third = journal.enqueue(&plan, Conflict::KeepBoth).unwrap();
    journal.move_queued(third, true).unwrap();
    assert_eq!(
        journal
            .queue(256, 0)
            .unwrap()
            .iter()
            .map(|j| j.id)
            .collect::<Vec<_>>(),
        vec![third, second]
    );
    assert!(journal.schedule(first).is_err());
    drop(journal);
    assert!(Journal::open(&path).is_err(), "peer still owns the lease");
    go_tx.send(()).unwrap();
    assert_eq!(worker.join().unwrap().status, "completed");
    let journal = Journal::open(&path).unwrap();
    assert_eq!(journal.queue(256, 0).unwrap()[0].id, third);
}

#[test]
fn migrates_version_one_atomically_and_recovers_running_jobs() {
    let (root, plan) = fixture();
    let path = root.path().join("jobs.sqlite");
    let id;
    {
        let mut journal = Journal::open(&path).unwrap();
        id = journal.enqueue(&plan, Conflict::Error).unwrap();
        journal.set_job_status(id, "running", None).unwrap();
        journal
            .db
            .execute_batch("ALTER TABLE jobs DROP COLUMN queue_position; PRAGMA user_version=1;")
            .unwrap();
        journal.flush().unwrap();
    }
    let mut journal = Journal::open(&path).unwrap();
    assert_eq!(
        journal
            .db
            .pragma_query_value(None, "user_version", |r| r.get::<_, i32>(0))
            .unwrap(),
        VERSION
    );
    assert_eq!(journal.recover_running_jobs().unwrap(), 1);
    assert_eq!(journal.job(id).unwrap().status, "interrupted");
    journal.schedule(id).unwrap();
    let second = journal.enqueue(&plan, Conflict::KeepBoth).unwrap();
    assert_eq!(journal.queue(1, 0).unwrap()[0].id, id);
    journal.move_queued(id, false).unwrap();
    assert_eq!(journal.queue(1, 0).unwrap()[0].id, second);
    assert!(journal.queue(257, 0).is_err());
}

#[test]
fn peer_flush_is_durable_while_owner_retains_an_old_read_snapshot() {
    let (root, plan) = fixture();
    let path = root.path().join("jobs.sqlite");
    let mut owner = Journal::open(&path).unwrap();
    let id = owner.enqueue(&plan, Conflict::Error).unwrap();
    let mut peer = owner.peer().unwrap();
    owner.db.execute_batch("BEGIN DEFERRED;").unwrap();
    assert_eq!(owner.job(id).unwrap().status, "queued");
    let report = crate::recovery::run(
        &mut peer,
        id,
        &crate::control::Control::default(),
        &mut |_| Ok(()),
    )
    .unwrap();
    assert_eq!(report.status, "completed");
    assert_eq!(owner.job(id).unwrap().status, "queued");
    owner.db.execute_batch("COMMIT;").unwrap();
    assert_eq!(owner.job(id).unwrap().status, "completed");
    drop(peer);
    owner.flush().unwrap();
    drop(owner);
    assert_eq!(
        Journal::open(&path).unwrap().job(id).unwrap().status,
        "completed"
    );
}

#[test]
fn report_streams_all_issue_pages_and_never_replaces_existing_output() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("tree");
    let dest = root.path().join("dest");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&dest).unwrap();
    for i in 0..300 {
        fs::write(source.join(format!("file-{i}")), b"data").unwrap();
    }
    let plan = scan(
        &source,
        &dest,
        ScanOptions::default(),
        &AtomicBool::new(false),
    )
    .unwrap();
    let mut journal = Journal::open(&root.path().join("jobs.sqlite")).unwrap();
    let id = journal.enqueue(&plan, Conflict::Error).unwrap();
    for i in 1..=300 {
        journal.entry_error(id, i, "example failure").unwrap();
    }
    let first = journal.details(id, 256, 0).unwrap();
    assert_eq!(first.issues.len(), 256);
    assert!(first.has_more);
    let second = journal.details(id, 256, 256).unwrap();
    assert_eq!(second.issues.len(), 44);
    assert!(!second.has_more);
    let output = root.path().join("report.json");
    journal.export_report(id, &output).unwrap();
    let data = fs::read(&output).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&data).unwrap();
    assert_eq!(json["issues"].as_array().unwrap().len(), 300);
    assert_eq!(json["summary"]["entries_pending"], 301);
    assert!(json["issues"][0]["relative"]["windows_utf16"].is_array());
    assert!(journal.export_report(id, &output).is_err());
    assert_eq!(fs::read(output).unwrap(), data);
    assert!(journal.details(id, 257, 0).is_err());
    journal.hold(id, false).unwrap();
    assert_eq!(journal.job(id).unwrap().status, "paused");
    journal.schedule(id).unwrap();
    journal.hold(id, true).unwrap();
    assert_eq!(journal.job(id).unwrap().status, "cancelled");
    assert!(journal.recheck(id).is_err());
}

#[test]
fn file_table_pages_preserve_native_names_stream_sizes_states_and_errors() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("tree");
    let dest = root.path().join("dest");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&dest).unwrap();
    fs::create_dir(source.join("empty")).unwrap();
    for i in 0..130 {
        fs::write(source.join(format!("file-{i:03}")), b"data").unwrap();
    }
    fs::write(source.join("file-000:note"), b"ADS").unwrap();
    let plan = scan(
        &source,
        &dest,
        ScanOptions::default(),
        &AtomicBool::new(false),
    )
    .unwrap();
    let mut journal = Journal::open(&root.path().join("jobs.sqlite")).unwrap();
    let id = journal.enqueue(&plan, Conflict::Error).unwrap();
    let native = PathBuf::from(OsString::from_wide(&[b'x' as u16, 0xD800]));
    journal
        .db
        .execute(
            "UPDATE entries SET relative=?1 WHERE job_id=?2 AND ordinal=131",
            params![encode_path(&native), id],
        )
        .unwrap();
    journal.entry_error(id, 131, "kept failure").unwrap();
    let first = journal.entry_page(id, 128, 0).unwrap();
    assert_eq!(first.total, 132);
    assert_eq!(first.entries.len(), 128);
    assert!(first.has_more);
    assert_eq!(first.entries[0].kind, EntryKind::Directory);
    assert_eq!(first.entries[0].bytes, None);
    assert!(
        first
            .entries
            .iter()
            .any(|e| e.relative == Path::new("file-000") && e.bytes == Some(7))
    );
    let last = journal.entry_page(id, 128, 128).unwrap();
    assert_eq!(last.entries.len(), 4);
    assert!(!last.has_more);
    assert_eq!(last.entries[3].ordinal, 131);
    assert_eq!(last.entries[3].relative, native);
    assert_eq!(last.entries[3].error.as_deref(), Some("kept failure"));
    assert_eq!(last.entries[3].state, EntryState::Planned);
    let past = journal.entry_page(id, 64, 132).unwrap();
    assert!(past.entries.is_empty());
    assert!(!past.has_more);
    assert!(journal.entry_page(id, 0, 0).is_err());
    assert!(journal.entry_page(id, 129, 0).is_err());
    assert!(journal.entry_page(id, 64, usize::MAX).is_err());
    assert!(journal.entry_page(id + 1, 64, 0).is_err());
}
