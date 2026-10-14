use super::*;
use crate::planner::{ScanOptions, scan};
use std::ffi::OsStr;
use std::fs;
use std::sync::atomic::AtomicBool;

struct Fixture {
    _root: tempfile::TempDir,
    source: PathBuf,
    dest: PathBuf,
    journal: PathBuf,
    data: Vec<u8>,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("джерело.bin");
        let dest = root.path().join("destination");
        fs::create_dir(&dest).unwrap();
        let data = (0..CHUNK_BYTES * 2 + 31)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>();
        fs::write(&source, &data).unwrap();
        fs::write(
            platform::stream_path(&source, OsStr::new(":опис")),
            b"alternate stream",
        )
        .unwrap();
        let journal = root.path().join("jobs.sqlite");
        Self {
            _root: root,
            source,
            dest,
            journal,
            data,
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
        self.dest.join(self.source.file_name().unwrap())
    }
    fn assert_data(&self) {
        assert_eq!(fs::read(self.target()).unwrap(), self.data);
        assert_eq!(
            fs::read(platform::stream_path(&self.target(), OsStr::new(":опис"))).unwrap(),
            b"alternate stream"
        );
    }
}

#[test]
fn recovers_at_every_file_protocol_boundary_and_is_idempotent() {
    for phase in [
        Phase::StageRecorded,
        Phase::BlockWritten,
        Phase::BlockFlushed,
        Phase::CheckpointCommitted,
        Phase::Verified,
        Phase::PublishIntent,
        Phase::Published,
        Phase::Committed,
    ] {
        let f = Fixture::new();
        let (mut journal, id) = f.enqueue(Conflict::Error);
        let mut hit = false;
        let report = run(&mut journal, id, &Control::default(), &mut |event| {
            if !hit && event.phase == phase {
                hit = true;
                return Err(Error::Journal("injected stop".into()));
            }
            Ok(())
        })
        .unwrap();
        assert!(hit, "{phase:?}");
        assert_eq!(report.status, "interrupted", "{phase:?}: {report:?}");
        drop(journal);
        let mut journal = Journal::open(&f.journal).unwrap();
        let report = run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
        assert_eq!(report.status, "completed", "{phase:?}: {report:?}");
        if phase == Phase::CheckpointCommitted {
            assert_eq!(report.resumed_bytes, CHUNK_BYTES as u64);
        }
        f.assert_data();
        let report = run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
        assert_eq!(report.status, "completed", "{phase:?}: {report:?}");
        assert_eq!(report.entries_committed, 1);
        f.assert_data();
    }
}

#[test]
fn resume_rehashes_source_even_when_size_and_mtime_are_restored() {
    let f = Fixture::new();
    let (mut journal, id) = f.enqueue(Conflict::Error);
    let report = run(&mut journal, id, &Control::default(), &mut |event| {
        if event.phase == Phase::CheckpointCommitted {
            return Err(Error::Journal("stop".into()));
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(report.status, "interrupted");
    let modified = fs::metadata(&f.source).unwrap().modified().unwrap();
    let mut file = File::options().write(true).open(&f.source).unwrap();
    file.write_all(b"CHANGED").unwrap();
    file.set_modified(modified).unwrap();
    drop(file);
    let report = run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "interrupted", "{report:?}");
    assert!(report.error.unwrap().contains("resume prefix"));
    assert!(!f.target().exists());
}

#[test]
fn corrupt_stage_and_replaced_stage_are_never_published_or_deleted() {
    let f = Fixture::new();
    let (mut journal, id) = f.enqueue(Conflict::Error);
    run(&mut journal, id, &Control::default(), &mut |e| {
        if e.phase == Phase::CheckpointCommitted {
            return Err(Error::Journal("stop".into()));
        }
        Ok(())
    })
    .unwrap();
    let entry = journal.entry(id, 0).unwrap();
    let stage = entry.stage.unwrap();
    let mut file = File::options().write(true).open(&stage).unwrap();
    file.write_all(b"CORRUPT").unwrap();
    drop(file);
    let report = run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "interrupted", "{report:?}");
    assert!(!f.target().exists());
    fs::rename(&stage, stage.with_extension("owned")).unwrap();
    fs::write(&stage, b"foreign").unwrap();
    let report = run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "interrupted", "{report:?}");
    assert_eq!(fs::read(stage).unwrap(), b"foreign");
}

