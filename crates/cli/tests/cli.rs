use std::fs;
use std::os::windows::process::CommandExt;
use std::process::{Command, Output};

#[test]
fn cli_enqueue_jobs_resume_and_idempotent_completed_job() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("file");
    let destination = root.path().join("dest");
    let journal = root.path().join("jobs.sqlite");
    fs::create_dir(&destination).unwrap();
    fs::write(&source, b"durable").unwrap();
    let output = cli(&[
        "copy".as_ref(),
        source.as_os_str(),
        destination.as_os_str(),
        "--journal".as_ref(),
        journal.as_os_str(),
        "--enqueue".as_ref(),
        "--json".as_ref(),
    ]);
    assert!(output.status.success(), "{output:?}");
    let queued: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = queued["id"].as_i64().unwrap().to_string();
    assert_eq!(queued["status"], "queued");
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    let output = cli(&[
        "jobs".as_ref(),
        "--journal".as_ref(),
        journal.as_os_str(),
        "--json".as_ref(),
    ]);
    assert!(output.status.success(), "{output:?}");
    let jobs: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(jobs.as_array().unwrap().len(), 1);
    for _ in 0..2 {
        let output = cli(&[
            "resume".as_ref(),
            id.as_ref(),
            "--journal".as_ref(),
            journal.as_os_str(),
            "--json".as_ref(),
        ]);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["status"],
            "completed"
        );
        assert_eq!(fs::read(destination.join("file")).unwrap(), b"durable");
    }
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 1);
}

#[test]
fn cli_durable_conflict_is_not_false_success_and_keeps_job_for_inspection() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("file");
    let destination = root.path().join("dest");
    let journal = root.path().join("jobs.sqlite");
    fs::create_dir(&destination).unwrap();
    fs::write(&source, b"new").unwrap();
    fs::write(destination.join("file"), b"original").unwrap();
    let output = cli(&[
        "copy".as_ref(),
        source.as_os_str(),
        destination.as_os_str(),
        "--journal".as_ref(),
        journal.as_os_str(),
        "--json".as_ref(),
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "interrupted");
    assert!(report["job_id"].is_i64());
    assert!(report["error"].is_string());
    assert_eq!(fs::read(destination.join("file")).unwrap(), b"original");
}

fn cli(args: &[&std::ffi::OsStr]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vartacopy"));
    command.args(args);
    if args.first().is_some_and(|arg| *arg == "copy") && !args.iter().any(|arg| *arg == "--journal")
    {
        let path = std::path::Path::new(args[1])
            .parent()
            .unwrap()
            .join(".test-jobs.sqlite");
        command.arg("--journal").arg(path);
    }
    command.output().unwrap()
}

