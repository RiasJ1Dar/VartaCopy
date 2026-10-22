use super::*;
use eframe::App as _;
use egui::accesskit::{Node, NodeId, Role};
use std::{
    fs,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
use vartacopy_core::{
    journal::Journal,
    planner::{self, ScanOptions},
};
fn fixture() -> (tempfile::TempDir, App) {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let dest = root.path().join("dest");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&dest).unwrap();
    for n in 0..140 {
        fs::write(source.join(format!("file-{n:03}")), b"data").unwrap();
    }
    let path = root.path().join("jobs.sqlite");
    let mut journal = Journal::open(&path).unwrap();
    let plan = planner::scan(
        &source,
        &dest,
        ScanOptions::default(),
        &AtomicBool::new(false),
    )
    .unwrap();
    let id = journal.enqueue(&plan, Conflict::Error).unwrap();
    drop(journal);
    let context = egui::Context::default();
    let mut app = App::new(&eframe::CreationContext::_new_kittest(context), path).unwrap();
    app.select(id);
    update(&mut app, |s| s.ready && s.files.is_some());
    (root, app)
}
#[test]
fn new_worker_error_opens_readable_messages_once_without_changing_selected_job() {
    let (_root, mut app) = fixture();
    let selected = app.selected;
    let mut state = app.state.clone();
    state.errors.push_back(
        "Копіювання не розпочато; запит не додано до черги. Причина: os error 87.".into(),
    );
    app.accept_snapshot(state.clone());
    assert!(app.messages_open);
    assert_eq!(app.selected, selected);
    let nodes = render(&mut app, [800.0, 600.0], None);
    assert!(nodes.iter().any(|(_, n)| n.label() == Some("Повідомлення")));
    assert!(nodes.iter().any(|(_, n)| {
        n.label()
            .is_some_and(|s| s.contains("Копіювання не розпочато"))
    }));
    app.messages_open = false;
    app.accept_snapshot(state.clone());
    assert!(!app.messages_open); // Dismissing the window is respected on subsequent snapshots.
    state.errors.push_back("Інша помилка".into());
    app.accept_snapshot(state);
    assert!(app.messages_open);
    app.begin_close();
    update(&mut app, |s| s.closed);
}

