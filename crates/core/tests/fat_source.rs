//! Opt-in hardware probe: only temporary fixtures within an explicitly supplied
//! FAT/FAT32 directory are created. Never formats, enumerates, or deletes user data.
use std::{fs, path::PathBuf, sync::atomic::AtomicBool};
use vartacopy_core::{
    Conflict,
    control::Control,
    journal::{Journal, OperationKind},
    planner::{self, ScanOptions},
    recovery::{self, Phase},
};
use vartacopy_platform_windows as platform;

#[test]
fn path_scoped_source_identity_never_authorizes_staging_or_source_deletion() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("keep.txt");
    fs::write(&source, b"keep source").unwrap();
    let legacy = platform::Identity {
        volume_serial: 1,
        file_id: [2; 16],
        kind: platform::IdentityKind::FatSource,
    };
    assert!(platform::CleanupOwner::open(&source, &legacy, false, true).is_err());
    assert!(platform::identity_exists_on_source_volume(&source, &legacy).is_err());
    assert!(platform::Stage::reopen(&source, &legacy).is_err());
    assert!(platform::Stage::reopen_directory(root.path(), &legacy).is_err());
    assert_eq!(fs::read(&source).unwrap(), b"keep source");
    let old = serde_json::json!({"volume_serial":1,"file_id":vec![2;16]});
    let native: platform::Identity = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(native.kind, platform::IdentityKind::Native128);
    assert_eq!(serde_json::to_value(&native).unwrap(), old);
    assert_eq!(
        serde_json::from_slice::<platform::Identity>(&serde_json::to_vec(&legacy).unwrap())
            .unwrap(),
        legacy
    );
}

#[test]
#[ignore = "requires VARTACOPY_TEST_FAT_ROOT pointing to an authorized FAT32 fixture directory"]
fn verified_fat_source_copy_tree_resume_and_move_preflight() {
    let selected = PathBuf::from(
        std::env::var_os("VARTACOPY_TEST_FAT_ROOT").expect("explicit FAT fixture root required"),
    );
    assert!(selected.is_absolute());
    let guard = platform::open_directory(&selected, false).unwrap();
    let volume = platform::volume_information(&guard).unwrap();
    assert!(matches!(volume.filesystem.as_str(), "FAT" | "FAT32"));
    let source_fixture = tempfile::Builder::new()
        .prefix("vartacopy-owned-fat-probe-")
        .tempdir_in(&selected)
        .unwrap();
    let source = source_fixture.path().join("дерево-😀");
    fs::create_dir(&source).unwrap();
    fs::create_dir(source.join("порожня")).unwrap();
    fs::write(source.join("нуль.txt"), []).unwrap();
    let payload: Vec<u8> = (0..(8 * 1024 * 1024 + 17))
        .map(|n| (n % 251) as u8)
        .collect();
    let source_file = source.join("дані-😀.bin");
    fs::write(&source_file, &payload).unwrap();
    let opened = platform::open_source(&source_file).unwrap();
    assert_eq!(
        platform::source_identity(&opened).unwrap().kind,
        platform::IdentityKind::FatSource
    );
    assert!(platform::identity(&opened).is_err()); // No weakening of staging/publication identity.
    assert_eq!(
        platform::streams(&source_file).unwrap(),
        vec![platform::Stream {
            name: Default::default(),
            size: payload.len() as u64
        }]
    );
    assert!(platform::streams(&source).unwrap().is_empty());
    drop(opened);
    let target_fixture = tempfile::tempdir().unwrap();
    let destination = target_fixture.path().join("destination");
    fs::create_dir(&destination).unwrap();
    let journal_path = target_fixture.path().join("jobs.sqlite");
    let mut journal = Journal::open(&journal_path).unwrap();
    let plan = planner::scan(
        &source,
        &destination,
        ScanOptions::default(),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(plan.is_ready(), "{plan:?}");
    assert!(
        journal
            .enqueue_with_operation(&plan, Conflict::Error, OperationKind::Move)
            .is_err()
    );
    assert!(journal.jobs(10, 0).unwrap().is_empty());
    assert_eq!(fs::read(&source_file).unwrap(), payload);
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    let id = journal.enqueue(&plan, Conflict::Error).unwrap();
    let control = Control::default();
    let mut stopped = false;
    let report = recovery::run(&mut journal, id, &control, &mut |event| {
        if event.phase == Phase::CheckpointCommitted && !stopped {
            stopped = true;
            control.cancel();
        }
        Ok(())
    })
    .unwrap();
    assert!(stopped);
    assert_ne!(report.status, "completed");
    drop(journal);
    let mut journal = Journal::open(&journal_path).unwrap();
    journal.schedule(id).unwrap();
    let report = recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "completed", "{report:?}");
    let copied = destination.join("дерево-😀");
    assert_eq!(fs::read(copied.join("дані-😀.bin")).unwrap(), payload);
    assert_eq!(fs::read(&source_file).unwrap(), payload);
    assert!(copied.join("порожня").is_dir());
    assert_eq!(fs::metadata(copied.join("нуль.txt")).unwrap().len(), 0);
    assert!(report.resumed_bytes > 0);
    drop(journal);
}