#[test]
fn cli_dry_run_copy_conflict_skip_and_verify_exit_codes() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("тест.dat");
    let dest = root.path().join("destination");
    fs::create_dir(&dest).unwrap();
    fs::write(&source, b"five!").unwrap();
    let output = cli(&[
        "copy".as_ref(),
        source.as_os_str(),
        dest.as_os_str(),
        "--dry-run".as_ref(),
        "--json".as_ref(),
    ]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
    let output = cli(&[
        "copy".as_ref(),
        source.as_os_str(),
        dest.as_os_str(),
        "--json".as_ref(),
    ]);
    assert!(output.status.success(), "{output:?}");
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "completed");
    assert!(result["destination"]["windows_utf16"].is_array());

    let output = cli(&[
        "copy".as_ref(),
        source.as_os_str(),
        dest.as_os_str(),
        "--json".as_ref(),
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["status"],
        "interrupted"
    );
    let output = cli(&[
        "copy".as_ref(),
        source.as_os_str(),
        dest.as_os_str(),
        "--on-conflict".as_ref(),
        "skip".as_ref(),
        "--json".as_ref(),
    ]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["status"],
        "partial"
    );

    let target = dest.join(source.file_name().unwrap());
    let output = cli(&[
        "verify".as_ref(),
        source.as_os_str(),
        target.as_os_str(),
        "--json".as_ref(),
    ]);
    assert!(output.status.success(), "{output:?}");
    fs::write(&target, b"wrong").unwrap();
    let output = cli(&[
        "verify".as_ref(),
        source.as_os_str(),
        target.as_os_str(),
        "--json".as_ref(),
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
}

#[test]
fn cli_missing_source_and_overlapping_directory_fail_without_writing() {
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("dest");
    fs::create_dir(&dest).unwrap();
    let missing = root.path().join("missing");
    let output = cli(&["copy".as_ref(), missing.as_os_str(), dest.as_os_str()]);
    assert_eq!(output.status.code(), Some(1));
    let output = cli(&["copy".as_ref(), root.path().as_os_str(), dest.as_os_str()]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(fs::read_dir(dest).unwrap().count(), 0);
}

#[test]
fn junction_destination_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    let link = root.path().join("junction");
    let source = root.path().join("source");
    fs::create_dir(&real).unwrap();
    fs::write(&source, b"data").unwrap();
    let command = format!("mklink /J \"{}\" \"{}\"", link.display(), real.display());
    let result = Command::new("cmd.exe")
        .raw_arg("/d /c")
        .raw_arg(&command)
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    let result = cli(&[
        "copy".as_ref(),
        source.as_os_str(),
        link.as_os_str(),
        "--json".as_ref(),
    ]);
    assert_eq!(result.status.code(), Some(1), "{result:?}");
    assert_eq!(fs::read_dir(&real).unwrap().count(), 0);
    // Remove only the junction itself so TempDir cleanup never traverses its target.
    fs::remove_dir(link).unwrap();
}

#[test]
fn cli_tree_copy_dry_run_and_conflicts_report_truthful_counts() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("tree");
    let destination = root.path().join("destination");
    fs::create_dir_all(source.join("nested/empty")).unwrap();
    fs::create_dir(&destination).unwrap();
    fs::write(source.join("nested/file"), b"tree-data").unwrap();
    let output = cli(&[
        "copy".as_ref(),
        source.as_os_str(),
        destination.as_os_str(),
        "--dry-run".as_ref(),
        "--json".as_ref(),
    ]);
    assert!(output.status.success(), "{output:?}");
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["status"], "ready");
    assert_eq!(plan["totals"]["files"], 1);
    assert_eq!(plan["totals"]["directories"], 3);
    assert_eq!(plan["entries"].as_array().unwrap().len(), 4);
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    let output = cli(&[
        "copy".as_ref(),
        source.as_os_str(),
        destination.as_os_str(),
        "--json".as_ref(),
    ]);
    assert!(output.status.success(), "{output:?}");
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["entries_committed"], 4);
    assert_eq!(result["entries_failed"], 0);
    assert_eq!(
        fs::read(destination.join("tree/nested/file")).unwrap(),
        b"tree-data"
    );
    let output = cli(&[
        "copy".as_ref(),
        source.as_os_str(),
        destination.as_os_str(),
        "--on-conflict".as_ref(),
        "skip".as_ref(),
        "--json".as_ref(),
    ]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "partial");
    assert_eq!(result["entries_committed"], 0);
    assert_eq!(result["verified_bytes"], 0);
    assert_eq!(result["entries_skipped"], 4);
}

#[test]
fn cli_reparse_skip_is_partial_and_limits_never_write() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("tree");
    let destination = root.path().join("destination");
    let outside = root.path().join("outside");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&destination).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(source.join("normal"), b"data").unwrap();
    let link = source.join("link");
    let command = format!("mklink /J \"{}\" \"{}\"", link.display(), outside.display());
    let result = Command::new("cmd.exe")
        .raw_arg("/d /c")
        .raw_arg(&command)
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    let output = cli(&[
        "copy".as_ref(),
        source.as_os_str(),
        destination.as_os_str(),
        "--json".as_ref(),
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["status"],
        "failed"
    );
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    let output = cli(&[
        "copy".as_ref(),
        source.as_os_str(),
        destination.as_os_str(),
        "--max-entries".as_ref(),
        "1".as_ref(),
        "--json".as_ref(),
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stderr).unwrap()["status"],
        "failed"
    );
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    let output = cli(&[
        "copy".as_ref(),
        source.as_os_str(),
        destination.as_os_str(),
        "--reparse".as_ref(),
        "skip".as_ref(),
        "--json".as_ref(),
    ]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "partial");
    assert_eq!(result["entries_committed"], 2);
    assert_eq!(result["entries_skipped"], 1);
    assert!(!destination.join("tree/link").exists());
    fs::remove_dir(link).unwrap();
}