#[test]
fn fat_failure_shows_reason_and_explicit_windows_choice_with_collapsed_evidence() {
    let (_root, mut app) = fixture();
    let request = vartacopy_shell_windows::transfer::TransferRequest {
        id: [91; 16],
        operation: vartacopy_shell_windows::transfer::TransferOperation::Copy,
        origin: vartacopy_shell_windows::transfer::TransferOrigin::Explorer,
        sources: vec![PathBuf::from(r"D:\source.txt")],
        destination: PathBuf::from(r"E:\destination"),
    };
    let raw = "journal rejected: Копіювання не розпочато; запит не додано до черги. FAT32: FileIdInfo недоступний. Windows: The parameter is incorrect. (os error 87)";
    let mut state = app.state.clone();
    state.errors.push_back(raw.into());
    state.windows_retry = Some(request);
    app.accept_snapshot(state);
    render(&mut app, [900.0, 700.0], None); // Resolve the new window's scroll geometry.
    let nodes = render(&mut app, [900.0, 700.0], None);
    let labels = nodes
        .iter()
        .flat_map(|(_, n)| [n.label(), n.value()])
        .flatten()
        .collect::<Vec<_>>();
    assert!(labels.iter().any(|s| s.contains("FAT/exFAT")));
    assert!(labels.contains(&"Передати через Windows"));
    assert!(
        labels.join(" ").contains("Джерела не видалялися"),
        "{labels:?}"
    );
    assert!(!labels.iter().any(|s| s.contains("os error 87")));
    assert!(!app.state.dispatching);
    assert!(app.chooser.is_none()); // Rendering never retries the operation.
    app.begin_close();
    update(&mut app, |s| s.closed);
}
#[test]
fn tray_exit_uses_durable_shutdown_and_hide_requires_an_available_icon() {
    let (_root, mut app) = fixture();
    let before = app
        .state
        .history
        .iter()
        .map(|j| (j.id, j.status.clone()))
        .collect::<Vec<_>>();
    app.hide_to_tray(); // No native tray in headless mode: the window must remain reachable.
    assert!(!app.closing);
    app.tray_action(tray::Action::Show);
    assert!(!app.closing);
    app.tray_action(tray::Action::Exit);
    app.tray_action(tray::Action::Exit); // Repeated exit is idempotent.
    assert!(app.closing);
    update(&mut app, |s| s.closed);
    let saved = Journal::open(&app.journal).unwrap().jobs(50, 0).unwrap();
    assert_eq!(
        saved
            .iter()
            .map(|j| (j.id, j.status.clone()))
            .collect::<Vec<_>>(),
        before
    );
}
#[test]
fn selected_shell_request_waits_for_dispatch_and_never_opens_another_chooser() {
    use vartacopy_shell_windows::transfer::{
        TransferDelivery, TransferOperation, TransferOrigin, TransferRequest,
    };
    let (root, mut app) = fixture();
    let source = root.path().join("shell-choice.txt");
    fs::write(&source, b"explicit shell choice").unwrap();
    let source = source.canonicalize().unwrap();
    let request = TransferRequest {
        id: [0x89; 16],
        operation: TransferOperation::Copy,
        origin: TransferOrigin::Explorer,
        sources: vec![source.clone()],
        destination: root.path().join("dest"),
    };
    app.receive_transfer(TransferDelivery::SelectedVarta(request.clone()))
        .unwrap();
    assert!(app.chooser.is_none());
    app.state.dispatching = true;
    app.dispatch_pending_varta();
    assert!(app.pending_varta.is_some());
    app.state.dispatching = false;
    app.dispatch_pending_varta();
    assert!(app.pending_varta.is_none());
    update(&mut app, |s| {
        s.history
            .iter()
            .any(|j| j.source == source && j.status == "completed")
            && !s.dispatching
    });
    assert_eq!(
        fs::read(root.path().join("dest/shell-choice.txt")).unwrap(),
        b"explicit shell choice"
    );
    fs::remove_file(&source).unwrap();
    app.receive_transfer(TransferDelivery::SelectedVarta(request))
        .unwrap();
    app.dispatch_pending_varta();
    update(&mut app, |s| {
        !s.dispatching
            && s.notice
                .as_ref()
                .is_some_and(|n| n.contains("Повторного запуску"))
    });
    assert_eq!(
        app.state
            .history
            .iter()
            .filter(|j| j.source == source)
            .count(),
        1
    );
    assert!(app.chooser.is_none());
}
fn update(app: &mut App, condition: impl Fn(&State) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = app.client.snapshot().unwrap();
        if condition(&state) {
            app.state = state;
            return;
        }
        assert!(Instant::now() < deadline, "UI worker timeout");
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn render(app: &mut App, size: [f32; 2], key: Option<egui::Key>) -> Vec<(NodeId, Node)> {
    render_with_commands(app, size, key).0
}
fn render_with_commands(
    app: &mut App,
    size: [f32; 2],
    key: Option<egui::Key>,
) -> (Vec<(NodeId, Node)>, Vec<egui::ViewportCommand>) {
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(size[0], size[1]),
        )),
        events: key
            .map(|key| egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            })
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let ctx = app.context.clone();
    let mut frame = eframe::Frame::_new_kittest();
    let mut output = ctx.run_ui(input, |ui| {
        app.shortcuts(ui.ctx());
        app.ui(ui, &mut frame);
    });
    let nodes = output
        .platform_output
        .accesskit_update
        .take()
        .unwrap()
        .nodes;
    let commands = output
        .viewport_output
        .get(&egui::ViewportId::ROOT)
        .map(|v| v.commands.clone())
        .unwrap_or_default();
    output.drop_without_applying_deltas();
    (nodes, commands)
}

