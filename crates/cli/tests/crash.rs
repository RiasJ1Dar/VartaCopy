#![cfg(feature = "test-harness")]
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{atomic::AtomicBool, mpsc};
use std::time::Duration;
use vartacopy_core::{
    Conflict,
    control::Control,
    journal::{CHUNK_BYTES, Journal, OperationKind},
    planner::{ScanOptions, scan},
    recovery,
};

#[test]
fn terminate_process_at_move_cleanup_boundaries_preserves_data_and_replacement_sources() {
    for phase in [
        "cleanup_intent",
        "source_delete_prepared",
        "source_removed",
        "cleanup_committed",
    ] {
        for replace_source in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("move.bin");
            let destination = root.path().join("destination");
            fs::create_dir(&destination).unwrap();
            let data = vec![97; CHUNK_BYTES + 701];
            fs::write(&source, &data).unwrap();
            let ads = std::path::PathBuf::from(format!("{}:note", source.display()));
            fs::write(&ads, b"move alternate data").unwrap();
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
                    .enqueue_with_operation(&plan, Conflict::Error, OperationKind::Move)
                    .unwrap()
            };
            kill_at_operation(&path, id, phase, OperationKind::Move);
            let target = destination.join("move.bin");
            // A real process kill must never leave neither verified output nor source.
            assert_eq!(fs::read(&target).unwrap(), data, "{phase}");
            assert_eq!(
                fs::read(format!("{}:note", target.display())).unwrap(),
                b"move alternate data",
                "{phase}"
            );
            if source.exists() {
                assert_eq!(fs::read(&source).unwrap(), data, "{phase}");
                assert_eq!(fs::read(&ads).unwrap(), b"move alternate data", "{phase}");
            }
            if replace_source {
                if source.exists() {
                    fs::rename(&source, root.path().join("original-retained.bin")).unwrap();
                }
                fs::write(&source, b"replacement must survive").unwrap();
            }
            let mut journal = Journal::open(&path).unwrap();
            for _ in 0..2 {
                let report =
                    recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
                assert_eq!(journal.job(id).unwrap().operation, OperationKind::Move);
                assert_eq!(fs::read(&target).unwrap(), data, "{phase}");
                assert_eq!(
                    fs::read(format!("{}:note", target.display())).unwrap(),
                    b"move alternate data"
                );
                if replace_source {
                    assert_eq!(
                        fs::read(&source).unwrap(),
                        b"replacement must survive",
                        "{phase}: {report:?}"
                    );
                    assert!(
                        matches!(report.status.as_str(), "partial" | "completed"),
                        "{report:?}"
                    );
                    if phase == "cleanup_intent" || phase == "source_delete_prepared" {
                        assert_eq!(
                            report.status, "partial",
                            "replacement identity must block pending cleanup"
                        );
                    }
                } else {
                    if phase == "source_removed" {
                        // Filesystem deletion alone is not durable proof: an ambiguous
                        // missing source remains pending until the cleanup SQL commit.
                        assert_eq!(report.status, "partial", "{phase}: {report:?}");
                        assert_eq!(report.entries_pending, 1, "{phase}: {report:?}");
                    } else {
                        assert_eq!(report.status, "completed", "{phase}: {report:?}");
                    }
                    assert!(!source.exists(), "{phase}");
                }
            }
        }
    }
}

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
    kill_at_operation(path, id, phase, OperationKind::Copy);
}

#[test]
#[ignore = "requires VARTACOPY_TEST_FAT_ROOT pointing to an authorized FAT32 fixture directory"]
fn native_fat_copy_terminate_process_then_resume() {
    let selected = std::path::PathBuf::from(
        std::env::var_os("VARTACOPY_TEST_FAT_ROOT").expect("explicit FAT fixture root required"),
    );
    assert!(selected.is_absolute());
    for directory in [false, true] {
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
            let destination = tempfile::Builder::new()
                .prefix("vartacopy-owned-fat-crash-")
                .tempdir_in(&selected)
                .unwrap();
            let source = root.path().join(if directory {
                "дерево-😀"
            } else {
                "дані-😀.bin"
            });
            let data = vec![53; CHUNK_BYTES + 701];
            if directory {
                fs::create_dir_all(source.join("nested/empty")).unwrap();
                fs::write(source.join("nested/data.bin"), &data).unwrap();
                fs::write(source.join("zero.txt"), []).unwrap();
            } else {
                fs::write(&source, &data).unwrap();
            }
            let path = root.path().join("jobs.sqlite");
            let id = {
                let plan = scan(
                    &source,
                    destination.path(),
                    ScanOptions::default(),
                    &AtomicBool::new(false),
                )
                .unwrap();
                Journal::open(&path)
                    .unwrap()
                    .enqueue(&plan, Conflict::Error)
                    .unwrap()
            };
            {
                let journal = Journal::open(&path).unwrap();
                let identity =
                    serde_json::to_value(journal.job(id).unwrap().parent_identity).unwrap();
                assert_eq!(
                    identity["kind"], "fat_source",
                    "the test requires a real FAT/FAT32 destination"
                );
            }
            kill_at(&path, id, phase);
            let mut journal = Journal::open(&path).unwrap();
            journal.recover_running_jobs().unwrap();
            for _ in 0..2 {
                let result =
                    recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
                assert_eq!(
                    result.status, "completed",
                    "{directory}/{phase}: {result:?}"
                );
                assert_eq!(
                    result.resumed_bytes, 0,
                    "FAT never adopts old stages for writes"
                );
                let output = destination.path().join(source.file_name().unwrap());
                let (original, copied) = if directory {
                    (
                        source.join("nested/data.bin"),
                        output.join("nested/data.bin"),
                    )
                } else {
                    (source.clone(), output)
                };
                assert_eq!(fs::read(original).unwrap(), data);
                assert_eq!(fs::read(copied).unwrap(), data);
            }
            println!(
                "FAT32 hard process termination: directory={directory}, phase={phase}: recovered and rechecked"
            );
        }
    }
}

fn kill_at_operation(path: &std::path::Path, id: i64, phase: &str, operation: OperationKind) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_vartacopy-fault"))
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW: hardware tests never take over the desktop.
        .arg(path)
        .arg(id.to_string())
        .arg(phase)
        .args(["--operation", operation.as_str()])
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
