use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::os::windows::{ffi::OsStringExt, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};
use vartacopy_core::{
    Conflict, Error,
    planner::{self, EntryKind, ReparsePolicy, ScanLimits, ScanOptions},
    tree,
};
use vartacopy_platform_windows as platform;

#[test]
fn locked_file_is_an_explicit_preflight_failure() {
    use std::os::windows::fs::OpenOptionsExt;
    let f = Fixture::new();
    let file = f.source.join("locked");
    fs::write(&file, b"locked").unwrap();
    let _lock = File::options()
        .write(true)
        .share_mode(0)
        .open(&file)
        .unwrap();
    let plan = f.scan();
    assert!(!plan.is_ready());
    assert_eq!(plan.totals.unsupported, 1);
    let report = tree::execute(&plan, Conflict::Error, &cancel()).unwrap();
    assert_eq!(report.status, "failed");
    assert_eq!(report.exit_code(), 1);
    assert!(!f.output().exists());
}

#[test]
fn swapped_child_directory_is_blocked_without_touching_replacement() {
    let f = Fixture::new();
    let child = f.source.join("child");
    fs::create_dir(&child).unwrap();
    fs::write(child.join("original"), b"original").unwrap();
    fs::write(f.source.join("good"), b"good").unwrap();
    let root_time = fs::metadata(&f.source).unwrap().modified().unwrap();
    let plan = f.scan();
    fs::rename(&child, f._root.path().join("moved")).unwrap();
    fs::create_dir(&child).unwrap();
    fs::write(child.join("replacement"), b"untouched").unwrap();
    platform::open_directory(&f.source, true)
        .unwrap()
        .set_modified(root_time)
        .unwrap();
    let report = tree::execute(&plan, Conflict::Error, &cancel()).unwrap();
    assert_eq!(report.status, "partial", "{report:?}");
    assert_eq!(report.entries_failed, 2);
    assert_eq!(report.files_copied, 1);
    assert!(!f.output().join("child").exists());
    assert_eq!(fs::read(child.join("replacement")).unwrap(), b"untouched");
}

struct Fixture {
    _root: tempfile::TempDir,
    source: PathBuf,
    dest: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("джерело");
        let dest = root.path().join("ціль");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&dest).unwrap();
        Self {
            _root: root,
            source,
            dest,
        }
    }
    fn scan(&self) -> planner::OperationPlan {
        planner::scan(&self.source, &self.dest, ScanOptions::default(), &cancel()).unwrap()
    }
    fn output(&self) -> PathBuf {
        self.dest.join(self.source.file_name().unwrap())
    }
}
fn cancel() -> AtomicBool {
    AtomicBool::new(false)
}

