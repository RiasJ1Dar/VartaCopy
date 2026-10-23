use super::*;
use crate::{
    control::Control,
    planner::{self, ScanOptions},
    recovery::{self, Phase},
};
use std::{fs, sync::atomic::AtomicBool};

fn fixture() -> (tempfile::TempDir, Vec<OperationPlan>) {
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("dest");
    fs::create_dir(&dest).unwrap();
    let sources = [
        root.path().join("a.txt"),
        root.path().join("b.txt"),
        root.path().join("дерево-😀"),
    ];
    fs::write(&sources[0], b"one").unwrap();
    fs::write(&sources[1], b"two").unwrap();
    fs::create_dir(&sources[2]).unwrap();
    fs::create_dir(sources[2].join("empty")).unwrap();
    fs::write(sources[2].join("дані.txt"), b"three").unwrap();
    let plans = sources
        .iter()
        .map(|p| planner::scan(p, &dest, ScanOptions::default(), &AtomicBool::new(false)).unwrap())
        .collect();
    (root, plans)
}

#[test]
fn many_roots_have_one_persisted_operation_and_resume_never_restarts_finished_roots() {
    let (root, plans) = fixture();
    let path = root.path().join("jobs.sqlite");
    let mut journal = Journal::open(&path).unwrap();
    let ids = journal.enqueue_many(&plans, Conflict::Error).unwrap();
    assert_eq!(ids.len(), 3);
    let id = ids[0];
    let queue = journal.operations(32, 0, true).unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].source_count, 3);
    assert_eq!(queue[0].entry_count, 5);
    assert_eq!(journal.operations(1, 1, false).unwrap().len(), 0);
    let files = journal.operation_files(id, 2, 1).unwrap();
    assert_eq!(files.total, 5);
    assert_eq!(
        files.entries.iter().map(|e| e.ordinal).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(files.entries[0].source, plans[1].source);
    assert_eq!(files.entries[1].relative, PathBuf::from("дерево-😀"));
    let control = Control::default();
    recovery::run_operation(&mut journal, id, &control, &mut |event| {
        assert_eq!(event.job_id, id);
        if event.entry == 0 && event.phase == Phase::Committed {
            control.cancel();
        }
        Ok(())
    })
    .unwrap();
    journal.hold_operation(id, false).unwrap();
    assert_eq!(journal.job(ids[0]).unwrap().status, "completed");
    assert_eq!(journal.operation(id).unwrap().status, "paused");
    assert!(journal.operations(32, 0, true).unwrap().is_empty());
    drop(journal);
    let mut journal = Journal::open(&path).unwrap();
    journal.schedule_operation(id).unwrap();
    let mut written = 0;
    let report = recovery::run_operation(&mut journal, id, &Control::default(), &mut |event| {
        assert!(event.entry >= 1);
        assert_eq!(event.job_id, id);
        written += event.fresh_written_delta;
        Ok(())
    })
    .unwrap();
    assert_eq!(report.status, "completed");
    assert_eq!(written, 8);
    assert_eq!(report.entries_committed, 5);
    assert_eq!(report.entries_pending, 0);
    assert_eq!(journal.operations(32, 0, false).unwrap().len(), 1);
    for (name, bytes) in [
        ("a.txt", b"one".as_slice()),
        ("b.txt", b"two".as_slice()),
        ("дерево-😀/дані.txt", b"three".as_slice()),
    ] {
        assert_eq!(
            fs::read(root.path().join("dest").join(name)).unwrap(),
            bytes
        );
    }
    assert!(root.path().join("dest/дерево-😀/empty").is_dir());
    let report_path = root.path().join("operation.json");
    journal.export_operation_report(id, &report_path).unwrap();
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(&report_path).unwrap()).unwrap();
    assert_eq!(saved["format"], "vartacopy-operation-report");
    assert_eq!(saved["members"].as_array().unwrap().len(), 3);
    assert!(journal.export_operation_report(id, &report_path).is_err());
}

