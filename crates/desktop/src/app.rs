use crate::worker::{Client, Command, State};
use eframe::egui::{self, Color32, RichText};
use std::{path::PathBuf, sync::mpsc, time::Duration};
use vartacopy_core::{Conflict, journal::Job, planner::ReparsePolicy, recovery::Phase};
const BLUE: Color32 = Color32::from_rgb(37, 99, 235);
const INK: Color32 = Color32::from_rgb(15, 23, 42);
const MUTED: Color32 = Color32::from_rgb(71, 85, 105);
const ERROR: Color32 = Color32::from_rgb(185, 28, 28);

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
    fn show(&mut self, ui: &mut egui::Ui, id: &str, label: egui::Id) {
        let response = ui
            .add_sized(
                [ui.available_width().max(120.0), 34.0],
                egui::TextEdit::singleline(&mut self.text)
                    .id_salt(id)
                    .hint_text("Повний шлях…")
                    .background_color(Color32::from_rgb(241, 245, 249))
                    .margin(egui::Margin::symmetric(10, 6))
                    .desired_width(ui.available_width()),
            )
            .labelled_by(label);
        if response.changed() {
            self.native = None;
        }
    }
}
#[derive(Clone, Copy)]
enum Picker {
    File,
    Folder,
    Destination,
}
pub struct App {
    client: Client,
    state: State,
    source: PathInput,
    destination: PathInput,
    conflict: usize,
    skip_links: bool,
    history: bool,
    selected: Option<i64>,
    banner: Option<String>,
    picker: Option<mpsc::Receiver<(Picker, Option<PathBuf>)>>,
    closing: bool,
    journal: PathBuf,
}
impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, path: PathBuf) -> std::io::Result<Self> {
        let ctx = &cc.egui_ctx;
        ctx.enable_accesskit();
        ctx.set_theme(egui::Theme::Light);
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
        let mut style = (*ctx.style_of(egui::Theme::Light)).clone();
        style.spacing.item_spacing = egui::vec2(10.0, 8.0);
        style.spacing.button_padding = egui::vec2(14.0, 8.0);
        style.spacing.interact_size.y = 34.0;
        for id in [egui::TextStyle::Body, egui::TextStyle::Button] {
            style
                .text_styles
                .insert(id, egui::FontId::proportional(16.0));
        }
        style.visuals.override_text_color = Some(INK);
        style.visuals.panel_fill = Color32::from_rgb(248, 250, 252);
        style.visuals.selection.bg_fill = BLUE;
        style.visuals.selection.stroke = egui::Stroke::new(1.0, Color32::WHITE);
        ctx.set_global_style(style);
        Ok(Self {
            client: Client::start(path.clone())?,
            state: State::default(),
            source: PathInput::default(),
            destination: PathInput::default(),
            conflict: 0,
            skip_links: false,
            history: false,
            selected: None,
            banner: None,
            picker: None,
            closing: false,
            journal: path,
        })
    }
    fn command(&mut self, command: Command) {
        if let Err(e) = self.client.send(command) {
            self.banner = Some(e);
        }
    }
    fn pick(&mut self, kind: Picker) {
        if self.picker.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let result = std::thread::Builder::new()
            .name("vartacopy-dialog".into())
            .spawn(move || {
                let dialog = rfd::FileDialog::new();
                let value = match kind {
                    Picker::File => dialog.set_title("Джерело: файл").pick_file(),
                    Picker::Folder => dialog.set_title("Джерело: каталог").pick_folder(),
                    Picker::Destination => dialog
                        .set_title("Наявний каталог призначення")
                        .pick_folder(),
                };
                if let Err(e) = tx.send((kind, value)) {
                    eprintln!("VartaCopy: file dialog result undelivered: {e}");
                }
            });
        match result {
            Ok(_) => self.picker = Some(rx),
            Err(e) => self.banner = Some(format!("Не вдалося відкрити діалог: {e}")),
        }
    }
    fn form(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style())
            .fill(Color32::WHITE)
            .inner_margin(16.0)
            .show(ui, |ui| {
                ui.label(RichText::new("Нова операція").size(19.0).strong());
                let source_label = ui.label("Джерело — файл або каталог");
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(self.picker.is_none(), |ui| {
                        if ui.button("Файл…").clicked() {
                            self.pick(Picker::File);
                        }
                        if ui.button("Каталог…").clicked() {
                            self.pick(Picker::Folder);
                        }
                    });
                    self.source.show(ui, "source", source_label.id);
                });
                let destination_label = ui
                    .label("Призначення — наявний каталог; усередині буде створено копію джерела");
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(self.picker.is_none(), egui::Button::new("Обрати…"))
                        .clicked()
                    {
                        self.pick(Picker::Destination);
                    }
                    self.destination
                        .show(ui, "destination", destination_label.id);
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("Якщо назва зайнята:");
                    egui::ComboBox::from_id_salt("conflict")
                        .selected_text(
                            ["Показати помилку", "Зберегти обидва", "Пропустити"][self.conflict],
                        )
                        .show_ui(ui, |ui| {
                            for (index, label) in
                                ["Показати помилку", "Зберегти обидва", "Пропустити"]
                                    .into_iter()
                                    .enumerate()
                            {
                                ui.selectable_value(&mut self.conflict, index, label);
                            }
                        });
                    ui.checkbox(
                        &mut self.skip_links,
                        "Пропускати посилання (частковий результат)",
                    );
                });
                ui.horizontal_wrapped(|ui| {
                    let enabled = self.state.ready
                        && !self.state.planning
                        && !self.closing
                        && !self.source.text.trim().is_empty()
                        && !self.destination.text.trim().is_empty();
                    if ui
                        .add_enabled(
                            enabled,
                            egui::Button::new(
                                RichText::new("Додати до черги").color(Color32::WHITE),
                            )
                            .fill(BLUE),
                        )
                        .clicked()
                    {
                        self.banner = None;
                        self.command(Command::Submit {
                            source: self.source.path(),
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
                        });
                    }
                    if self.state.planning {
                        ui.spinner();
                        ui.label("Сканування… Завдання ще не збережено.");
                    } else {
                        ui.label(
                            RichText::new("Кожна копія проходить перевірку BLAKE3.").color(MUTED),
                        );
                    }
                });
            });
    }
    fn active(&mut self, ui: &mut egui::Ui) {
        if let Some(active) = self.state.active.clone() {
            egui::Frame::group(ui.style())
                .fill(Color32::WHITE)
                .inner_margin(16.0)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(format!("Операція #{}", active.id))
                                .strong()
                                .size(19.0),
                        );
                        if ui
                            .add_enabled(active.request.is_empty(), egui::Button::new("Пауза"))
                            .clicked()
                        {
                            self.command(Command::Pause(active.id));
                        }
                        if ui
                            .add_enabled(active.request.is_empty(), egui::Button::new("Зупинити"))
                            .clicked()
                        {
                            self.command(Command::Cancel(active.id));
                        }
                    });
                    if !active.request.is_empty() {
                        ui.label(
                            "Зупинка після поточного кроку I/O… Дані для відновлення зберігаються.",
                        );
                    }
                    if let Some(progress) = active.progress {
                        ui.label(format!(
                            "{} · елемент {} із {}",
                            phase(progress.phase),
                            progress.entry + 1,
                            active.entry_count
                        ));
                        if progress.total > 0 {
                            let fraction = (progress.bytes as f64 / progress.total as f64)
                                .clamp(0.0, 1.0) as f32;
                            ui.add(
                                egui::ProgressBar::new(fraction)
                                    .desired_width(f32::INFINITY)
                                    .text(format!(
                                        "Поточний потік: {} / {}",
                                        bytes(progress.bytes),
                                        bytes(progress.total)
                                    )),
                            );
                        } else {
                            ui.spinner();
                        }
                    } else {
                        ui.spinner();
                        ui.label("Підготовка відновлення…");
                    }
                });
        }
    }
    fn job_row(&mut self, ui: &mut egui::Ui, job: &Job) {
        let active = self.state.active.as_ref().is_some_and(|a| a.id == job.id);
        egui::Frame::group(ui.style())
            .fill(Color32::WHITE)
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .selectable_label(
                            self.selected == Some(job.id),
                            RichText::new(format!(
                                "#{}  {}",
                                job.id,
                                job.source
                                    .file_name()
                                    .unwrap_or(job.source.as_os_str())
                                    .to_string_lossy()
                            ))
                            .strong(),
                        )
                        .clicked()
                    {
                        self.selected = Some(job.id);
                    }
                    ui.label(status(if active { "running" } else { &job.status }));
                    if job.status == "queued" && !active {
                        if ui.small_button("Вище").clicked() {
                            self.command(Command::Move {
                                id: job.id,
                                earlier: true,
                            });
                        }
                        if ui.small_button("Нижче").clicked() {
                            self.command(Command::Move {
                                id: job.id,
                                earlier: false,
                            });
                        }
                        if ui.small_button("Пауза").clicked() {
                            self.command(Command::Pause(job.id));
                        }
                    }
                    if matches!(job.status.as_str(), "paused" | "interrupted" | "cancelled")
                        && !active
                        && ui.small_button("Відновити").clicked()
                    {
                        self.command(Command::Resume(job.id));
                    }
                    if matches!(job.status.as_str(), "queued" | "paused" | "interrupted")
                        && !active
                        && ui.small_button("Зупинити").clicked()
                    {
                        self.command(Command::Cancel(job.id));
                    }
                });
                ui.add(
                    egui::Label::new(RichText::new(job.source.to_string_lossy()).color(MUTED))
                        .wrap(),
                );
                ui.add(
                    egui::Label::new(format!(
                        "До: {}",
                        job.output_root
                            .as_ref()
                            .unwrap_or(&job.destination)
                            .display()
                    ))
                    .wrap(),
                );
                if let Some(e) = &job.last_error {
                    ui.colored_label(ERROR, e);
                }
                if self.selected == Some(job.id) {
                    ui.label(format!(
                        "Елементів у плані: {}. Конфлікти: {:?}.",
                        job.entry_count, job.conflict
                    ));
                    if ui.button("Відкрити каталог призначення").clicked()
                    {
                        let path = job.destination.parent().unwrap_or(&job.destination);
                        match std::process::Command::new("explorer.exe").arg(path).spawn() {
                            Ok(_) => {}
                            Err(e) => {
                                self.banner = Some(format!("Не вдалося відкрити каталог: {e}"))
                            }
                        }
                    }
                }
            });
    }
}
impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
        if let Some(path) =
            ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()))
        {
            self.source.set(path);
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(root,|ui|{
            ui.horizontal(|ui|{
                ui.heading(RichText::new("VartaCopy").size(28.0).strong().color(BLUE));
                ui.label("Копіювання з перевіркою та відновленням");
            });
            ui.label(RichText::new("Файли й каталоги · локальний журнал · джерело зберігається").color(MUTED));
            ui.add_space(8.0);
            egui::ScrollArea::vertical().id_salt("main").show(ui,|ui|{
                ui.set_min_width(ui.available_width());
                ui.add_enabled_ui(!self.closing,|ui|self.form(ui));
                ui.add_space(8.0);
                self.active(ui);
                if self.closing {ui.label("Завершую поточний крок і зберігаю стан…");}
                if let Some(message)=&self.banner {ui.colored_label(ERROR,message);}
                if let Some(error)=self.state.errors.back(){ui.colored_label(ERROR,error);}
                if !self.state.ready&&!self.state.closed {ui.horizontal(|ui|{ui.spinner();ui.label("Відкриваю журнал…");});}
                ui.horizontal_wrapped(|ui|{
                    if ui.add_enabled(self.state.ready&&!self.closing,egui::Button::new(if self.state.automatic{"Зупинити запуск наступних"}else{"Запустити чергу"})).clicked(){self.command(Command::RunQueue(!self.state.automatic));}
                    ui.selectable_value(&mut self.history,false,"Черга");
                    ui.selectable_value(&mut self.history,true,"Історія та відновлення");
                });
                if self.history{
                    ui.horizontal(|ui|{
                        if ui.add_enabled(self.state.offset>0,egui::Button::new("Попередні")).clicked(){self.command(Command::Page(self.state.offset.saturating_sub(32)));}
                        ui.label(format!("Сторінка {}",self.state.offset/32+1));
                        if ui.add_enabled(self.state.history.len()==32,egui::Button::new("Наступні")).clicked(){self.command(Command::Page(self.state.offset+32));}
                    });
                }
                let jobs=if self.history{self.state.history.clone()}else{self.state.queue.clone()};
                if jobs.is_empty(){
                    ui.add_space(16.0);
                    ui.label(if self.history{"Збережених операцій на цій сторінці немає."}else{"Черга порожня. Оберіть джерело й призначення та додайте операцію."});
                }else{
                    if !self.history {ui.label("Перші 32 очікувані операції, у порядку виконання.");}
                    for job in &jobs{self.job_row(ui,job);}
                }
                if let Some(report)=self.state.last_report.clone(){
                    ui.separator();
                    ui.label(RichText::new(format!("Результат #{}: {}",report.job_id,status(&report.status))).strong());
                    ui.label(format!("Завершено: {} · пропущено: {} · залишилося: {} · помилки: {}",report.entries_committed,report.entries_skipped,report.entries_pending,report.entries_failed));
                    ui.label(format!("Перевірено потоків даних: {}",bytes(report.verified_bytes)));
                    if let Some(e)=&report.error{ui.colored_label(ERROR,e);}
                    if !report.issues.is_empty(){egui::CollapsingHeader::new("Помилки окремих елементів").show(ui,|ui|{for issue in &report.issues{ui.colored_label(ERROR,format!("{}: {}",issue.path.display,issue.message));}if report.issues_omitted>0{ui.label(format!("Ще {} повідомлень не показано",report.issues_omitted));}});}
                }
                ui.add_space(12.0);
                egui::CollapsingHeader::new("Журнал і правила копіювання").show(ui,|ui|{
                    ui.label(self.journal.to_string_lossy());
                    ui.label("Журнал зберігається на локальному фіксованому диску. Після збою відкрийте історію та натисніть «Відновити». Пауза зберігає прогрес; відновлення повторно перевіряє записані блоки.");
                    ui.label("Зайняті каталоги не об’єднуються. Пропущені елементи дають частковий результат. ACL та власник успадковуються від призначення; ADS каталогів і reparse points за замовчуванням блокують операцію.");
                });
            });
        });
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
        "partial" => "Частково — є пропущені елементи",
        _ => value,
    }
}
fn phase(value: Phase) -> &'static str {
    match value {
        Phase::StageRecorded => "Підготовка файлу",
        Phase::BlockWritten | Phase::BlockFlushed | Phase::CheckpointCommitted => "Копіювання",
        Phase::Checking | Phase::Verified => "Перевірка даних",
        Phase::PublishIntent | Phase::Published | Phase::Committed => "Публікація файлу",
        Phase::DirectoryIntent | Phase::DirectoryPublished | Phase::DirectoryReady => {
            "Створення каталогу"
        }
        Phase::DirectoryCommitted => "Збереження метаданих каталогу",
    }
}
