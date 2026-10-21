use super::*;
use std::{fs, io::Write};
fn records(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
#[test]
fn failed_recovery_keeps_native_error_before_it_becomes_a_text_report() {
    use crate::{Conflict, control::Control, journal::Journal, planner, recovery};
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.txt");
    let destination = root.path().join("destination");
    fs::create_dir(&destination).unwrap();
    fs::write(&source, b"original source").unwrap();
    fs::write(destination.join("source.txt"), b"existing destination").unwrap();
    let journal_path = root.path().join("jobs.sqlite");
    let session = Session::open(&journal_path, "test").unwrap();
    let _scope = enter(Some(session.clone()));
    let plan = planner::scan(
        &source,
        &destination,
        Default::default(),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap();
    let mut journal = Journal::open(&journal_path).unwrap();
    let id = journal.enqueue(&plan, Conflict::Error).unwrap();
    let report = recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "interrupted");
    let entries = records(session.log_path());
    assert!(
        entries
            .iter()
            .any(|e| e["event"] == "job.start" && e["data"]["job_id"] == id)
    );
    let failure = entries.iter().find(|e| e["event"] == "job.failed").unwrap();
    assert!(
        failure["data"]["error"]["chain"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["os_error"].is_i64())
    );
    assert!(
        entries
            .iter()
            .any(|e| e["event"] == "job.finished" && e["data"]["status"] == "interrupted")
    );
    assert_eq!(fs::read(source).unwrap(), b"original source");
    assert_eq!(
        fs::read(destination.join("source.txt")).unwrap(),
        b"existing destination"
    );
}
#[test]
#[ignore = "subprocess fixture, executed by panic_capture_survives_thread_without_scope"]
fn panic_child() {
    let journal = std::env::var_os("VARTACOPY_DIAGNOSTIC_PANIC_JOURNAL").expect("fixture journal");
    install_panic_hook();
    let _session = Session::open(Path::new(&journal), "panic-fixture").unwrap();
    panic!("diagnostic panic fixture");
}
#[test]
fn panic_capture_survives_thread_without_scope() {
    let root = tempfile::tempdir().unwrap();
    let journal = root.path().join("jobs.sqlite");
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "diagnostics::tests::panic_child",
            "--nocapture",
        ])
        .env("VARTACOPY_DIAGNOSTIC_PANIC_JOURNAL", &journal)
        .output()
        .unwrap();
    assert!(!child.status.success());
    let entries = records(&root.path().join("jobs.sqlite.diagnostics.jsonl"));
    let panic = entries.iter().find(|e| e["event"] == "panic").unwrap();
    assert_eq!(panic["data"]["message"], "diagnostic panic fixture");
    assert!(panic["data"]["backtrace"].as_str().unwrap().len() > 32);
    assert!(
        panic["data"]["location"]
            .as_str()
            .unwrap()
            .contains("diagnostics")
    );
    assert_eq!(panic["level"], "error");
    assert!(panic["unix_ms"].is_number());
    assert!(panic["revision"].as_str().is_some());
}
#[test]
fn native_error_keeps_api_utf16_request_context_and_original_error_without_touching_source() {
    let root = tempfile::tempdir().unwrap();
    let journal = root.path().join("jobs.sqlite");
    let source = root.path().join("дані-😀.txt");
    fs::write(&source, b"retained").unwrap();
    let session = Session::open(&journal, "test").unwrap();
    let _scope = enter(Some(session.clone()));
    event(
        "request",
        json!({"request_id":"fixture","source":path(&source)}),
    );
    let result: io::Result<()> = super::io(
        "GetFileInformationByHandleEx(FileIdInfo)",
        &source,
        Err(io::Error::from_raw_os_error(87)),
    );
    assert_eq!(result.unwrap_err().raw_os_error(), Some(87));
    assert!(session.failure().is_none());
    let entries = records(session.log_path());
    let failure = entries
        .iter()
        .find(|e| e["event"] == "filesystem.error")
        .unwrap();
    assert_eq!(
        failure["data"]["context"]["api"],
        "GetFileInformationByHandleEx(FileIdInfo)"
    );
    assert_eq!(failure["data"]["error"]["chain"][0]["os_error"], 87);
    assert_eq!(
        failure["data"]["context"]["path"]["utf16"],
        json!(source.as_os_str().encode_wide().collect::<Vec<_>>())
    );
    assert_eq!(fs::read(source).unwrap(), b"retained");
}
#[test]
fn rotation_is_bounded_survives_restart_and_refuses_foreign_previous_file() {
    let root = tempfile::tempdir().unwrap();
    let journal = root.path().join("jobs.sqlite");
    let session = Session::open(&journal, "test").unwrap();
    let log = session.log_path().to_path_buf();
    for n in 0..260 {
        session.record(
            "info",
            "bulk",
            json!({"n":n,"detail":"x".repeat(48*1024)}),
            false,
        );
    }
    assert!(session.failure().is_none());
    let previous = root.path().join("jobs.sqlite.diagnostics.previous.jsonl");
    for file in [&log, &previous] {
        assert!(file.metadata().unwrap().len() <= sink::LIMIT);
        assert!(records(file).len() > 1);
    }
    // A second writer must not truncate, rotate or append to the same journal's log.
    assert!(Session::open(&journal, "second-writer").is_err());
    drop(session);
    let mut f = fs::OpenOptions::new().append(true).open(&log).unwrap();
    f.write_all(b"{\"torn\":").unwrap();
    drop(f);
    let session = Session::open(&journal, "restarted").unwrap();
    assert!(
        records(&log)
            .iter()
            .any(|e| e["event"] == "log.tail_repaired")
    );
    fs::write(&previous, b"foreign data").unwrap();
    for n in 0..100 {
        session.record(
            "info",
            "bulk",
            json!({"n":n,"detail":"x".repeat(48*1024)}),
            false,
        );
    }
    assert!(session.failure().unwrap().contains("unowned"));
    assert_eq!(fs::read(previous).unwrap(), b"foreign data");
}
#[test]
fn scoped_threads_share_a_session_and_foreign_or_linked_files_are_preserved() {
    let root = tempfile::tempdir().unwrap();
    let journal = root.path().join("jobs.sqlite");
    let log = root.path().join("jobs.sqlite.diagnostics.jsonl");
    fs::write(&log, b"unrelated log").unwrap();
    assert!(Session::open(&journal, "test").is_err());
    assert_eq!(fs::read(&log).unwrap(), b"unrelated log");
    fs::remove_file(&log).unwrap();
    let session = Session::open(&journal, "test").unwrap();
    let threads = (0..4)
        .map(|n| {
            let session = session.clone();
            std::thread::spawn(move || {
                let _scope = enter(Some(session));
                for item in 0..20 {
                    event("thread", json!({"n":n,"item":item}));
                }
            })
        })
        .collect::<Vec<_>>();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(
        records(&log)
            .iter()
            .filter(|e| e["event"] == "thread")
            .count(),
        80
    );
    drop(session);
    fs::hard_link(&log, root.path().join("alias.jsonl")).unwrap();
    assert!(Session::open(&journal, "test").is_err());
    assert_eq!(
        records(&log)
            .iter()
            .filter(|e| e["event"] == "thread")
            .count(),
        80
    );
}