#[test]
fn chooser_cancel_has_focus_closes_with_escape_and_never_starts_a_job() {
    let (root, mut app) = fixture();
    app.offer_transfer(vartacopy_shell_windows::transfer::TransferRequest {
        id: [9; 16],
        operation: vartacopy_shell_windows::transfer::TransferOperation::Move,
        origin: vartacopy_shell_windows::transfer::TransferOrigin::AppForm,
        sources: vec![root.path().join("source")],
        destination: root.path().join("dest"),
    })
    .unwrap();
    assert!(!app.main_keys());
    for size in [[760.0, 300.0], [380.0, 240.0]] {
        let nodes = render(&mut app, size, None);
        let cancel = nodes
            .iter()
            .find(|(_, n)| n.role() == Role::Button && n.label() == Some("Скасувати"))
            .unwrap();
        let rect = cancel.1.bounds().unwrap();
        assert!(
            rect.x0 >= 0.0
                && rect.x1 <= size[0] as f64
                && rect.y0 >= 0.0
                && rect.y1 <= size[1] as f64,
            "{rect:?}"
        );
    }
    render(&mut app, [380.0, 240.0], Some(egui::Key::Escape));
    assert!(app.chooser.is_none());
    assert!(root.path().join("source").exists());
    assert!(!root.path().join("dest/source").exists());
    assert!(!app.client.snapshot().unwrap().dispatching);
    app.client.shutdown();
    update(&mut app, |s| s.closed);
    drop(app);
    let journal = Journal::open(&root.path().join("jobs.sqlite")).unwrap();
    assert_eq!(
        journal.jobs(32, 0).unwrap().len(),
        1,
        "only the preexisting fixture job"
    );
    assert!(journal.dispatch_receipt([9; 16]).unwrap().is_none());
}

#[test]
fn compact_transfer_controls_graph_paths_and_user_only_resize_are_accessible() {
    let (root, mut app) = fixture();
    let job = app.state.history[0].clone();
    let event = vartacopy_core::recovery::Progress {
        job_id: job.id,
        entry: 1,
        phase: Phase::BlockWritten,
        bytes: 1024,
        total: 8192,
        fresh_written_delta: 1024,
        stream_index: Some(0),
    };
    let mut meter = crate::telemetry::Meter::new(42);
    meter.observe(Duration::from_secs(1), &event);
    app.state.active = Some(crate::worker::Active {
        id: job.id,
        job: job.clone(),
        entry_count: job.entry_count,
        progress: Some(event),
        request: "",
        run_started: Instant::now() - Duration::from_secs(1),
        telemetry: meter.snapshot(),
        current_source: Some(root.path().join("source/file-000")),
    });
    app.set_compact(true);
    let (_, commands) = render_with_commands(&mut app, [760.0, 300.0], None);
    assert!(commands.iter().any(|command| matches!(command, egui::ViewportCommand::MinInnerSize(size) if size.x == 560.0 && size.y == 280.0)));
    for size in [[560.0, 280.0], [760.0, 300.0]] {
        let (nodes, commands) = render_with_commands(&mut app, size, None);
        assert!(!commands.iter().any(|c| matches!(
            c,
            egui::ViewportCommand::InnerSize(_) | egui::ViewportCommand::MinInnerSize(_)
        )));
        for label in ["Пауза", "Зупинити", "Деталі", "Черга", "Повний вигляд"]
        {
            let (_, node) = nodes
                .iter()
                .find(|(_, n)| n.label() == Some(label))
                .unwrap();
            let bounds = node.bounds().unwrap();
            assert!(
                bounds.x0 >= 0.0
                    && bounds.x1 <= f64::from(size[0])
                    && bounds.y0 >= 0.0
                    && bounds.y1 <= f64::from(size[1]),
                "{label}: {bounds:?}"
            );
        }
        assert!(nodes.iter().any(|(_, node)| {
            node.role() == Role::Image
                && node
                    .label()
                    .is_some_and(|s| s.contains("Історія швидкості запису"))
        }));
        assert!(nodes.iter().any(|(_, node)| {
            node.value()
                .is_some_and(|s| s.starts_with("Швидкість запису:"))
        }));
        assert!(!nodes.iter().any(|(_, n)| n.role() == Role::Grid));
    }
    render(&mut app, [760.0, 300.0], Some(egui::Key::F6));
    assert!(app.jobs_open);
    render(&mut app, [540.0, 640.0], Some(egui::Key::F6));
    assert!(app.transfer_details);
    app.transfer_details = false;
    let finished = app.state.active.take().unwrap();
    app.state.last_transfer = Some(finished.clone());
    let nodes = render(&mut app, [760.0, 300.0], None);
    assert!(
        nodes
            .iter()
            .any(|(_, n)| n.value() == Some("Швидкість запису: — МіБ/с"))
    );
    assert!(nodes.iter().any(|(_, n)| {
        n.value()
            .is_some_and(|value| value.starts_with("За запуск:"))
    }));
    let mut older = job.clone();
    older.id = 999;
    app.selected = Some(older.id);
    app.state.details = None;
    app.state.history = vec![older];
    app.state.active = Some(finished.clone());
    let nodes = render(&mut app, [760.0, 300.0], None);
    assert!(
        nodes
            .iter()
            .any(|(_, n)| n.value() == Some("Історія швидкості цього запуску недоступна"))
    );
    assert!(!nodes.iter().any(|(_, n)| {
        n.value()
            .is_some_and(|value| value.starts_with("За запуск:"))
    }));
    app.state.active = Some(finished);
    app.state.history = vec![job.clone()];
    app.selected = Some(job.id);
    app.context.all_styles_mut(|style| {
        for font in style.text_styles.values_mut() {
            font.size *= 1.5;
        }
    });
    let nodes = render(&mut app, [560.0, 440.0], None);
    for label in ["Пауза", "Зупинити", "Повний вигляд"] {
        let (_, node) = nodes
            .iter()
            .find(|(_, n)| n.label() == Some(label))
            .unwrap();
        assert!(
            node.bounds().unwrap().y1 <= 440.0,
            "large-font {label}: {:?}",
            node.bounds()
        );
    }
    app.set_compact(false);
    let (_, commands) = render_with_commands(&mut app, [980.0, 640.0], None);
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, egui::ViewportCommand::MinInnerSize(size) if size.x == 760.0))
    );
    app.client.shutdown();
    update(&mut app, |s| s.closed);
}

