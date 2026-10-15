#![cfg(feature = "test-harness")]
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::{atomic::AtomicBool, mpsc};
use std::time::Duration;
use vartacopy_core::{
    Conflict,
    control::Control,
    journal::{CHUNK_BYTES, Journal},
    planner::{ScanOptions, scan},
    recovery,
};

#[test]
fn terminate_process_at_every_boundary_then_resume_without_rust_destructors() {
    for phase in [
        "stage_recorded",
        "block_written",
        "block_flushed",
        "checkpoint_committed",
        "verified",
        "publish_intent",
        "published",
        "committed",
    ] {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.bin");
        let destination = root.path().join("destination");
        fs::create_dir(&destination).unwrap();
        let data = (0..CHUNK_BYTES + 8193)
            .map(|i| (i % 239) as u8)
            .collect::<Vec<_>>();
        fs::write(&source, &data).unwrap();
        // ADS is independently checked after each real process termination.
        let ads = std::path::PathBuf::from(format!("{}:note", source.display()));
        fs::write(&ads, b"alternate data").unwrap();
        let path = root.path().join("jobs.sqlite");
        let id;
        {
            let plan = scan(
                &source,
                &destination,
                ScanOptions::default(),
                &AtomicBool::new(false),
            )
            .unwrap();
            let mut journal = Journal::open(&path).unwrap();
            id = journal.enqueue(&plan, Conflict::Error).unwrap();
        }
        kill_at(&path, id, phase);
        let mut journal = Journal::open(&path).unwrap();
        let report = recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
        assert_eq!(report.status, "completed", "{phase}: {report:?}");
        let target = destination.join("source.bin");
        assert_eq!(fs::read(&target).unwrap(), data, "{phase}");
        assert_eq!(
            fs::read(std::path::PathBuf::from(format!(
                "{}:note",
                target.display()
            )))
            .unwrap(),
            b"alternate data",
            "{phase}"
        );
        assert_eq!(fs::read(&source).unwrap(), data);
        if phase == "checkpoint_committed" {
            assert_eq!(report.resumed_bytes, CHUNK_BYTES as u64);
        }
        let repeated =
            recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
        assert_eq!(repeated.status, "completed");
        assert_eq!(
            fs::read_dir(&destination).unwrap().count(),
            1,
            "{phase}: staging must be renamed, not duplicated"
        );
    }
}

fn kill_at(path: &std::path::Path, id: i64, phase: &str) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_vartacopy-fault"))
        .arg(path)
        .arg(id.to_string())
        .arg(phase)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        sender.send(result).expect("barrier receiver");
    });
    let ready = receiver.recv_timeout(Duration::from_secs(30));
    if !matches!(&ready,Ok(Ok(line)) if line.trim()=="READY") {
        if child.try_wait().unwrap().is_none() {
            child.kill().unwrap();
        }
        child.wait().unwrap();
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        panic!("{phase}: barrier missing: {ready:?}; {stderr}");
    }
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success());
    reader.join().unwrap();
}

#[test]
fn terminate_process_during_tree_creation_and_postorder_metadata() {
    for phase in [
        "directory_intent",
        "directory_published",
        "directory_ready",
        "checkpoint_committed",
        "published",
        "directory_committed",
    ] {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("tree");
        let destination = root.path().join("destination");
        fs::create_dir_all(source.join("nested/empty")).unwrap();
        fs::create_dir(&destination).unwrap();
        let data = vec![173; CHUNK_BYTES + 311];
        fs::write(source.join("nested/data"), &data).unwrap();
        fs::write(source.join("small"), b"small").unwrap();
        let path = root.path().join("jobs.sqlite");
        let id = {
            let plan = scan(
                &source,
                &destination,
                ScanOptions::default(),
                &AtomicBool::new(false),
            )
            .unwrap();
            Journal::open(&path)
                .unwrap()
                .enqueue(&plan, Conflict::Error)
                .unwrap()
        };
        kill_at(&path, id, phase);
        let mut journal = Journal::open(&path).unwrap();
        for _ in 0..2 {
            let report =
                recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
            assert_eq!(report.status, "completed", "{phase}: {report:?}");
            assert_eq!(report.entries_committed, 5);
            assert_eq!(
                fs::read(destination.join("tree/nested/data")).unwrap(),
                data
            );
            assert_eq!(fs::read(destination.join("tree/small")).unwrap(), b"small");
            assert!(destination.join("tree/nested/empty").is_dir());
            assert_eq!(fs::read_dir(&destination).unwrap().count(), 1);
            assert_eq!(fs::read_dir(destination.join("tree")).unwrap().count(), 2);
        }
    }
}