#[test]
fn publication_reconciles_without_source_and_rejects_corrupt_or_replaced_final() {
    let f = Fixture::new();
    let (mut journal, id) = f.enqueue(Conflict::Error);
    run(&mut journal, id, &Control::default(), &mut |event| {
        if event.phase == Phase::Published {
            return Err(Error::Journal("stop".into()));
        }
        Ok(())
    })
    .unwrap();
    fs::remove_file(&f.source).unwrap();
    let report = run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "completed", "{report:?}");
    f.assert_data();
    let mut file = File::options().write(true).open(f.target()).unwrap();
    file.write_all(b"bad").unwrap();
    drop(file);
    let report = run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "interrupted", "{report:?}");
    fs::rename(f.target(), f.target().with_extension("owned")).unwrap();
    fs::write(f.target(), b"foreign").unwrap();
    let report = run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "interrupted", "{report:?}");
    assert_eq!(fs::read(f.target()).unwrap(), b"foreign");
}

#[test]
fn persisted_conflict_policy_preserves_existing_data() {
    for conflict in [Conflict::Skip, Conflict::KeepBoth] {
        let f = Fixture::new();
        let (mut journal, id) = f.enqueue(conflict);
        fs::write(f.target(), b"original").unwrap();
        let report = run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
        assert_eq!(fs::read(f.target()).unwrap(), b"original");
        match conflict {
            Conflict::Skip => {
                assert_eq!(report.status, "partial", "{report:?}");
                assert_eq!(report.entries_skipped, 1);
            }
            Conflict::KeepBoth => {
                assert_eq!(report.status, "completed", "{report:?}");
                assert_eq!(fs::read(f.dest.join("джерело (1).bin")).unwrap(), f.data);
                let report = run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
                assert_eq!(report.status, "completed", "{report:?}");
                assert!(!f.dest.join("джерело (2).bin").exists());
            }
            Conflict::Error => unreachable!(),
        }
    }
}

#[test]
fn cooperative_cancel_retains_owned_checkpoint_for_resume() {
    let f = Fixture::new();
    let (mut journal, id) = f.enqueue(Conflict::Error);
    let control = Control::default();
    let report = run(&mut journal, id, &control, &mut |event| {
        if event.phase == Phase::CheckpointCommitted {
            control.cancel();
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(report.status, "paused", "{report:?}");
    assert_eq!(report.exit_code(), 130);
    assert!(!f.target().exists());
    let report = run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "completed", "{report:?}");
    assert_eq!(report.resumed_bytes, CHUNK_BYTES as u64);
    f.assert_data();
}

#[test]
#[allow(
    clippy::permissions_set_readonly_false,
    reason = "Windows-only test fixture cleanup"
)]
fn readonly_verified_stage_is_recovered_by_identity() {
    let f = Fixture::new();
    let mut perms = fs::metadata(&f.source).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&f.source, perms).unwrap();
    let (mut journal, id) = f.enqueue(Conflict::Error);
    run(&mut journal, id, &Control::default(), &mut |event| {
        if event.phase == Phase::PublishIntent {
            return Err(Error::Journal("stop".into()));
        }
        Ok(())
    })
    .unwrap();
    let stage = journal.entry(id, 0).unwrap().stage.unwrap();
    assert!(fs::metadata(&stage).unwrap().permissions().readonly());
    let report = run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "completed", "{report:?}");
    assert!(fs::metadata(f.target()).unwrap().permissions().readonly());
    for path in [&f.source, &f.target()] {
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_readonly(false);
        fs::set_permissions(path, perms).unwrap();
    }
}