#[test]
fn move_form_has_named_choice_and_submits_persisted_move_without_changing_copy_drafts() {
    let (root, mut app) = fixture();
    assert_eq!(app.operation, OperationKind::Copy);
    app.source.set(root.path().join("source"));
    app.destination.set(root.path().join("dest"));
    app.open_form();
    app.operation = OperationKind::Move;
    let nodes = render(&mut app, [540.0, 640.0], None);
    let radios: Vec<_> = nodes
        .iter()
        .filter(|(_, n)| n.role() == Role::RadioButton)
        .map(|(_, n)| n.label())
        .collect();
    assert!(radios.contains(&Some("Копіювати")));
    assert!(radios.contains(&Some("Перемістити")));
    assert!(
        nodes
            .iter()
            .any(|(_, n)| n.role() == Role::Button && n.label() == Some("Перемістити"))
    );
    app.submit(false);
    update(&mut app, |s| !s.planning && s.queue.len() == 2);
    assert_eq!(app.state.queue[0].operation, OperationKind::Copy);
    assert_eq!(app.state.queue[1].operation, OperationKind::Move);
    assert!(root.path().join("source").exists());
    assert!(!root.path().join("dest/source").exists());
    app.add_sources(vec![root.path().join("source")]).unwrap();
    assert_eq!(app.operation, OperationKind::Copy);
    assert!(app.destination.text.is_empty());
    let mut job = app.state.queue[1].clone();
    job.status = "partial".into();
    assert!(job_status(&job).contains("не завершено"));
    assert!(!job_status(&job).contains("Переміщено з перевіркою"));
    job.status = "cancelled".into();
    assert!(job_status(&job).contains("залишаються"));
    job.status = "completed".into();
    assert_eq!(job_status(&job), "Переміщено з перевіркою");
    job.status = "partial".into();
    app.close_form();
    app.selected = Some(job.id);
    app.state.details = None;
    app.state.queue.clear();
    app.state.history = vec![job];
    let nodes = render(&mut app, [980.0, 640.0], None);
    assert!(
        nodes
            .iter()
            .any(|(_, n)| n.role() == Role::Button && n.label() == Some("Відновити переміщення"))
    );
    assert!(
        !nodes
            .iter()
            .any(|(_, n)| n.role() == Role::Button && n.label() == Some("Перевірити"))
    );
    app.client.shutdown();
    update(&mut app, |s| s.closed);
}