#[test]
fn cli_queue_controls_reports_and_recheck_without_source() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("dest");
    fs::create_dir(&destination).unwrap();
    let journal = root.path().join("jobs.sqlite");
    for name in ["one", "two"] {
        let source = root.path().join(name);
        fs::write(&source, name.as_bytes()).unwrap();
        let output = cli(&[
            "copy".as_ref(),
            source.as_os_str(),
            destination.as_os_str(),
            "--enqueue".as_ref(),
            "--journal".as_ref(),
            journal.as_os_str(),
            "--json".as_ref(),
        ]);
        assert!(output.status.success(), "{output:?}");
    }
    let call = |args: &[&std::ffi::OsStr]| {
        let mut all = vec!["--journal".as_ref(), journal.as_os_str()];
        all.extend_from_slice(args);
        cli(&all)
    };
    for args in [
        vec!["pause", "1", "--json"],
        vec!["schedule", "1", "--json"],
        vec!["order", "2", "up", "--json"],
    ] {
        let output = call(&args.iter().map(|s| s.as_ref()).collect::<Vec<_>>());
        assert!(output.status.success(), "{output:?}");
    }
    let output = call(&["queue".as_ref(), "--json".as_ref()]);
    let jobs: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(jobs[0]["id"], 2);
    let output = call(&["queue".as_ref(), "--run".as_ref(), "--json".as_ref()]);
    assert!(output.status.success(), "{output:?}");
    let reports = String::from_utf8(output.stdout).unwrap();
    assert_eq!(reports.lines().count(), 2);
    assert_eq!(fs::read(destination.join("one")).unwrap(), b"one");
    assert_eq!(fs::read(destination.join("two")).unwrap(), b"two");
    fs::remove_file(root.path().join("one")).unwrap();
    let output = call(&[
        "schedule".as_ref(),
        "1".as_ref(),
        "--recheck".as_ref(),
        "--json".as_ref(),
    ]);
    assert!(output.status.success(), "{output:?}");
    let output = call(&["queue".as_ref(), "--run".as_ref(), "--json".as_ref()]);
    assert!(output.status.success(), "{output:?}");
    fs::write(destination.join("one"), b"bad").unwrap();
    assert!(
        call(&["schedule".as_ref(), "1".as_ref(), "--recheck".as_ref()])
            .status
            .success()
    );
    let output = call(&["queue".as_ref(), "--run".as_ref(), "--json".as_ref()]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let output = call(&["show".as_ref(), "1".as_ref(), "--json".as_ref()]);
    let details: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(details["job"]["status"], "interrupted");
    assert_eq!(details["issues"].as_array().unwrap().len(), 1);
    let report = root.path().join("report.json");
    let output = call(&[
        "export".as_ref(),
        "1".as_ref(),
        report.as_os_str(),
        "--json".as_ref(),
    ]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(&report).unwrap()).unwrap()["summary"]
            ["job"]["status"],
        "interrupted"
    );
    assert_eq!(
        call(&["export".as_ref(), "1".as_ref(), report.as_os_str()])
            .status
            .code(),
        Some(1)
    );
    fs::write(destination.join("one"), b"one").unwrap();
    assert!(call(&["schedule".as_ref(), "1".as_ref()]).status.success());
    assert!(call(&["queue".as_ref(), "--run".as_ref()]).status.success());
    let output = call(&["show".as_ref(), "1".as_ref(), "--json".as_ref()]);
    let details: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(details["job"]["status"], "completed");
    assert!(details["issues"].as_array().unwrap().is_empty());
}
