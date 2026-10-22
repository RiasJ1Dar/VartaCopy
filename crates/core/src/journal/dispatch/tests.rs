use super::*;
use crate::planner::{ScanOptions, scan};
use std::{
    fs,
    sync::{Arc, Barrier, atomic::AtomicBool},
};

fn fixture() -> (tempfile::TempDir, OperationPlan) {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    fs::write(&source, b"own fixture bytes").unwrap();
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
fn count(journal: &Journal, table: &str) -> i64 {
    // Only fixed test-owned table names are passed here.
    journal
        .db
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn varta_accept_replay_reopen_freezes_engine_payload_options_and_jobs_without_running() {
    let (root, first) = fixture();
    let (_second_root, second) = fixture();
    let plans = vec![first, second];
    let path = root.path().join("ledger.sqlite");
    let mut journal = Journal::open(&path).unwrap();
    let id = [1; 16];
    let bytes = b"trusted canonical envelope v1";
    let saved = journal
        .accept_dispatch(
            id,
            bytes,
            OperationKind::Move,
            Conflict::KeepBoth,
            Some(DispatchEngine::VartaCopy),
            &plans,
        )
        .unwrap()
        .unwrap();
    assert_eq!(saved.job_ids.len(), 2);
    assert_eq!(saved.state, DispatchState::Accepted);
    for job in &saved.job_ids {
        let job = journal.job(*job).unwrap();
        assert_eq!(job.operation, OperationKind::Move);
        assert_eq!(job.status, "queued");
        assert!(job.source.exists());
        assert!(!job.destination.exists(), "accept must not run jobs");
    }
    journal.flush().unwrap();
    drop(journal);
    let mut journal = Journal::open(&path).unwrap();
    let looked_up = journal.dispatch_receipt(id).unwrap().unwrap();
    assert_eq!(looked_up.receipt_id, saved.receipt_id);
    assert_eq!(looked_up.job_ids, saved.job_ids);
    assert_eq!(journal.dispatch_payload(id).unwrap().unwrap(), bytes);
    // Replay does not need a repeated scan: the original jobs stay frozen.
    let replay = journal
        .accept_dispatch(
            id,
            bytes,
            OperationKind::Move,
            Conflict::KeepBoth,
            Some(DispatchEngine::VartaCopy),
            &[],
        )
        .unwrap()
        .unwrap();
    assert_eq!(replay.receipt_id, saved.receipt_id);
    assert_eq!(replay.job_ids, saved.job_ids);
    for (payload, operation, conflict, engine) in [
        (
            b"changed".as_slice(),
            OperationKind::Move,
            Conflict::KeepBoth,
            DispatchEngine::VartaCopy,
        ),
        (
            bytes.as_slice(),
            OperationKind::Copy,
            Conflict::KeepBoth,
            DispatchEngine::VartaCopy,
        ),
        (
            bytes.as_slice(),
            OperationKind::Move,
            Conflict::Skip,
            DispatchEngine::VartaCopy,
        ),
        (
            bytes.as_slice(),
            OperationKind::Move,
            Conflict::KeepBoth,
            DispatchEngine::Explorer,
        ),
    ] {
        assert!(
            journal
                .accept_dispatch(id, payload, operation, conflict, Some(engine), &[])
                .is_err()
        );
    }
    assert!(journal.begin_native_dispatch(id).is_err());
    assert!(
        journal
            .finish_native_dispatch(id, b"declared result")
            .is_err()
    );
    assert_eq!(count(&journal, "jobs"), 2);
    assert_eq!(count(&journal, "dispatch_requests"), 1);
    assert_eq!(count(&journal, "dispatch_jobs"), 2);
}

#[test]
fn injected_sql_failure_after_first_job_rolls_back_request_jobs_links_and_events() {
    let (root, first) = fixture();
    let (_second_root, second) = fixture();
    let plans = vec![first, second];
    let mut journal = Journal::open(&root.path().join("ledger.sqlite")).unwrap();
    journal.db.execute_batch("CREATE TEMP TRIGGER injected_second_job BEFORE INSERT ON jobs WHEN (SELECT count(*) FROM jobs)=1 BEGIN SELECT RAISE(ABORT,'injected after first job insert'); END;").unwrap();
    let error = journal
        .accept_dispatch(
            [2; 16],
            b"batch",
            OperationKind::Copy,
            Conflict::Error,
            Some(DispatchEngine::VartaCopy),
            &plans,
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected after first job insert")
    );
    for table in [
        "dispatch_requests",
        "dispatch_jobs",
        "jobs",
        "entries",
        "events",
    ] {
        assert_eq!(count(&journal, table), 0, "{table}");
    }
    assert!(journal.dispatch_receipt([2; 16]).unwrap().is_none());
    journal
        .db
        .execute_batch("DROP TRIGGER injected_second_job;")
        .unwrap();
    let accepted = journal
        .accept_dispatch(
            [2; 16],
            b"batch",
            OperationKind::Copy,
            Conflict::Error,
            Some(DispatchEngine::VartaCopy),
            &plans,
        )
        .unwrap()
        .unwrap();
    assert_eq!(accepted.job_ids.len(), 2);
}

#[test]
fn cancel_has_no_sql_and_fresh_accept_bounds_and_planner_boundary_are_enforced() {
    let (root, mut plan) = fixture();
    let mut journal = Journal::open(&root.path().join("ledger.sqlite")).unwrap();
    journal.db.execute_batch("BEGIN IMMEDIATE;").unwrap();
    // If cancel touched the ledger transaction, this would fail as nested SQL.
    assert!(
        journal
            .accept_dispatch(
                [0; 16],
                &[],
                OperationKind::Move,
                Conflict::Error,
                None,
                std::slice::from_ref(&plan)
            )
            .unwrap()
            .is_none()
    );
    assert!(!journal.db.is_autocommit());
    journal.db.execute_batch("ROLLBACK;").unwrap();
    assert!(
        journal
            .accept_dispatch(
                [3; 16],
                b"fresh",
                OperationKind::Copy,
                Conflict::Error,
                Some(DispatchEngine::VartaCopy),
                &[]
            )
            .is_err()
    );
    plan.status = "blocked";
    assert!(
        journal
            .accept_dispatch(
                [3; 16],
                b"fresh",
                OperationKind::Copy,
                Conflict::Error,
                Some(DispatchEngine::VartaCopy),
                std::slice::from_ref(&plan)
            )
            .is_err()
    );
    assert!(
        journal
            .accept_dispatch(
                [3; 16],
                b"fresh",
                OperationKind::Copy,
                Conflict::Error,
                Some(DispatchEngine::Explorer),
                std::slice::from_ref(&plan)
            )
            .is_err()
    );
    for (id, bytes) in [
        ([0; 16], vec![1]),
        ([3; 16], vec![]),
        ([3; 16], vec![1; MAX_CANONICAL_REQUEST_BYTES + 1]),
    ] {
        assert!(
            journal
                .accept_dispatch(
                    id,
                    &bytes,
                    OperationKind::Copy,
                    Conflict::Error,
                    Some(DispatchEngine::Explorer),
                    &[]
                )
                .is_err()
        );
    }
    assert_eq!(count(&journal, "dispatch_requests"), 0);
    assert_eq!(count(&journal, "jobs"), 0);
    let max = vec![7; MAX_CANONICAL_REQUEST_BYTES];
    journal
        .accept_dispatch(
            [3; 16],
            &max,
            OperationKind::Copy,
            Conflict::Error,
            Some(DispatchEngine::Explorer),
            &[],
        )
        .unwrap();
    assert_eq!(journal.dispatch_payload([3; 16]).unwrap().unwrap(), max);
    assert!(
        journal
            .finish_native_dispatch([3; 16], b"before intent")
            .is_err()
    );
    journal.begin_native_dispatch([3; 16]).unwrap();
    for result in [vec![], vec![0; MAX_NATIVE_RESULT_BYTES + 1]] {
        assert!(journal.finish_native_dispatch([3; 16], &result).is_err());
    }
    assert_eq!(
        journal.dispatch_receipt([3; 16]).unwrap().unwrap().state,
        DispatchState::NativeIntent
    );
    let max_result = vec![9; MAX_NATIVE_RESULT_BYTES];
    journal
        .finish_native_dispatch([3; 16], &max_result)
        .unwrap();
    assert_eq!(
        journal
            .finish_native_dispatch([3; 16], &max_result)
            .unwrap()
            .native_result,
        Some(max_result)
    );
    assert!(
        journal
            .finish_native_dispatch([3; 16], b"changed result")
            .is_err()
    );
    assert!(journal.begin_native_dispatch([3; 16]).is_err());
}

#[test]
fn native_intent_survives_crash_as_ambiguous_and_is_discoverable_without_known_id() {
    let (root, _plan) = fixture();
    let path = root.path().join("ledger.sqlite");
    let mut journal = Journal::open(&path).unwrap();
    for number in 10..=12 {
        let accepted = journal
            .accept_dispatch(
                [number; 16],
                &[number],
                OperationKind::Move,
                Conflict::Skip,
                Some(DispatchEngine::Explorer),
                &[],
            )
            .unwrap()
            .unwrap();
        assert!(accepted.job_ids.is_empty());
    }
    let intent = journal.begin_native_dispatch([10; 16]).unwrap();
    assert_eq!(intent.state, DispatchState::NativeIntent);
    assert!(journal.begin_native_dispatch([10; 16]).is_err());
    journal.begin_native_dispatch([12; 16]).unwrap();
    journal
        .finish_native_dispatch(
            [12; 16],
            b"adapter reported cancellation; no filesystem success asserted",
        )
        .unwrap();
    journal.flush().unwrap();
    drop(journal); // Simulates losing the caller between intent and native acknowledgement.
    let mut journal = Journal::open(&path).unwrap();
    assert!(
        journal.begin_native_dispatch([10; 16]).is_err(),
        "never automatically repeat potentially started native dispatch"
    );
    let first = journal.unresolved_native_dispatches(1, 0).unwrap();
    assert!(first.has_more);
    assert_eq!(first.receipts[0].request_id, [10; 16]);
    assert_eq!(first.receipts[0].state, DispatchState::NativeIntent);
    let second = journal.unresolved_native_dispatches(1, 1).unwrap();
    assert!(!second.has_more);
    assert_eq!(second.receipts[0].request_id, [11; 16]);
    for row in first.receipts.iter().chain(second.receipts.iter()) {
        assert!(journal.dispatch_payload(row.request_id).unwrap().is_some());
    }
    assert!(journal.unresolved_native_dispatches(0, 0).is_err());
    assert!(journal.unresolved_native_dispatches(129, 0).is_err());
    assert!(journal.unresolved_native_dispatches(1, usize::MAX).is_err());
    let replay = journal
        .accept_dispatch(
            [10; 16],
            &[10],
            OperationKind::Move,
            Conflict::Skip,
            Some(DispatchEngine::Explorer),
            &[],
        )
        .unwrap()
        .unwrap();
    assert_eq!(replay.receipt_id, intent.receipt_id);
    assert_eq!(replay.state, DispatchState::NativeIntent);
    assert_eq!(count(&journal, "jobs"), 0);
}

#[test]
fn two_peer_accepts_serialize_to_one_receipt_and_one_job() {
    let (root, plan) = fixture();
    let journal = Journal::open(&root.path().join("ledger.sqlite")).unwrap();
    let plans = Arc::new(vec![plan]);
    let barrier = Arc::new(Barrier::new(2));
    let peers = [journal.peer().unwrap(), journal.peer().unwrap()];
    let threads = peers
        .into_iter()
        .map(|mut peer| {
            let plans = plans.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                peer.accept_dispatch(
                    [20; 16],
                    b"same concurrent request",
                    OperationKind::Copy,
                    Conflict::Error,
                    Some(DispatchEngine::VartaCopy),
                    &plans,
                )
                .unwrap()
                .unwrap()
            })
        })
        .collect::<Vec<_>>();
    let receipts = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(receipts[0].receipt_id, receipts[1].receipt_id);
    assert_eq!(receipts[0].job_ids, receipts[1].job_ids);
    assert_eq!(count(&journal, "dispatch_requests"), 1);
    assert_eq!(count(&journal, "dispatch_jobs"), 1);
    assert_eq!(count(&journal, "jobs"), 1);
    assert_eq!(count(&journal, "events"), 1);
}

