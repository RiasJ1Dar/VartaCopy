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
    output.drop_without_applying_deltas();
    nodes
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