#[test]
fn move_summary_separates_destination_copies_from_durable_source_removals() {
    let (_root, mut app) = fixture();
    let mut details = app.state.details.as_ref().unwrap().as_ref().clone();
    details.job.operation = OperationKind::Move;
    details.job.status = "partial".into();
    details.job.entry_count = 5;
    details.entries_committed = 4;
    details.entries_skipped = 1;
    details.entries_pending = 3;
    let summary = job_counts(&details);
    assert!(summary.contains("у призначенні 4"));
    assert!(summary.contains("переміщено 1"));
    assert!(summary.contains("залишилося перемістити 3"));
    assert!(!summary.contains("Готово 4"));
    app.state.history = vec![details.job.clone()];
    app.state.details = Some(Arc::new(details.clone()));
    let nodes = render(&mut app, [980.0, 640.0], None);
    assert!(
        nodes
            .iter()
            .any(|(_, node)| node.value() == Some(summary.as_str()))
    );
    app.details_open = true;
    let nodes = render(&mut app, [980.0, 640.0], None);
    assert!(
        nodes
            .iter()
            .any(|(_, node)| node.value() == Some(summary.as_str()))
    );
    assert_eq!(entry_status(EntryState::Committed), "Копію збережено");
    assert_eq!(phase(Phase::Committed), "Копію файлу збережено");
    details.job.operation = OperationKind::Copy;
    assert_eq!(
        job_counts(&details),
        "Готово 4 · пропущено 1 · залишилося 3"
    );
    app.client.shutdown();
    update(&mut app, |s| s.closed);
}
#[test]
fn virtual_grid_has_real_rows_cells_and_one_keyboard_focus_target() {
    let (_root, mut app) = fixture();
    render(&mut app, [980.0, 640.0], None);
    let id = app.files_id();
    app.context.memory_mut(|m| m.request_focus(id));
    render(&mut app, [980.0, 640.0], None);
    render(&mut app, [980.0, 640.0], Some(egui::Key::Enter));
    assert!(
        !app.file_open,
        "no selection must not open a permanently loading dialog"
    );
    let nodes = render(&mut app, [980.0, 640.0], Some(egui::Key::ArrowDown));
    assert_eq!(app.file_selected, Some(0));
    let grid = nodes
        .iter()
        .find(|(node, _)| *node == id.accesskit_id())
        .unwrap();
    assert_eq!(grid.1.role(), Role::Grid);
    assert_eq!(grid.1.row_count(), Some(141));
    assert_eq!(grid.1.column_count(), Some(3));
    assert!(grid.1.active_descendant().is_some());
    let left = grid.1.bounds().unwrap().x0;
    for (_, n) in nodes.iter().filter(|(_, n)| {
        matches!(n.role(), Role::Cell | Role::ColumnHeader) && n.column_index() == Some(0)
    }) {
        assert!(
            (n.bounds().unwrap().x0 - left).abs() < 4.0,
            "first column must align to the left edge"
        );
    }
    assert_eq!(
        nodes
            .iter()
            .filter(|(_, n)| n.role() == Role::ColumnHeader)
            .count(),
        3
    );
    let rows = nodes.iter().filter(|(_, n)| n.role() == Role::Row).count();
    assert!((3..30).contains(&rows));
    assert!(
        nodes
            .iter()
            .filter(|(_, n)| matches!(n.role(), Role::Row | Role::Cell | Role::ListBoxOption))
            .all(|(_, n)| !n.supports_action(egui::accesskit::Action::Focus))
    );
    assert_eq!(
        nodes.iter().filter(|(_, n)| n.role() == Role::Cell).count(),
        rows * 3
    );
    render(&mut app, [980.0, 640.0], Some(egui::Key::ArrowDown));
    assert_eq!(app.file_selected, Some(1));
    render(&mut app, [980.0, 640.0], Some(egui::Key::End));
    assert_eq!(app.file_selected, Some(140));
    update(&mut app, |s| {
        s.files
            .as_ref()
            .is_some_and(|p| p.offset > 0 && p.offset + p.entries.len() == 141)
    });
    render(&mut app, [980.0, 640.0], None);
    assert!(app.file_visible.contains(&140));
    render(&mut app, [980.0, 640.0], Some(egui::Key::Enter));
    assert!(app.file_open);
    assert_eq!(app.file_details.as_ref().unwrap().1.ordinal, 140);
    render(&mut app, [980.0, 640.0], None);
    let nodes = render(&mut app, [980.0, 640.0], None);
    let focused = app.context.memory(|m| m.focused()).unwrap().accesskit_id();
    let focus = &nodes.iter().find(|(id, _)| *id == focused).unwrap().1;
    assert_eq!(focus.label(), Some("Копіювати повний шлях"));
    render(&mut app, [980.0, 640.0], Some(egui::Key::Home));
    assert_eq!(
        app.file_selected,
        Some(140),
        "dialog must isolate table keys"
    );
    render(&mut app, [980.0, 640.0], Some(egui::Key::Escape));
    assert!(!app.file_open);
    assert_eq!(app.context.memory(|m| m.focused()), Some(id));
    render(&mut app, [980.0, 640.0], Some(egui::Key::Home));
    assert_eq!(app.file_selected, Some(0));
}
#[test]
fn absent_targets_stop_focus_retries_and_wide_layout_has_one_job_list() {
    let (_root, mut app) = fixture();
    render(&mut app, [980.0, 640.0], None);
    app.jobs_open = true;
    let nodes = render(&mut app, [980.0, 640.0], None);
    assert!(!app.jobs_open);
    assert_eq!(
        nodes
            .iter()
            .filter(|(_, n)| n.role() == Role::ListBox)
            .count(),
        1
    );
    app.selected = None;
    app.pane = Pane::Jobs;
    render(&mut app, [980.0, 640.0], Some(egui::Key::F6));
    assert_eq!(app.pending_focus, None);
    assert_eq!(app.context.memory(|m| m.focused()), app.command_focus);
    app.request_focus(Some(egui::Id::new("nonexistent")));
    for _ in 0..5 {
        render(&mut app, [980.0, 640.0], None);
    }
    assert!(app.pending_focus.is_none());
    assert!(app.banner.is_some());
    app.request_focus(Some(egui::Id::new("nonexistent")));
    app.open_form();
    assert!(app.pending_focus.is_none());
}
#[test]
fn f6_cycles_panes_and_form_returns_focus_in_a_narrow_logical_viewport() {
    let (_root, mut app) = fixture();
    render(&mut app, [380.0, 240.0], None);
    let command = app.command_focus.unwrap();
    app.context.memory_mut(|m| m.request_focus(command));
    let nodes = render(&mut app, [380.0, 240.0], Some(egui::Key::F6));
    render(&mut app, [380.0, 240.0], None);
    render(&mut app, [380.0, 240.0], None);
    assert!(app.jobs_open);
    assert!(nodes.iter().any(|(_, n)| n.role() == Role::ListBox));
    assert_eq!(app.context.memory(|m| m.focused()), Some(app.jobs_id()));
    render(&mut app, [380.0, 240.0], Some(egui::Key::F6));
    assert!(!app.jobs_open);
    assert_eq!(app.context.memory(|m| m.focused()), Some(app.files_id()));
    render(&mut app, [380.0, 240.0], Some(egui::Key::F6));
    assert_eq!(app.context.memory(|m| m.focused()), Some(command));
    app.open_form();
    let nodes = render(&mut app, [380.0, 240.0], None);
    let focused = app.context.memory(|m| m.focused()).unwrap().accesskit_id();
    assert_eq!(
        nodes
            .iter()
            .find(|(id, _)| *id == focused)
            .unwrap()
            .1
            .role(),
        Role::TextInput
    );
    for label in ["Копіювати", "До черги", "Скасувати"] {
        let node = &nodes
            .iter()
            .find(|(_, n)| n.label() == Some(label))
            .unwrap()
            .1;
        let bounds = node.bounds().unwrap();
        assert!(
            bounds.x0 >= 0.0 && bounds.x1 <= 380.0,
            "{label}: {bounds:?}"
        );
        assert!(
            bounds.y0 >= 0.0 && bounds.y1 <= 240.0,
            "{label}: {bounds:?}"
        );
    }
    render(&mut app, [380.0, 240.0], Some(egui::Key::Escape));
    assert!(!app.new_operation);
    assert_eq!(app.context.memory(|m| m.focused()), Some(command));
}

