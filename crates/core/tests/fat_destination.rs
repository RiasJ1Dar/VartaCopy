//! Real FAT/FAT32 destination acceptance. Creates only private temporary data
//! beneath an explicitly authorized fixture directory; never formats a device.
use std::{fs, path::PathBuf, sync::atomic::AtomicBool};
use vartacopy_core::{
    Conflict, Error,
    control::Control,
    journal::{Journal, OperationKind},
    planner::{self, ScanOptions},
    recovery::{self, Phase},
};
use vartacopy_platform_windows as platform;

#[test]
#[ignore = "requires VARTACOPY_TEST_FAT_ROOT pointing to an authorized FAT32 fixture directory"]
fn native_fat_destination_file_tree_and_recovery() {
    let selected = PathBuf::from(
        std::env::var_os("VARTACOPY_TEST_FAT_ROOT").expect("explicit FAT fixture root required"),
    );
    assert!(selected.is_absolute());
    let guard = platform::open_directory(&selected, false).unwrap();
    assert!(matches!(
        platform::volume_information(&guard)
            .unwrap()
            .filesystem
            .as_str(),
        "FAT" | "FAT32"
    ));
    let payload = vec![193u8; 4 * 1024 * 1024 + 17];
    for directory in [false, true] {
        for phase in [
            None,
            Some(Phase::CheckpointCommitted),
            Some(Phase::Published),
        ] {
            let source_fixture = tempfile::tempdir().unwrap();
            let target_fixture = tempfile::Builder::new()
                .prefix("vartacopy-owned-fat-destination-")
                .tempdir_in(&selected)
                .unwrap();
            let source = source_fixture.path().join(if directory {
                "дерево-😀"
            } else {
                "дані-😀.bin"
            });
            if directory {
                fs::create_dir(&source).unwrap();
                fs::create_dir(source.join("порожня")).unwrap();
                fs::create_dir(source.join("вкладена")).unwrap();
                fs::write(source.join("нуль.txt"), []).unwrap();
                fs::write(source.join("вкладена/дані.bin"), &payload).unwrap();
            } else {
                fs::write(&source, &payload).unwrap();
            }
            let journal_path = source_fixture.path().join("jobs.sqlite");
            let mut journal = Journal::open(&journal_path).unwrap();
            let plan = planner::scan(
                &source,
                target_fixture.path(),
                ScanOptions::default(),
                &AtomicBool::new(false),
            )
            .unwrap();
            assert!(plan.is_ready());
            assert!(
                journal
                    .enqueue_with_operation(&plan, Conflict::Error, OperationKind::Move)
                    .is_err()
            );
            assert!(journal.jobs(10, 0).unwrap().is_empty());
            let id = journal.enqueue(&plan, Conflict::Error).unwrap();
            let mut stopped = false;
            let result = recovery::run(&mut journal, id, &Control::default(), &mut |event| {
                if Some(event.phase) == phase && !stopped {
                    stopped = true;
                    return Err(Error::Cancelled);
                }
                Ok(())
            })
            .unwrap();
            if phase.is_some() {
                assert!(stopped && result.status != "completed", "{result:?}");
                drop(journal);
                journal = Journal::open(&journal_path).unwrap();
            } else {
                assert_eq!(result.status, "completed", "{result:?}");
            }
            let result =
                recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
            assert_eq!(
                result.status, "completed",
                "{directory}/{phase:?}: {result:?}"
            );
            assert_eq!(result.verified_bytes, payload.len() as u64);
            let output = target_fixture.path().join(source.file_name().unwrap());
            let (input, file) = if directory {
                assert!(output.join("порожня").is_dir());
                assert_eq!(fs::metadata(output.join("нуль.txt")).unwrap().len(), 0);
                (
                    source.join("вкладена/дані.bin"),
                    output.join("вкладена/дані.bin"),
                )
            } else {
                (source.clone(), output.clone())
            };
            assert_eq!(fs::read(input).unwrap(), payload);
            assert_eq!(fs::read(&file).unwrap(), payload);
            assert!(platform::identity(&platform::open_source(&file).unwrap()).is_err());
            assert_eq!(
                journal.counts(id).unwrap(),
                (if directory { 5 } else { 1 }, 0)
            );
            println!(
                "native FAT32 destination verified: directory={directory}, interruption={phase:?}, bytes={}",
                payload.len()
            );
            fs::write(&file, b"changed FAT destination").unwrap();
            let rejected =
                recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
            assert_eq!(rejected.status, "interrupted");
            assert_eq!(fs::read(&file).unwrap(), b"changed FAT destination");
            drop(journal);
        }
    }
}

#[test]
#[ignore = "requires VARTACOPY_TEST_FAT_ROOT pointing to an authorized FAT32 fixture directory"]
fn native_fat_to_fat_copy_and_ads_preflight() {
    let selected = PathBuf::from(
        std::env::var_os("VARTACOPY_TEST_FAT_ROOT").expect("explicit FAT fixture root required"),
    );
    assert!(selected.is_absolute());
    let selected_guard = platform::open_directory(&selected, false).unwrap();
    assert!(matches!(
        platform::volume_information(&selected_guard)
            .unwrap()
            .filesystem
            .as_str(),
        "FAT" | "FAT32"
    ));
    let source_fixture = tempfile::Builder::new()
        .prefix("vartacopy-owned-fat-source-")
        .tempdir_in(&selected)
        .unwrap();
    let destination = tempfile::Builder::new()
        .prefix("vartacopy-owned-fat-target-")
        .tempdir_in(&selected)
        .unwrap();
    let source = source_fixture.path().join("дерево-😀");
    fs::create_dir(&source).unwrap();
    fs::create_dir(source.join("порожня")).unwrap();
    fs::write(source.join("нуль.txt"), []).unwrap();
    let data = vec![43; 1024 * 1024 + 17];
    fs::write(source.join("дані-😀.bin"), &data).unwrap();
    let local = tempfile::tempdir().unwrap();
    let mut journal = Journal::open(&local.path().join("jobs.sqlite")).unwrap();
    let plan = planner::scan(
        &source,
        destination.path(),
        ScanOptions::default(),
        &AtomicBool::new(false),
    )
    .unwrap();
    let id = journal.enqueue(&plan, Conflict::Error).unwrap();
    let report = recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
    assert_eq!(report.status, "completed", "{report:?}");
    let target = destination.path().join(source.file_name().unwrap());
    assert_eq!(fs::read(target.join("дані-😀.bin")).unwrap(), data);
    assert_eq!(fs::read(source.join("дані-😀.bin")).unwrap(), data);
    assert!(target.join("порожня").is_dir());
    let ntfs = local.path().join("with-ads.bin");
    fs::write(&ntfs, b"main data").unwrap();
    let stream = PathBuf::from(format!("{}:note", ntfs.display()));
    fs::write(&stream, b"preserve additional data").unwrap();
    let rejected = planner::scan(
        &ntfs,
        destination.path(),
        ScanOptions::default(),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(rejected.to_string().contains("ADS"));
    assert_eq!(journal.jobs(10, 0).unwrap().len(), 1);
    assert!(!destination.path().join("with-ads.bin").exists());
    assert_eq!(fs::read(stream).unwrap(), b"preserve additional data");
}