#[test]
fn version_three_migration_preserves_copy_move_queue_and_cleanup() {
    let (root, plan) = fixture();
    let path = root.path().join("ledger.sqlite");
    let mut journal = Journal::open(&path).unwrap();
    let copy = journal.enqueue(&plan, Conflict::Error).unwrap();
    let moved = journal
        .enqueue_with_operation(&plan, Conflict::KeepBoth, OperationKind::Move)
        .unwrap();
    journal
        .db
        .execute(
            "UPDATE entries SET state='committed' WHERE job_id=?1",
            [moved],
        )
        .unwrap();
    journal.cleanup_transition(moved, 0, false).unwrap();
    journal
        .db
        .execute_batch(
            "DROP TABLE operation_members; DROP TABLE dispatch_jobs; DROP TABLE dispatch_requests; PRAGMA user_version=3;",
        )
        .unwrap();
    journal.flush().unwrap();
    drop(journal);
    let mut journal = Journal::open(&path).unwrap();
    assert_eq!(
        journal
            .db
            .pragma_query_value(None, "user_version", |r| r.get::<_, i32>(0))
            .unwrap(),
        VERSION
    );
    assert_eq!(journal.job(copy).unwrap().operation, OperationKind::Copy);
    assert_eq!(journal.job(moved).unwrap().operation, OperationKind::Move);
    assert_eq!(
        journal.cleanup_state(moved, 0).unwrap().as_deref(),
        Some("intent")
    );
    assert_eq!(
        journal
            .queue(256, 0)
            .unwrap()
            .iter()
            .map(|job| job.id)
            .collect::<Vec<_>>(),
        vec![copy, moved]
    );
    assert_eq!(count(&journal, "dispatch_requests"), 0);
    journal
        .accept_dispatch(
            [30; 16],
            b"post migration",
            OperationKind::Copy,
            Conflict::Error,
            Some(DispatchEngine::Explorer),
            &[],
        )
        .unwrap();
}

#[test]
fn two_peers_cannot_acquire_the_same_native_start_intent_twice() {
    let (root, _plan) = fixture();
    let mut journal = Journal::open(&root.path().join("ledger.sqlite")).unwrap();
    journal
        .accept_dispatch(
            [40; 16],
            b"one native attempt",
            OperationKind::Move,
            Conflict::Error,
            Some(DispatchEngine::Explorer),
            &[],
        )
        .unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let threads = [journal.peer().unwrap(), journal.peer().unwrap()]
        .into_iter()
        .map(|mut peer| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                peer.begin_native_dispatch([40; 16])
            })
        })
        .collect::<Vec<_>>();
    let results = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    for result in results {
        match result {
            Ok(receipt) => assert_eq!(receipt.state, DispatchState::NativeIntent),
            Err(error) => assert!(
                error
                    .to_string()
                    .contains("reconcile existing intent/result"),
                "{error}"
            ),
        }
    }
    assert_eq!(
        journal.dispatch_receipt([40; 16]).unwrap().unwrap().state,
        DispatchState::NativeIntent
    );
    assert_eq!(count(&journal, "jobs"), 0);
}