fn receive_shell(app: &mut App, paths: Vec<PathBuf>) {
    let (tx, rx) = mpsc::channel();
    app.shell = Some(rx);
    tx.send(paths).unwrap();
    let ctx = app.context.clone();
    let mut frame = eframe::Frame::_new_kittest();
    let output = ctx.run_ui(egui::RawInput::default(), |ui| {
        app.logic(ui.ctx(), &mut frame);
    });
    output.drop_without_applying_deltas();
}

#[test]
fn shell_draft_after_completion_or_cancel_has_no_stale_destination_or_transfer() {
    let (root, mut app) = fixture();
    assert!(app.command(Command::RunQueue(true)));
    update(&mut app, |s| {
        s.active.is_none()
            && s.queue.is_empty()
            && s.history.iter().any(|j| j.status == "completed")
    });
    let completed = app
        .state
        .history
        .iter()
        .find(|j| j.status == "completed")
        .unwrap()
        .clone();
    let old_destination = root.path().join("dest");
    let incoming = root.path().join("new source");
    fs::create_dir(&incoming).unwrap();
    fs::write(incoming.join("file"), b"unchanged source").unwrap();
    app.source.set(root.path().join("source"));
    app.additional_sources.push(root.path().join("stale extra"));
    app.destination.set(old_destination.clone());
    let history_before = app.state.history.len();

    receive_shell(&mut app, vec![incoming.clone()]);
    assert!(app.new_operation);
    assert_eq!(app.source.path(), incoming);
    assert!(app.additional_sources.is_empty());
    assert!(app.destination.text.is_empty());
    assert!(app.destination.native.is_none());
    let nodes = render(&mut app, [980.0, 640.0], None);
    for label in ["Копіювати", "До черги"] {
        let node = &nodes
            .iter()
            .find(|(_, n)| n.label() == Some(label))
            .unwrap()
            .1;
        assert!(node.is_disabled(), "{label} needs a destination");
    }
    app.destination.set(old_destination.clone());
    render(&mut app, [980.0, 640.0], Some(egui::Key::Escape));
    assert!(!app.new_operation);
    receive_shell(&mut app, vec![root.path().join("source")]);
    assert!(app.destination.text.is_empty());
    assert!(app.destination.native.is_none());
    assert!(app.additional_sources.is_empty());
    assert_eq!(app.source.path(), root.path().join("source"));
    let state = app.client.snapshot().unwrap();
    assert!(state.active.is_none());
    assert!(!state.planning);
    assert!(state.queue.is_empty());
    assert_eq!(state.history.len(), history_before);
    assert!(
        state
            .history
            .iter()
            .any(|j| j.id == completed.id && j.status == "completed")
    );
    assert_eq!(
        fs::read(incoming.join("file")).unwrap(),
        b"unchanged source"
    );
    assert!(!old_destination.join("new source").exists());
    assert_eq!(
        fs::read(completed.output_root.as_ref().unwrap().join("file-000")).unwrap(),
        b"data"
    );
}

