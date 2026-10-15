use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use vartacopy_core::{
    Conflict, Error,
    control::Control,
    journal::{CHUNK_BYTES, Journal},
    planner::{ReparsePolicy, ScanOptions, scan},
    recovery::{self, Phase},
};

struct Fixture {
    _root: tempfile::TempDir,
    source: PathBuf,
    dest: PathBuf,
    journal: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("дерево");
        let dest = root.path().join("dest");
        fs::create_dir_all(source.join("nested/empty")).unwrap();
        fs::create_dir(&dest).unwrap();
        fs::write(source.join("small"), b"small").unwrap();
        fs::write(source.join("nested/large"), vec![37; CHUNK_BYTES + 19]).unwrap();
        let stream = source.join("nested/large").into_os_string();
        let mut stream = stream;
        stream.push(":ads");
        fs::write(PathBuf::from(stream), b"alternate").unwrap();
        let journal = root.path().join("jobs.sqlite");
        Self {
            _root: root,
            source,
            dest,
            journal,
        }
    }
    fn enqueue(&self, conflict: Conflict) -> (Journal, i64) {
        let plan = scan(
            &self.source,
            &self.dest,
            ScanOptions::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let mut journal = Journal::open(&self.journal).unwrap();
        let id = journal.enqueue(&plan, conflict).unwrap();
        (journal, id)
    }
    fn target(&self) -> PathBuf {
        self.dest.join("дерево")
    }
    fn check(&self, target: &std::path::Path) {
        assert_eq!(fs::read(target.join("small")).unwrap(), b"small");
        assert_eq!(
            fs::read(target.join("nested/large")).unwrap(),
            vec![37; CHUNK_BYTES + 19]
        );
        let mut stream = target.join("nested/large").into_os_string();
        stream.push(":ads");
        assert_eq!(fs::read(PathBuf::from(stream)).unwrap(), b"alternate");
        assert!(target.join("nested/empty").is_dir());
    }
}

#[test]
fn durable_tree_recovers_directory_and_file_boundaries_without_merging() {
    for phase in [
        Phase::DirectoryIntent,
        Phase::DirectoryPublished,
        Phase::DirectoryReady,
        Phase::CheckpointCommitted,
        Phase::Published,
        Phase::DirectoryCommitted,
    ] {
        let f = Fixture::new();
        let (mut journal, id) = f.enqueue(Conflict::Error);
        let mut hit = false;
        let report = recovery::run(&mut journal, id, &Control::default(), &mut |event| {
            if !hit && event.phase == phase {
                hit = true;
                return Err(Error::Journal("injected".into()));
            }
            Ok(())
        })
        .unwrap();
        assert!(hit, "{phase:?}");
        assert_eq!(report.status, "interrupted", "{phase:?}: {report:?}");
        drop(journal);
        let mut journal = Journal::open(&f.journal).unwrap();
        let report = recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
        assert_eq!(report.status, "completed", "{phase:?}: {report:?}");
        assert_eq!(report.entries_pending, 0);
        f.check(&f.target());
        let report = recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
        assert_eq!(report.status, "completed", "{phase:?}: {report:?}");
        f.check(&f.target());
    }
}

#[test]
fn durable_root_skip_and_keep_both_preserve_foreign_tree() {
    for conflict in [Conflict::Skip, Conflict::KeepBoth] {
        let f = Fixture::new();
        let (mut journal, id) = f.enqueue(conflict);
        fs::create_dir(f.target()).unwrap();
        fs::write(f.target().join("foreign"), b"keep").unwrap();
        let report = recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
        assert_eq!(fs::read(f.target().join("foreign")).unwrap(), b"keep");
        if matches!(conflict, Conflict::Skip) {
            assert_eq!(report.status, "partial", "{report:?}");
            assert_eq!(report.entries_committed, 0);
            assert_eq!(report.entries_pending, 0);
            assert!(!f.target().join("small").exists());
        } else {
            assert_eq!(report.status, "completed", "{report:?}");
            f.check(&f.dest.join("дерево (1)"));
        }
    }
}

#[test]
fn completed_tree_reconciles_without_source_but_rejects_replaced_root() {
    let f = Fixture::new();
    let (mut journal, id) = f.enqueue(Conflict::Error);
    assert_eq!(
        recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(()))
            .unwrap()
            .status,
        "completed"
    );
    fs::rename(&f.source, f._root.path().join("source-moved")).unwrap();
    let report = recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "completed", "{report:?}");
    fs::rename(f.target(), f.dest.join("owned-moved")).unwrap();
    fs::create_dir(f.target()).unwrap();
    fs::write(f.target().join("foreign"), b"untouched").unwrap();
    let report = recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "interrupted", "{report:?}");
    assert_eq!(fs::read(f.target().join("foreign")).unwrap(), b"untouched");
}

#[test]
fn one_changed_source_file_does_not_stop_other_files_and_remains_pending() {
    let f = Fixture::new();
    let (mut journal, id) = f.enqueue(Conflict::Error);
    fs::write(f.source.join("small"), b"longer changed content").unwrap();
    let report = recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "interrupted", "{report:?}");
    assert_eq!(report.entries_failed, 1);
    assert_eq!(report.entries_pending, 1);
    assert!(!f.target().join("small").exists());
    assert_eq!(
        fs::read(f.target().join("nested/large")).unwrap(),
        vec![37; CHUNK_BYTES + 19]
    );
}

#[test]
fn tree_skip_reparse_is_persisted_and_partial() {
    use std::os::windows::process::CommandExt;
    let f = Fixture::new();
    let link = f.source.join("link");
    let command = format!("mklink /J \"{}\" \"{}\"", link.display(), f.dest.display());
    let result = std::process::Command::new("cmd.exe")
        .raw_arg("/d /c")
        .raw_arg(command)
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    let plan = scan(
        &f.source,
        &f.dest,
        ScanOptions {
            reparse: ReparsePolicy::Skip,
            ..ScanOptions::default()
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    let mut journal = Journal::open(&f.journal).unwrap();
    let id = journal.enqueue(&plan, Conflict::Error).unwrap();
    let report = recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "partial", "{report:?}");
    assert_eq!(report.entries_skipped, 1);
    assert_eq!(report.entries_pending, 0);
    assert!(!f.target().join("link").exists());
    fs::remove_dir(link).unwrap();
}
