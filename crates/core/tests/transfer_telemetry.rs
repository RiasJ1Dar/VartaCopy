use std::{collections::HashMap, fs, path::PathBuf, sync::atomic::AtomicBool};
use vartacopy_core::{
    Conflict, Error,
    control::Control,
    journal::{CHUNK_BYTES, Journal, OperationKind},
    planner::{EntryKind, ScanOptions, scan},
    recovery::{self, Phase, Progress},
};
use vartacopy_platform_windows as platform;

fn stream_phase(phase: Phase) -> bool {
    matches!(
        phase,
        Phase::Checking | Phase::BlockWritten | Phase::BlockFlushed | Phase::CheckpointCommitted
    )
}

#[test]
fn actual_file_and_ads_writes_have_exact_deltas_and_stable_stream_keys() {
    let legacy: Progress =
        serde_json::from_str(r#"{"job_id":1,"entry":0,"phase":"checking","bytes":17,"total":23}"#)
            .unwrap();
    assert_eq!(legacy.fresh_written_delta, 0);
    assert_eq!(legacy.stream_index, None);
    let json = serde_json::to_value(&legacy).unwrap();
    assert_eq!(json["bytes"], 17);
    assert_eq!(json["total"], 23);
    assert_eq!(json["fresh_written_delta"], 0);
    assert!(json["stream_index"].is_null());

    for operation in [OperationKind::Copy, OperationKind::Move] {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("tree");
        let destination = root.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(source.join("first"), vec![31; CHUNK_BYTES + 21]).unwrap();
        fs::write(
            platform::stream_path(&source.join("first"), ":named".as_ref()),
            vec![47; 37],
        )
        .unwrap();
        fs::write(source.join("second"), vec![53; 19]).unwrap();
        fs::write(
            platform::stream_path(&source.join("second"), ":other".as_ref()),
            vec![61; 31],
        )
        .unwrap();
        let plan = scan(
            &source,
            &destination,
            ScanOptions::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let mut journal = Journal::open(&root.path().join("journal.sqlite")).unwrap();
        let id = journal
            .enqueue_with_operation(&plan, Conflict::Error, operation)
            .unwrap();
        let mut expected = HashMap::new();
        for ordinal in 0..journal.job(id).unwrap().entry_count {
            let entry = journal.entry(id, ordinal).unwrap();
            if entry.kind != EntryKind::File {
                continue;
            }
            let path = source.join(entry.relative);
            for (index, stream) in platform::streams(&path).unwrap().into_iter().enumerate() {
                let contents = fs::read(platform::stream_path(&path, &stream.name)).unwrap();
                expected.insert((ordinal, index), (stream.name, contents));
            }
        }
        let mut events = Vec::new();
        let report = recovery::run(&mut journal, id, &Control::default(), &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();
        assert_eq!(report.status, "completed", "{report:?}");
        let mut written = HashMap::<(usize, usize), u64>::new();
        for (position, event) in events.iter().enumerate() {
            assert_eq!(event.job_id, id);
            if stream_phase(event.phase) {
                let key = (
                    event.entry,
                    event.stream_index.expect("stream-local event identity"),
                );
                let (_, contents) = expected.get(&key).expect("recorded entry/stream key");
                assert_eq!(event.total, contents.len() as u64);
                assert!(event.bytes <= event.total);
                if event.phase == Phase::BlockWritten {
                    assert!(event.fresh_written_delta > 0);
                    *written.entry(key).or_default() += event.fresh_written_delta;
                    // Telemetry must preserve the existing crash boundaries and
                    // stream-local byte positions, including ADS resets.
                    for (next, phase) in [
                        (position + 1, Phase::BlockFlushed),
                        (position + 2, Phase::CheckpointCommitted),
                    ] {
                        let next = &events[next];
                        assert_eq!(next.phase, phase);
                        assert_eq!(
                            (next.entry, next.stream_index, next.bytes, next.total),
                            (event.entry, event.stream_index, event.bytes, event.total)
                        );
                        assert_eq!(next.fresh_written_delta, 0);
                    }
                } else {
                    assert_eq!(event.fresh_written_delta, 0);
                }
            } else {
                assert_eq!(event.stream_index, None);
                assert_eq!(event.fresh_written_delta, 0);
            }
        }
        assert_eq!(written.len(), expected.len());
        let expected_total: u64 = expected
            .values()
            .map(|(_, contents)| contents.len() as u64)
            .sum();
        assert_eq!(written.values().sum::<u64>(), expected_total);
        assert_eq!(report.verified_bytes, expected_total);
        for ((ordinal, index), (name, contents)) in expected {
            assert_eq!(written[&(ordinal, index)], contents.len() as u64);
            let target = journal.entry(id, ordinal).unwrap().target.unwrap();
            assert_eq!(
                fs::read(platform::stream_path(&target, &name)).unwrap(),
                contents
            );
        }
        assert_eq!(source.exists(), operation == OperationKind::Copy);
    }
}

#[test]
fn resumed_prefix_and_repeated_recheck_never_become_fresh_write_bytes() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir(&destination).unwrap();
    let main = vec![71; CHUNK_BYTES * 2 + 31];
    let ads = vec![83; CHUNK_BYTES + 13];
    fs::write(&source, &main).unwrap();
    fs::write(platform::stream_path(&source, ":named".as_ref()), &ads).unwrap();
    let journal_path = root.path().join("journal.sqlite");
    let plan = scan(
        &source,
        &destination,
        ScanOptions::default(),
        &AtomicBool::new(false),
    )
    .unwrap();
    let mut journal = Journal::open(&journal_path).unwrap();
    let id = journal.enqueue(&plan, Conflict::Error).unwrap();
    let mut first_written = 0;
    let report = recovery::run(&mut journal, id, &Control::default(), &mut |event| {
        first_written += event.fresh_written_delta;
        if event.phase == Phase::CheckpointCommitted {
            return Err(Error::Journal("checkpoint interruption".into()));
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(report.status, "interrupted");
    assert_eq!(first_written, CHUNK_BYTES as u64);
    drop(journal);
    let mut journal = Journal::open(&journal_path).unwrap();
    let mut events = Vec::new();
    let report = recovery::run(&mut journal, id, &Control::default(), &mut |event| {
        events.push(event);
        Ok(())
    })
    .unwrap();
    assert_eq!(report.status, "completed", "{report:?}");
    assert_eq!(report.resumed_bytes, CHUNK_BYTES as u64);
    let total = (main.len() + ads.len()) as u64;
    assert_eq!(
        events
            .iter()
            .map(|event| event.fresh_written_delta)
            .sum::<u64>(),
        total - first_written
    );
    assert!(events.iter().any(|event| event.phase == Phase::Checking
        && event.stream_index == Some(0)
        && event.bytes == CHUNK_BYTES as u64
        && event.fresh_written_delta == 0));
    assert!(
        events
            .iter()
            .filter(|event| event.phase != Phase::BlockWritten)
            .all(|event| event.fresh_written_delta == 0)
    );
    for _ in 0..2 {
        journal.recheck(id).unwrap();
        let mut events = Vec::new();
        let report = recovery::run(&mut journal, id, &Control::default(), &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();
        assert_eq!(report.status, "completed", "{report:?}");
        assert_eq!(report.verified_bytes, total);
        assert!(events.iter().all(|event| event.fresh_written_delta == 0));
        assert!(
            events
                .iter()
                .any(|event| event.phase == Phase::Checking && event.stream_index == Some(0))
        );
        assert!(
            events
                .iter()
                .any(|event| event.phase == Phase::Checking && event.stream_index == Some(1))
        );
        assert!(
            !events
                .iter()
                .any(|event| event.phase == Phase::BlockWritten)
        );
    }
    let target: PathBuf = journal.entry(id, 0).unwrap().target.unwrap();
    assert_eq!(fs::read(&target).unwrap(), main);
    assert_eq!(
        fs::read(platform::stream_path(&target, ":named".as_ref())).unwrap(),
        ads
    );
}
