mod chooser;
mod dialogs;
mod navigation;
mod system_style;
mod transfer_panel;
#[cfg(test)]
mod ui_tests;
mod view;
use crate::worker::{Client, Command, State};
use eframe::egui::{self, RichText};
use navigation::{Pane, composite, lock_arrows, navigate};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
};
use vartacopy_core::{
    Conflict,
    journal::{EntryState, EntrySummary, Job, JobDetails, OperationKind},
    planner::{EntryKind, ReparsePolicy},
    recovery::Phase,
};

#[derive(Default)]
struct PathInput {
    text: String,
    native: Option<PathBuf>,
}
impl PathInput {
    fn set(&mut self, path: PathBuf) {
        self.text = path.to_string_lossy().into_owned();
        self.native = Some(path);
    }
    fn path(&self) -> PathBuf {
        self.native
            .clone()
            .unwrap_or_else(|| PathBuf::from(&self.text))
    }
    fn show(&mut self, ui: &mut egui::Ui, id: &str, label: egui::Id, focus: bool) {
        let response = ui
            .add_sized(
                [ui.available_width().max(120.0), 32.0],
                egui::TextEdit::singleline(&mut self.text)
                    .id_salt(id)
                    .hint_text("Повний шлях…")
                    .desired_width(ui.available_width()),
            )
            .labelled_by(label);
        if response.changed() {
            self.native = None;
        }
        if focus {
            response.request_focus();
        }
    }
}
#[derive(Clone, Copy)]
enum Picker {
    File,
    Folder,
    Destination,
    Report(i64),
}
pub struct App {
    client: Client,
    state: State,
    source: PathInput,
    additional_sources: Vec<PathBuf>,
    destination: PathInput,
    operation: OperationKind,
    conflict: usize,
    skip_links: bool,
    history: bool,
    compact: bool,
    transfer_details: bool,
    expanded_size: Option<egui::Vec2>,
    mode_focus_pending: bool,
    selected: Option<i64>,
    last_active: Option<i64>,
    newest: Option<i64>,
    new_operation: bool,
    form_focus: bool,
    details_open: bool,
    messages_open: bool,
    settings_open: bool,
    file_selected: Option<usize>,
    file_details: Option<(Job, EntrySummary)>,
    file_visible: std::ops::Range<usize>,
    file_scroll: Option<usize>,
    pane: Pane,
    command_focus: Option<egui::Id>,
    return_focus: Option<egui::Id>,
    pending_focus: Option<egui::Id>,
    focus_attempts: u8,
    job_scroll: bool,
    jobs_open: bool,
    file_open: bool,
    file_focus: bool,
    files_pending: Option<(i64, usize)>,
    theme: egui::ThemePreference,
    base_styles: [egui::Style; 2],
    contrast: Option<system_style::Palette>,
    settings_watch: Option<system_style::Watch>,
    shell: Option<mpsc::Receiver<Vec<PathBuf>>>,
    transfers: Option<mpsc::Receiver<vartacopy_shell_windows::transfer::TransferRequest>>,
    chooser: Option<vartacopy_shell_windows::transfer::TransferRequest>,
    chooser_focus: bool,
    shell_action: Option<mpsc::Receiver<std::io::Result<String>>>,
    banner: Option<String>,
    picker: Option<mpsc::Receiver<(Picker, Option<PathBuf>)>>,
    closing: bool,
    journal: PathBuf,
    context: egui::Context,
}
impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, path: PathBuf) -> std::io::Result<Self> {
        let ctx = &cc.egui_ctx;
        ctx.enable_accesskit();
        ctx.set_theme(egui::ThemePreference::System);
        // Prefer the installed Windows UI font; bundled egui fonts remain a fallback.
        if let Some(windows) = std::env::var_os("WINDIR") {
            match std::fs::read(PathBuf::from(windows).join("Fonts/segoeui.ttf")) {
                Ok(data) => {
                    let mut fonts = egui::FontDefinitions::default();
                    fonts.font_data.insert(
                        "Windows UI".into(),
                        std::sync::Arc::new(egui::FontData::from_owned(data)),
                    );
                    fonts
                        .families
                        .entry(egui::FontFamily::Proportional)
                        .or_default()
                        .insert(0, "Windows UI".into());
                    ctx.set_fonts(fonts);
                }
                Err(error) => eprintln!(
                    "VartaCopy: cannot load Windows UI font; using bundled fonts: {error}"
                ),
            }
        }

        ctx.all_styles_mut(|style| {
            style.visuals.override_text_color = Some(if style.visuals.dark_mode {
                egui::Color32::from_rgb(232, 232, 232)
            } else {
                egui::Color32::from_rgb(28, 28, 28)
            });
            style.visuals.weak_text_color = Some(if style.visuals.dark_mode {
                egui::Color32::from_rgb(176, 176, 176)
            } else {
                egui::Color32::from_rgb(85, 85, 85)
            });
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
            style.spacing.button_padding = egui::vec2(10.0, 5.0);
            style.spacing.interact_size.y = 28.0;
            for id in [egui::TextStyle::Body, egui::TextStyle::Button] {
                style
                    .text_styles
                    .insert(id, egui::FontId::proportional(14.0));
            }
            style
                .text_styles
                .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
        });
        let base_styles = [
            (*ctx.style_of(egui::Theme::Light)).clone(),
            (*ctx.style_of(egui::Theme::Dark)).clone(),
        ];
        let settings_watch = if cc.winit_window().is_some() {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let handle = cc
                .window_handle()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                return Err(std::io::Error::other("expected a Windows GUI handle"));
            };
            Some(system_style::Watch::attach(
                handle.hwnd.get() as _,
                ctx.clone(),
            )?)
        } else {
            None
        };
        let contrast = system_style::Palette::read()?;
        system_style::apply(ctx, &base_styles, contrast);
        Ok(Self {
            client: Client::start_with_notify(path.clone(), {
                let ctx = ctx.clone();
                Arc::new(move || ctx.request_repaint())
            })?,
            state: State::default(),
            source: PathInput::default(),
            additional_sources: Vec::new(),
            destination: PathInput::default(),
            operation: OperationKind::Copy,
            conflict: 0,
            skip_links: false,
            history: false,
            compact: false,
            transfer_details: false,
            expanded_size: None,
            mode_focus_pending: false,
            selected: None,
            last_active: None,
            newest: None,
            new_operation: false,
            form_focus: false,
            details_open: false,
            messages_open: false,
            settings_open: false,
            file_selected: None,
            file_details: None,
            file_visible: 0..0,
            file_scroll: None,
            pane: Pane::Commands,
            command_focus: None,
            return_focus: None,
            pending_focus: None,
            focus_attempts: 0,
            job_scroll: false,
            jobs_open: false,
            file_open: false,
            file_focus: false,
            files_pending: None,
            theme: egui::ThemePreference::System,
            base_styles,
            contrast,
            settings_watch,
            shell: None,
            transfers: None,
            chooser: None,
            chooser_focus: false,
            shell_action: None,
            banner: None,
            picker: None,
            closing: false,
            journal: path,
            context: ctx.clone(),
        })
    }
    fn command(&mut self, command: Command) -> bool {
        match self.client.send(command) {
            Ok(()) => true,
            Err(e) => {
                self.banner = Some(e);
                false
            }
        }
    }
    pub fn attach_shell(&mut self, marker: &[u16], sources: Vec<PathBuf>) -> std::io::Result<()> {
        self.shell = Some(
            self.settings_watch
                .as_mut()
                .ok_or_else(|| std::io::Error::other("native window required"))?
                .shell(marker)?,
        );
        self.transfers = Some(
            self.settings_watch
                .as_mut()
                .ok_or_else(|| std::io::Error::other("native window required"))?
                .transfers()?,
        );
        if !sources.is_empty() {
            self.add_sources(sources)?;
        }
        Ok(())
    }
    fn shell_registration(&mut self, install: bool) {
        if self.shell_action.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = self.context.clone();
        let spawned = std::thread::Builder::new()
            .name("vartacopy-explorer".into())
            .spawn(move || {
                let result = std::env::current_exe().and_then(|gui| {
                    if install {
                        vartacopy_shell_windows::registration::install(&gui)?;
                    } else {
                        vartacopy_shell_windows::registration::remove(&gui)?;
                    }
                    Ok(if install {
                        "Меню Провідника додано. Windows 11: «Показати додаткові параметри»."
                    } else {
                        "Меню VartaCopy прибрано."
                    }
                    .to_string())
                });
                if let Err(error) = tx.send(result) {
                    eprintln!("VartaCopy: Explorer registration result undelivered: {error}");
                }
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => self.shell_action = Some(rx),
            Err(error) => self.banner = Some(error.to_string()),
        }
    }
    fn pick(&mut self, kind: Picker) {
        if self.picker.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let context = self.context.clone();
        let result = std::thread::Builder::new()
            .name("vartacopy-dialog".into())
            .spawn(move || {
                let dialog = rfd::FileDialog::new();
                let value = match kind {
                    Picker::File => dialog.set_title("Джерело: файл").pick_file(),
                    Picker::Folder => dialog.set_title("Джерело: каталог").pick_folder(),
                    Picker::Report(id) => dialog
                        .set_title("Зберегти звіт JSON")
                        .set_file_name(format!("VartaCopy-job-{id}.json"))
                        .add_filter("JSON", &["json"])
                        .save_file(),
                    Picker::Destination => dialog
                        .set_title("Наявний каталог призначення")
                        .pick_folder(),
                };
                if let Err(e) = tx.send((kind, value)) {
                    eprintln!("VartaCopy: file dialog result undelivered: {e}");
                }
                context.request_repaint();
            });
        match result {
            Ok(_) => self.picker = Some(rx),
            Err(e) => self.banner = Some(format!("Не вдалося відкрити діалог: {e}")),
        }
    }

    fn select(&mut self, id: i64) {
        self.selected = Some(id);
        self.file_selected = None;
        self.file_details = None;
        self.file_scroll = Some(0);
        self.file_visible = 0..0;
        self.file_open = false;
        self.command(Command::Inspect { id, offset: 0 });
        self.load_files(id, 0);
    }
    fn load_files(&mut self, id: i64, offset: usize) {
        if self.command(Command::Files { id, offset }) {
            self.files_pending = Some((id, offset));
        }
    }
    fn selected_job(&self) -> Option<Job> {
        let id = self.selected?;
        self.state
            .details
            .as_ref()
            .filter(|d| d.job.id == id)
            .map(|d| d.job.clone())
            .or_else(|| {
                self.state
                    .history
                    .iter()
                    .chain(&self.state.queue)
                    .find(|j| j.id == id)
                    .cloned()
            })
    }
    fn submit(&mut self, start: bool) {
        let mut sources = vec![self.source.path()];
        sources.extend(self.additional_sources.iter().cloned());
        let accepted = self.command(Command::SubmitMany {
            sources,
            destination: self.destination.path(),
            conflict: match self.conflict {
                1 => Conflict::KeepBoth,
                2 => Conflict::Skip,
                _ => Conflict::Error,
            },
            reparse: if self.skip_links {
                ReparsePolicy::Skip
            } else {
                ReparsePolicy::Reject
            },
            operation: self.operation,
            start,
        });
        if accepted {
            self.return_focus = self.command_focus;
            self.close_form();
            self.banner = None;
            self.history = false;
        }
    }
}
impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.chooser.is_none()
            && let Some(inbox) = &self.transfers
        {
            match inbox.try_recv() {
                Ok(request) => {
                    if let Err(error) = self.offer_transfer(request) {
                        self.banner = Some(error.to_string());
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.transfers = None;
                    self.banner = Some("Канал запитів передачі закрито".into());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(inbox) = &self.shell {
            match inbox.try_recv() {
                Ok(paths) => {
                    if !paths.is_empty()
                        && let Err(error) = self.add_sources(paths)
                    {
                        self.banner = Some(error.to_string());
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    ctx.request_repaint(); // Drain the bounded inbox, then return to event-driven idle.
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.shell = None;
                    self.banner = Some("Канал Провідника закрито".into());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(action) = &self.shell_action {
            match action.try_recv() {
                Ok(result) => {
                    self.banner =
                        Some(result.unwrap_or_else(|e| format!("Інтеграція Провідника: {e}")));
                    self.shell_action = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.banner = Some("Встановлення меню завершилось без відповіді".into());
                    self.shell_action = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self
            .settings_watch
            .as_ref()
            .is_some_and(|watch| watch.take_changed())
        {
            match system_style::Palette::read() {
                Ok(palette) => {
                    self.contrast = palette;
                    system_style::apply(ctx, &self.base_styles, palette);
                }
                Err(error) => {
                    self.banner = Some(format!(
                        "Не вдалося прочитати системні параметри доступності: {error}"
                    ))
                }
            }
        }
        match self.client.snapshot() {
            Ok(state) => self.state = state,
            Err(e) => self.banner = Some(e.to_string()),
        }
        if let Some(receiver) = self.picker.as_ref() {
            match receiver.try_recv() {
                Ok((kind, Some(path))) => {
                    match kind {
                        Picker::File | Picker::Folder => self.source.set(path),
                        Picker::Destination => self.destination.set(path),
                        Picker::Report(id) => {
                            self.command(Command::Export { id, path });
                        }
                    }
                    self.picker = None;
                }
                Ok((_, None)) => self.picker = None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.banner = Some("Діалог завершився без відповіді".into());
                    self.picker = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some((id, offset)) = self.files_pending
            && self
                .state
                .files
                .as_ref()
                .is_some_and(|p| p.job_id == id && p.offset == offset)
        {
            self.files_pending = None;
        }
        if self.file_open && self.file_details.is_none() {
            self.inspect_file();
        }
        let newest = self.state.history.first().map(|j| j.id);
        if newest != self.newest {
            let initial = self.newest.is_none();
            self.newest = newest;
            if let Some(id) = newest {
                self.select(id);
                if initial && self.state.queue.is_empty() {
                    self.history = true;
                }
            }
        }
        let active = self.state.active.as_ref().map(|a| a.id);
        if active != self.last_active {
            self.last_active = active;
            if let Some(id) = active {
                self.select(id);
            }
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::N))
            && self.state.ready
            && self.chooser.is_none()
            && !self.closing
        {
            self.open_form();
        }
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect::<Vec<_>>()
        });
        if !dropped.is_empty() {
            let result = if !self.destination.text.trim().is_empty() {
                self.offer_paths(
                    dropped,
                    vartacopy_shell_windows::transfer::TransferOrigin::AppDrop,
                    None,
                )
            } else {
                self.add_sources(dropped)
            };
            if let Err(error) = result {
                self.banner = Some(error.to_string());
            }
        }
        self.shortcuts(ctx);
        if ctx.input(|i| i.viewport().close_requested()) {
            self.closing = true;
            self.client.shutdown();
            if !self.state.closed {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
        }
        if self.closing && self.state.closed {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.compact && root.available_width() >= 680.0 {
            self.jobs_open = false;
        }
        let mut toolbar = egui::Panel::top("toolbar");
        if self.compact {
            toolbar = toolbar
                .exact_size(transfer_panel::compact_bar_height(
                    root,
                    &["Повний вигляд", "Деталі", "Черга", "Нова…"],
                ))
                .frame(
                    egui::Frame::side_top_panel(root.style())
                        .inner_margin(egui::Margin::symmetric(8, 4)),
                );
        }
        toolbar.show(root, |ui| self.toolbar(ui));
        if !self.compact
            || self.closing
            || self.state.planning
            || self.state.dispatching
            || !self.state.ready
            || self.banner.is_some()
            || !self.state.errors.is_empty()
            || self.state.notice.is_some()
        {
            egui::Panel::bottom("status").show(root, |ui| self.footer(ui));
        }
        if self.compact {
            egui::Panel::bottom("transfer_controls")
                .exact_size(self.compact_controls_height(root))
                .frame(
                    egui::Frame::side_top_panel(root.style())
                        .inner_margin(egui::Margin::symmetric(8, 4)),
                )
                .show(root, |ui| {
                    if let Some(job) = self.panel_job() {
                        let active = self.state.active.clone().filter(|a| a.id == job.id);
                        self.operation_controls(ui, &job, active.as_ref());
                    }
                });
        }
        if !self.compact && root.available_width() >= 680.0 {
            egui::Panel::left("jobs")
                .default_size(190.0)
                .size_range(180.0..=320.0)
                .show(root, |ui| self.sidebar(ui));
        }
        egui::CentralPanel::default().show(root, |ui| {
            if let Some(job) = self.panel_job() {
                if self.compact {
                    egui::ScrollArea::vertical()
                        .id_salt("transfer_summary")
                        .show(ui, |ui| {
                            self.operation(ui, &job);
                            if self.transfer_details {
                                self.table(ui, &job);
                            }
                        });
                } else {
                    self.operation(ui, &job);
                    self.table(ui, &job);
                }
            } else {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.heading("Додайте файли для копіювання");
                    ui.label("Файл або дерево каталогів із перевіркою та відновленням.");
                    if ui
                        .add_enabled(self.state.ready, egui::Button::new("Нова операція…"))
                        .clicked()
                    {
                        self.open_form();
                    }
                });
            }
        });
        self.dialogs();
        if let Some(id) = self.pending_focus {
            if self
                .context
                .read_response(id)
                .is_some_and(|r| r.enabled() && r.sense.is_focusable())
            {
                self.context.memory_mut(|m| m.request_focus(id));
                self.pending_focus = None;
            } else {
                self.focus_attempts += 1;
                if self.focus_attempts < 4 {
                    self.context.request_repaint();
                } else {
                    self.pending_focus = None;
                    self.banner = Some("Не вдалося перейти до області. Повторіть F6.".into());
                }
            }
        }
    }
}
fn name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}
// Explorer uses shell paths. Strip only the verbatim DOS/UNC prefix, preserving native names.
fn explorer_path(path: &Path) -> PathBuf {
    use std::{
        ffi::OsString,
        path::{Component, Prefix},
    };
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return path.to_path_buf();
    };
    let mut output = match prefix.kind() {
        Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:\\", char::from(drive))),
        Prefix::VerbatimUNC(server, share) => {
            let mut value = OsString::from("\\\\");
            value.push(server);
            value.push("\\");
            value.push(share);
            value.push("\\");
            PathBuf::from(value)
        }
        _ => return path.to_path_buf(),
    };
    for component in components {
        if component != Component::RootDir {
            output.push(component.as_os_str());
        }
    }
    output
}

fn path_line(ui: &mut egui::Ui, label: &str, path: &Path) {
    ui.scope(|ui| {
        ui.spacing_mut().interact_size.y = 18.0;
        ui.horizontal(|ui| {
            ui.label(RichText::new(label).weak());
            let response = ui.add(egui::Label::new(path.display().to_string()).truncate());
            response.context_menu(|ui| {
                if ui.button("Копіювати шлях").clicked() {
                    ui.ctx().copy_text(path.to_string_lossy().into_owned());
                    ui.close();
                }
            });
            response.on_hover_text(path.display().to_string());
        });
    });
}
fn column_widths(width: f32) -> [f32; 3] {
    let status = if width < 430.0 { 100.0 } else { 124.0 };
    let size = if width < 430.0 { 64.0 } else { 78.0 };
    [(width - status - size - 16.0).max(60.0), status, size]
}
fn table_label(
    ui: &mut egui::Ui,
    width: f32,
    height: f32,
    text: impl Into<egui::WidgetText>,
    right: bool,
) -> egui::Response {
    let size = egui::vec2(width, height);
    let layout = if right {
        egui::Layout::right_to_left(egui::Align::Center)
    } else {
        egui::Layout::left_to_right(egui::Align::Center)
    };
    ui.allocate_ui_with_layout(size, layout, |ui| {
        ui.set_min_size(size);
        ui.add(egui::Label::new(text).truncate())
    })
    .inner
}
fn entry_status(state: EntryState) -> &'static str {
    match state {
        EntryState::Planned => "Очікує",
        EntryState::Copying => "Копіювання",
        EntryState::Verified => "Перевірено",
        EntryState::Publishing => "Публікація",
        EntryState::Committed => "Копію збережено",
        EntryState::Skipped => "Пропущено",
        EntryState::DirIntent => "Створення",
        EntryState::DirectoryReady => "Каталог створено",
    }
}
fn operation_action(operation: OperationKind) -> &'static str {
    match operation {
        OperationKind::Copy => "Копіювати",
        OperationKind::Move => "Перемістити",
    }
}
fn operation_name(operation: OperationKind) -> &'static str {
    match operation {
        OperationKind::Copy => "Копіювання",
        OperationKind::Move => "Переміщення",
    }
}
fn job_status(job: &Job) -> &str {
    match (job.operation, job.status.as_str()) {
        (OperationKind::Move, "completed") => "Переміщено з перевіркою",
        (OperationKind::Move, "partial") => {
            "Частково — переміщення не завершено; перевірте збережені джерела"
        }
        (OperationKind::Move, "cancelled") => {
            "Зупинено — збережені результати й решта джерел залишаються"
        }
        (OperationKind::Move, "paused") => "На паузі — переміщення не завершено",
        _ => status(&job.status),
    }
}
fn job_counts(details: &JobDetails) -> String {
    if details.job.operation == OperationKind::Move {
        let moved = details
            .job
            .entry_count
            .saturating_sub(details.entries_skipped)
            .saturating_sub(details.entries_pending);
        format!(
            "Скопійовано/перевірено у призначенні {} · переміщено {} · пропущено {} · залишилося перемістити {}",
            details.entries_committed, moved, details.entries_skipped, details.entries_pending
        )
    } else {
        format!(
            "Готово {} · пропущено {} · залишилося {}",
            details.entries_committed, details.entries_skipped, details.entries_pending
        )
    }
}
fn short_status(value: &str) -> &str {
    match value {
        "completed" => "Завершено",
        "partial" => "Частково",
        "interrupted" => "Перервано",
        _ => status(value),
    }
}
pub fn bytes(value: u64) -> String {
    let units = ["Б", "КіБ", "МіБ", "ГіБ", "ТіБ"];
    let mut v = value as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < units.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{value} {}", units[i])
    } else {
        format!("{v:.1} {}", units[i])
    }
}
fn status(value: &str) -> &str {
    match value {
        "queued" => "Очікує",
        "running" => "Виконується",
        "paused" => "На паузі",
        "interrupted" => "Перервано — потрібне відновлення",
        "cancelled" => "Зупинено",
        "completed" => "Завершено з перевіркою",
        "partial" => "Частково — є пропущені елементи або збережені джерела",
        _ => value,
    }
}
fn phase(value: Phase) -> &'static str {
    match value {
        Phase::StageRecorded => "Підготовка файлу",
        Phase::BlockWritten | Phase::BlockFlushed | Phase::CheckpointCommitted => "Копіювання",
        Phase::Checking | Phase::Verified => "Перевірка даних",
        Phase::PublishIntent => "Публікація файла",
        Phase::Published => "Файл опубліковано",
        Phase::Committed => "Копію файлу збережено",
        Phase::DirectoryIntent | Phase::DirectoryPublished | Phase::DirectoryReady => {
            "Створення каталогу"
        }
        Phase::DirectoryCommitted => "Збереження метаданих каталогу",
        Phase::CleanupIntent => "Підготовка видалення джерела",
        Phase::SourceDeletePrepared => "Перевірка джерела перед видаленням",
        Phase::SourceRemoved => "Джерело видалено",
        Phase::CleanupCommitted => "Видалення джерела збережено в журналі",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        ffi::OsString,
        os::windows::ffi::{OsStrExt, OsStringExt},
    };
    #[test]
    fn explorer_dos_and_unc_paths_preserve_unpaired_utf16() {
        let mut native: Vec<u16> = r"\\?\D:\copy\".encode_utf16().collect();
        native.extend([0xD800, b'x' as u16]);
        let native = PathBuf::from(OsString::from_wide(&native));
        let expected: Vec<u16> = r"D:\copy\"
            .encode_utf16()
            .chain([0xD800, b'x' as u16])
            .collect();
        assert_eq!(
            explorer_path(&native)
                .as_os_str()
                .encode_wide()
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            explorer_path(Path::new(r"\\?\UNC\server\share\copy")),
            PathBuf::from(r"\\server\share\copy")
        );
        assert_eq!(
            explorer_path(Path::new(r"D:\copy")),
            PathBuf::from(r"D:\copy")
        );
    }
}