#[test]
fn shell_deliveries_preserve_open_manual_draft_and_accumulate_sources() {
    let (root, mut app) = fixture();
    let manual = root.path().join("manual");
    let destination = root.path().join("chosen destination");
    let first = root.path().join("first shell source");
    let second = root.path().join("second shell source");
    app.open_form();
    app.source.set(manual.clone());
    app.destination.set(destination.clone());
    app.conflict = 2;
    app.skip_links = true;
    receive_shell(&mut app, vec![first.clone()]);
    receive_shell(&mut app, vec![first.clone(), second.clone()]);
    assert_eq!(app.source.path(), manual);
    assert_eq!(app.additional_sources, vec![first.clone(), second.clone()]);
    assert_eq!(app.destination.path(), destination);
    assert_eq!(app.conflict, 2);
    assert!(app.skip_links);
    assert!(app.new_operation);

    // Editing the primary source to empty must not discard the other draft roots.
    app.source = PathInput::default();
    let third = root.path().join("third shell source");
    receive_shell(&mut app, vec![third.clone()]);
    assert_eq!(app.source.path(), first);
    assert_eq!(app.additional_sources, vec![second, third]);
    assert_eq!(app.destination.path(), destination);
    assert_eq!(app.state.history.len(), 1);
    assert!(app.state.active.is_none());
}

#[test]
fn rejected_shell_selection_does_not_mutate_closed_or_full_open_draft() {
    let (root, mut app) = fixture();
    let source = root.path().join("manual");
    let destination = root.path().join("chosen destination");
    app.source.set(source.clone());
    app.destination.set(destination.clone());
    assert!(app.add_sources(vec![PathBuf::from("relative")]).is_err());
    assert!(!app.new_operation);
    assert_eq!(app.source.path(), source);
    assert_eq!(app.destination.path(), destination);

    app.open_form();
    app.additional_sources = (1..512)
        .map(|n| root.path().join(format!("source-{n}")))
        .collect();
    let additional = app.additional_sources.clone();
    assert!(app.add_sources(vec![root.path().join("overflow")]).is_err());
    assert_eq!(app.source.path(), source);
    assert_eq!(app.additional_sources, additional);
    assert_eq!(app.destination.path(), destination);
    assert!(app.new_operation);
}