#[test]
fn operation_cancel_reorder_and_recheck_apply_to_every_member() {
    let (root, plans) = fixture();
    let mut journal = Journal::open(&root.path().join("jobs.sqlite")).unwrap();
    let first = journal.enqueue_many(&plans[..2], Conflict::Error).unwrap()[0];
    let second = journal.enqueue(&plans[2], Conflict::Error).unwrap();
    journal.move_operation(first, false).unwrap();
    assert_eq!(
        journal
            .operations(10, 0, true)
            .unwrap()
            .iter()
            .map(|j| j.id)
            .collect::<Vec<_>>(),
        vec![second, first]
    );
    journal.hold_operation(first, true).unwrap();
    assert!(
        journal
            .operation_members(first)
            .unwrap()
            .iter()
            .all(|j| j.status == "cancelled")
    );
    assert_eq!(journal.operations(10, 0, true).unwrap().len(), 1);
    journal.schedule_operation(first).unwrap();
    assert_eq!(
        journal
            .operation_members(first)
            .unwrap()
            .iter()
            .filter(|j| j.status == "queued")
            .count(),
        2
    );
    let report =
        recovery::run_operation(&mut journal, first, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "completed");
    journal.recheck_operation(first).unwrap();
    let mut written = 0;
    recovery::run_operation(&mut journal, first, &Control::default(), &mut |event| {
        written += event.fresh_written_delta;
        Ok(())
    })
    .unwrap();
    assert_eq!(written, 0);
}

#[test]
fn version_four_dispatch_history_migrates_without_changing_root_jobs_or_checkpoints() {
    let (root, plans) = fixture();
    let path = root.path().join("jobs.sqlite");
    let mut journal = Journal::open(&path).unwrap();
    let receipt = journal
        .accept_dispatch(
            [93; 16],
            b"legacy-batch",
            OperationKind::Copy,
            Conflict::Error,
            Some(DispatchEngine::VartaCopy),
            &plans,
        )
        .unwrap()
        .unwrap();
    let ids = receipt.job_ids;
    for id in &ids {
        recovery::run(&mut journal, *id, &Control::default(), &mut |_| Ok(())).unwrap();
    }
    let before = serde_json::to_value(journal.jobs(32, 0).unwrap()).unwrap();
    journal
        .db
        .execute_batch(
            "DROP TABLE fat_attempts; DROP TABLE operation_members; PRAGMA user_version=4;",
        )
        .unwrap();
    journal.flush().unwrap();
    drop(journal);
    let journal = Journal::open(&path).unwrap();
    assert_eq!(
        serde_json::to_value(journal.jobs(32, 0).unwrap()).unwrap(),
        before
    );
    let history = journal.operations(32, 0, false).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].source_count, 3);
    assert_eq!(history[0].status, "completed");
    assert_eq!(
        journal.dispatch_receipt([93; 16]).unwrap().unwrap().job_ids,
        ids
    );
    assert_eq!(
        journal
            .db
            .query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}

#[test]
fn a_failed_root_does_not_hide_later_roots_or_issues_on_other_pages() {
    let (root, plans) = fixture();
    fs::write(root.path().join("dest/a.txt"), b"existing a").unwrap();
    fs::write(root.path().join("dest/b.txt"), b"existing b").unwrap();
    let mut journal = Journal::open(&root.path().join("jobs.sqlite")).unwrap();
    let id = journal.enqueue_many(&plans, Conflict::Error).unwrap()[0];
    let report =
        recovery::run_operation(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "interrupted");
    assert!(root.path().join("dest/дерево-😀/дані.txt").is_file());
    let first = journal.operation_details(id, 1, 0).unwrap();
    let second = journal.operation_details(id, 1, 1).unwrap();
    assert!(first.has_more);
    assert!(!second.has_more);
    assert_eq!(first.issues.len(), 1);
    assert_eq!(second.issues.len(), 1);
    assert_eq!(first.issues[0].relative, PathBuf::from("a.txt"));
    assert_eq!(second.issues[0].relative, PathBuf::from("b.txt"));
    assert_eq!(
        fs::read(root.path().join("dest/a.txt")).unwrap(),
        b"existing a"
    );
}

#[test]
fn grouped_move_removes_only_verified_sources_and_finishes_as_one_operation() {
    let (root, plans) = fixture();
    let mut journal = Journal::open(&root.path().join("jobs.sqlite")).unwrap();
    let id = journal
        .enqueue_many_with_operation(&plans, Conflict::Error, OperationKind::Move)
        .unwrap()[0];
    let report =
        recovery::run_operation(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "completed", "{report:?}");
    assert_eq!(report.entries_pending, 0);
    assert!(plans.iter().all(|p| !p.source.exists()));
    assert_eq!(journal.operations(10, 0, false).unwrap().len(), 1);
    assert!(journal.recheck_operation(id).is_err());
}