fn junction(link: &Path, target: &Path) {
    let command = format!("mklink /J \"{}\" \"{}\"", link.display(), target.display());
    let output = Command::new("cmd.exe")
        .raw_arg("/d /c")
        .raw_arg(command)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn copies_nested_unicode_ads_empty_dirs_and_directory_times_after_children() {
    let f = Fixture::new();
    let sub = f.source.join("вкладена");
    fs::create_dir(&sub).unwrap();
    fs::create_dir(sub.join("порожня")).unwrap();
    fs::write(f.source.join("zero"), []).unwrap();
    fs::write(sub.join("дані"), b"content").unwrap();
    fs::write(
        platform::stream_path(&sub.join("дані"), OsStr::new(":коментар")),
        b"ads",
    )
    .unwrap();
    let timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    for dir in [&f.source, &sub] {
        platform::open_directory(dir, true)
            .unwrap()
            .set_modified(timestamp)
            .unwrap();
    }
    let plan = f.scan();
    assert!(plan.is_ready());
    assert_eq!(plan.totals.files, 2);
    assert_eq!(plan.totals.directories, 3);
    assert_eq!(plan.totals.streams, 3);
    assert_eq!(plan.totals.bytes, 10);
    assert_eq!(fs::read_dir(&f.dest).unwrap().count(), 0);
    let report = tree::execute(&plan, Conflict::Error, &cancel()).unwrap();
    assert_eq!(report.status, "copied_verified", "{report:?}");
    assert_eq!(report.files_copied, 2);
    assert_eq!(report.directories_created, 3);
    assert_eq!(report.entries_not_attempted, 0);
    assert!(f.output().join("вкладена/порожня").is_dir());
    assert_eq!(
        fs::read(f.output().join("вкладена/дані")).unwrap(),
        b"content"
    );
    assert_eq!(
        fs::read(platform::stream_path(
            &f.output().join("вкладена/дані"),
            OsStr::new(":коментар")
        ))
        .unwrap(),
        b"ads"
    );
    assert_eq!(
        fs::metadata(f.output()).unwrap().modified().unwrap(),
        timestamp
    );
    assert_eq!(
        fs::metadata(f.output().join("вкладена"))
            .unwrap()
            .modified()
            .unwrap(),
        timestamp
    );
}

#[test]
fn copies_an_entirely_empty_tree() {
    let f = Fixture::new();
    let result = tree::execute(&f.scan(), Conflict::Error, &cancel()).unwrap();
    assert_eq!(result.exit_code(), 0, "{result:?}");
    assert_eq!(result.files_copied, 0);
    assert!(f.output().is_dir());
}

#[test]
fn root_conflicts_never_merge_and_keep_both_keeps_directory_extension() {
    let mut f = Fixture::new();
    let new_source = f.source.with_extension("folder");
    fs::rename(&f.source, &new_source).unwrap();
    f.source = new_source;
    fs::write(f.source.join("new"), b"new").unwrap();
    fs::create_dir(f.output()).unwrap();
    fs::write(f.output().join("old"), b"old").unwrap();
    fs::create_dir(f.dest.join("джерело.folder (1)")).unwrap();
    let plan = f.scan();
    assert!(tree::execute(&plan, Conflict::Error, &cancel()).is_err());
    assert!(!f.output().join("new").exists());
    let report = tree::execute(&plan, Conflict::Skip, &cancel()).unwrap();
    assert_eq!(report.status, "skipped_unverified");
    assert_eq!(report.files_skipped, 1);
    assert_eq!(report.exit_code(), 2);
    let report = tree::execute(&plan, Conflict::KeepBoth, &cancel()).unwrap();
    assert_eq!(report.exit_code(), 0, "{report:?}");
    assert_eq!(fs::read(f.output().join("old")).unwrap(), b"old");
    assert_eq!(
        fs::read(f.dest.join("джерело.folder (2)/new")).unwrap(),
        b"new"
    );
    assert!(!f.output().join("new").exists());
}

#[test]
fn rejects_overlap_by_identity_even_with_different_case() {
    let f = Fixture::new();
    let inside = f.source.join("inside");
    fs::create_dir(&inside).unwrap();
    let differently_cased = PathBuf::from(inside.to_string_lossy().to_uppercase());
    assert!(
        planner::scan(
            &f.source,
            &differently_cased,
            ScanOptions::default(),
            &cancel()
        )
        .is_err()
    );
    assert!(planner::scan(&f.source, &f.source, ScanOptions::default(), &cancel()).is_err());
    assert_eq!(fs::read_dir(&inside).unwrap().count(), 0);
}

#[test]
fn entry_depth_memory_and_cancellation_limits_abort_before_writes() {
    let f = Fixture::new();
    fs::write(f.source.join("one"), b"one").unwrap();
    fs::create_dir(f.source.join("nested")).unwrap();
    for limits in [
        ScanLimits {
            max_entries: 1,
            ..ScanLimits::default()
        },
        ScanLimits {
            max_depth: 0,
            ..ScanLimits::default()
        },
        ScanLimits {
            max_plan_bytes: 1,
            ..ScanLimits::default()
        },
    ] {
        let result = planner::scan(
            &f.source,
            &f.dest,
            ScanOptions {
                limits,
                ..ScanOptions::default()
            },
            &cancel(),
        );
        assert!(matches!(result, Err(Error::ScanLimit { .. })), "{result:?}");
        assert_eq!(fs::read_dir(&f.dest).unwrap().count(), 0);
    }
    let stop = AtomicBool::new(true);
    assert!(matches!(
        planner::scan(&f.source, &f.dest, ScanOptions::default(), &stop),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        tree::execute(&f.scan(), Conflict::Error, &stop),
        Err(Error::Cancelled)
    ));
    assert_eq!(fs::read_dir(&f.dest).unwrap().count(), 0);
}

#[test]
fn junction_is_blocked_by_default_and_explicit_skip_is_partial() {
    let f = Fixture::new();
    let outside = f._root.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret"), b"outside").unwrap();
    fs::write(f.source.join("normal"), b"normal").unwrap();
    let link = f.source.join("link");
    junction(&link, &outside);
    let plan = f.scan();
    assert!(!plan.is_ready());
    assert_eq!(plan.totals.reparse_points, 1);
    let report = tree::execute(&plan, Conflict::Error, &cancel()).unwrap();
    assert_eq!(report.exit_code(), 1);
    assert_eq!(fs::read_dir(&f.dest).unwrap().count(), 0);
    let plan = planner::scan(
        &f.source,
        &f.dest,
        ScanOptions {
            reparse: ReparsePolicy::Skip,
            ..ScanOptions::default()
        },
        &cancel(),
    )
    .unwrap();
    assert!(plan.is_ready());
    let report = tree::execute(&plan, Conflict::Error, &cancel()).unwrap();
    assert_eq!(report.status, "partial", "{report:?}");
    assert_eq!(report.entries_skipped, 1);
    assert_eq!(report.files_copied, 1);
    assert_eq!(report.exit_code(), 2);
    assert!(!f.output().join("link").exists());
    assert_eq!(fs::read(outside.join("secret")).unwrap(), b"outside");
    fs::remove_dir(link).unwrap();
}

#[test]
fn directory_ads_are_an_explicit_blocker() {
    let f = Fixture::new();
    fs::write(
        platform::stream_path(&f.source, OsStr::new(":dir-metadata")),
        b"important",
    )
    .unwrap();
    let plan = f.scan();
    assert!(!plan.is_ready());
    assert_eq!(plan.totals.unsupported, 1);
    let report = tree::execute(&plan, Conflict::Error, &cancel()).unwrap();
    assert_eq!(report.exit_code(), 1);
    assert!(report.details[0].message.contains("directory ADS"));
    assert!(!f.output().exists());
}

#[test]
fn hardlinks_are_reported_and_materialized_as_independent_files() {
    let f = Fixture::new();
    fs::write(f.source.join("first"), b"shared").unwrap();
    fs::hard_link(f.source.join("first"), f.source.join("alias")).unwrap();
    let plan = f.scan();
    assert_eq!(plan.totals.hardlink_copies, 1);
    let report = tree::execute(&plan, Conflict::Error, &cancel()).unwrap();
    assert_eq!(report.files_copied, 2, "{report:?}");
    let first = platform::open_source(&f.output().join("first")).unwrap();
    let alias = platform::open_source(&f.output().join("alias")).unwrap();
    assert_ne!(
        platform::identity(&first).unwrap(),
        platform::identity(&alias).unwrap()
    );
    assert_eq!(fs::read(f.output().join("alias")).unwrap(), b"shared");
}

#[test]
fn modified_file_fails_while_other_files_continue() {
    let f = Fixture::new();
    let changed = f.source.join("changed");
    fs::write(&changed, b"old").unwrap();
    fs::write(f.source.join("good"), b"good").unwrap();
    let plan = f.scan();
    fs::write(&changed, b"changed size").unwrap();
    let report = tree::execute(&plan, Conflict::Error, &cancel()).unwrap();
    assert_eq!(report.status, "partial", "{report:?}");
    assert_eq!(report.entries_failed, 1);
    assert_eq!(report.files_copied, 1);
    assert_eq!(fs::read(f.output().join("good")).unwrap(), b"good");
    assert!(!f.output().join("changed").exists());
}

#[test]
fn replaced_single_file_with_same_size_and_time_fails_by_file_id() {
    let f = Fixture::new();
    let source = f.source.join("file");
    fs::write(&source, b"same").unwrap();
    let timestamp = fs::metadata(&source).unwrap().modified().unwrap();
    let plan = planner::scan(&source, &f.dest, ScanOptions::default(), &cancel()).unwrap();
    fs::rename(&source, f.source.join("old")).unwrap();
    fs::write(&source, b"evil").unwrap();
    File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_modified(timestamp)
        .unwrap();
    assert!(matches!(
        tree::execute(&plan, Conflict::Error, &cancel()),
        Err(Error::Verification(_))
    ));
    assert!(!f.dest.join("file").exists());
}

#[test]
fn replaced_destination_directory_is_refused_before_copy() {
    let f = Fixture::new();
    fs::write(f.source.join("file"), b"bytes").unwrap();
    let plan = f.scan();
    fs::rename(&f.dest, f.dest.with_extension("old")).unwrap();
    fs::create_dir(&f.dest).unwrap();
    assert!(matches!(
        tree::execute(&plan, Conflict::Error, &cancel()),
        Err(Error::Verification(_))
    ));
    assert_eq!(fs::read_dir(&f.dest).unwrap().count(), 0);
}

#[test]
fn copies_long_paths_and_preserves_native_utf16_names() {
    let f = Fixture::new();
    let mut path = f.source.clone();
    for number in 0..12 {
        path = path.join(format!("long-directory-{number:02}-abcdefghijklmnop"));
    }
    fs::create_dir_all(&path).unwrap();
    let name = OsString::from_wide(&[0x0061, 0xD800, 0x0062]);
    let file = path.join(&name);
    fs::write(&file, b"native path").unwrap();
    assert!(file.as_os_str().len() > 260);
    let plan = f.scan();
    assert_eq!(
        plan.entries()
            .iter()
            .filter(|e| e.kind() == EntryKind::File)
            .count(),
        1
    );
    let report = tree::execute(&plan, Conflict::Error, &cancel()).unwrap();
    assert_eq!(report.exit_code(), 0, "{report:?}");
    let relative = file.strip_prefix(&f.source).unwrap();
    assert_eq!(fs::read(f.output().join(relative)).unwrap(), b"native path");
}
