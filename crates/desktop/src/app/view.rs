use super::*;
impl App {
    fn job_row(&mut self, ui: &mut egui::Ui, job: &Job) {
        let running = self.state.active.as_ref().is_some_and(|a| a.id == job.id);
        let selected = self.selected == Some(job.id);
        let caption = format!(
            "#{} · {}",
            job.id,
            short_status(if running { "running" } else { &job.status })
        );
        let frame = egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(8, 6))
            .fill(if selected {
                ui.visuals().selection.bg_fill
            } else {
                egui::Color32::TRANSPARENT
            })
            .corner_radius(3)
            .show(ui, |ui| {
                ui.set_width((ui.available_width() - 16.0).max(100.0));
                if selected {
                    ui.visuals_mut().override_text_color =
                        Some(ui.visuals().selection.stroke.color);
                }
                ui.add(egui::Label::new(name(&job.source)).truncate());
                ui.label(RichText::new(&caption).small());
            });
        let response = frame.response.interact(egui::Sense::click());
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                ui.is_enabled(),
                format!("{} {caption}", name(&job.source)),
            )
        });
        if response.has_focus() {
            ui.painter().rect_stroke(
                response.rect,
                3,
                ui.visuals().selection.stroke,
                egui::StrokeKind::Inside,
            );
        }
        if response.clicked() {
            self.select(job.id);
        }
        response.on_hover_text(job.source.display().to_string());
    }
    pub(super) fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.history, false, "Черга");
            ui.selectable_value(&mut self.history, true, "Історія");
        });
        ui.separator();
        let jobs = if self.history {
            self.state.history.clone()
        } else {
            self.state.queue.clone()
        };
        let active = if !self.history {
            self.state.active.as_ref().map(|a| a.job.clone())
        } else {
            None
        };
        let bottom = if self.history { 36.0 } else { 0.0 };
        egui::ScrollArea::vertical()
            .id_salt("queue")
            .max_height((ui.available_height() - bottom).max(80.0))
            .show(ui, |ui| {
                if let Some(job) = active.as_ref() {
                    self.job_row(ui, job);
                    ui.separator();
                }
                for job in &jobs {
                    self.job_row(ui, job);
                }
                if jobs.is_empty() && active.is_none() {
                    ui.label(
                        RichText::new(if self.history {
                            "Історія порожня"
                        } else {
                            "Черга порожня"
                        })
                        .weak(),
                    );
                }
            });
        if self.history {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(self.state.offset > 0, egui::Button::new("Назад"))
                    .clicked()
                {
                    self.command(Command::Page(self.state.offset.saturating_sub(32)));
                }
                if ui
                    .add_enabled(jobs.len() == 32, egui::Button::new("Далі"))
                    .clicked()
                {
                    self.command(Command::Page(self.state.offset + 32));
                }
            });
        }
    }
    pub(super) fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    self.state.ready && !self.closing,
                    egui::Button::new("Нова операція…"),
                )
                .on_hover_text("Ctrl+N")
                .clicked()
            {
                self.new_operation = true;
                self.form_focus = true;
            }
            let can_run = self.state.ready
                && !self.closing
                && (!self.state.queue.is_empty() || self.state.automatic);
            if ui
                .add_enabled(
                    can_run,
                    egui::Button::new(if self.state.automatic {
                        "Черга: запущена"
                    } else {
                        "Запустити чергу"
                    }),
                )
                .on_hover_text("Керує запуском наступних завдань; поточне копіювання триває")
                .clicked()
            {
                self.command(Command::RunQueue(!self.state.automatic));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button("Меню", |ui| {
                    if ui.button("Вигляд і правила…").clicked() {
                        self.settings_open = true;
                        ui.close();
                    }
                    if ui.button("Повідомлення…").clicked() {
                        self.messages_open = true;
                        ui.close();
                    }
                });
            });
        });
    }
    pub(super) fn operation(&mut self, ui: &mut egui::Ui, job: &Job) {
        let active = self.state.active.clone().filter(|a| a.id == job.id);
        ui.add(
            egui::Label::new(
                RichText::new(format!("{} · #{}", name(&job.source), job.id)).strong(),
            )
            .truncate(),
        );
        path_line(ui, "З:", &job.source);
        path_line(
            ui,
            "До:",
            job.output_root.as_ref().unwrap_or(&job.destination),
        );
        if let Some(a) = active.as_ref() {
            if !a.request.is_empty() {
                ui.label("Зупиняю поточний крок I/O, зберігаю прогрес…");
            } else if let Some(p) = a.progress.as_ref() {
                ui.label(format!(
                    "{} · елемент {} із {}",
                    phase(p.phase),
                    p.entry + 1,
                    a.entry_count
                ));
            } else {
                ui.label("Підготовка відновлення…");
            }
            let (fraction, text) = a
                .progress
                .as_ref()
                .filter(|p| p.total > 0)
                .map(|p| {
                    (
                        (p.bytes as f64 / p.total as f64).clamp(0.0, 1.0) as f32,
                        format!("Поточний потік: {} / {}", bytes(p.bytes), bytes(p.total)),
                    )
                })
                .unwrap_or((0.0, "Обробка елементів…".into()));
            ui.add(
                egui::ProgressBar::new(fraction)
                    .desired_width(ui.available_width())
                    .text(text),
            );
        } else {
            ui.label(status(&job.status));
            if let Some(d) = self.state.details.as_ref().filter(|d| d.job.id == job.id) {
                ui.label(
                    RichText::new(format!(
                        "Готово {} · пропущено {} · залишилося {}",
                        d.entries_committed, d.entries_skipped, d.entries_pending
                    ))
                    .weak(),
                );
            }
        }
        if let Some(e) = &job.last_error
            && ui
                .add(
                    egui::Button::new(RichText::new(e).color(ui.visuals().error_fg_color))
                        .frame(false)
                        .wrap_mode(egui::TextWrapMode::Truncate),
                )
                .clicked()
        {
            self.details_open = true;
        }
        ui.horizontal(|ui| {
            if let Some(a) = active.as_ref() {
                ui.add_enabled_ui(a.request.is_empty(), |ui| {
                    if ui.button("Пауза").clicked() {
                        self.command(Command::Pause(job.id));
                    }
                    if ui.button("Зупинити").clicked() {
                        self.command(Command::Cancel(job.id));
                    }
                });
            } else {
                if matches!(
                    job.status.as_str(),
                    "queued" | "paused" | "interrupted" | "cancelled"
                ) && ui
                    .button(if job.status == "queued" {
                        "Запустити"
                    } else {
                        "Відновити"
                    })
                    .clicked()
                {
                    self.command(Command::Resume(job.id));
                }
                if matches!(job.status.as_str(), "completed" | "partial")
                    && ui.button("Перевірити").clicked()
                {
                    self.command(Command::Recheck(job.id));
                }
            }
            ui.menu_button("Ще…", |ui| {
                if ui.button("Відкрити призначення").clicked() {
                    let folder = job.destination.parent().unwrap_or(&job.destination);
                    if let Err(error) = std::process::Command::new("explorer.exe")
                        .arg(explorer_path(folder))
                        .spawn()
                    {
                        self.banner = Some(format!("Не вдалося відкрити каталог: {error}"));
                    }
                    ui.close();
                }
                if ui.button("Подробиці операції…").clicked() {
                    self.details_open = true;
                    ui.close();
                }
                if ui
                    .add_enabled(
                        active.is_none() && self.picker.is_none(),
                        egui::Button::new("Експорт JSON…"),
                    )
                    .clicked()
                {
                    self.pick(Picker::Report(job.id));
                    ui.close();
                }
                if active.is_none() && job.status == "queued" {
                    if ui.button("Вище в черзі").clicked() {
                        self.command(Command::Move {
                            id: job.id,
                            earlier: true,
                        });
                        ui.close();
                    }
                    if ui.button("Нижче в черзі").clicked() {
                        self.command(Command::Move {
                            id: job.id,
                            earlier: false,
                        });
                        ui.close();
                    }
                    if ui.button("Відкласти").clicked() {
                        self.command(Command::Pause(job.id));
                        ui.close();
                    }
                }
                if active.is_none()
                    && matches!(job.status.as_str(), "queued" | "paused" | "interrupted")
                    && ui.button("Зупинити завдання").clicked()
                {
                    self.command(Command::Cancel(job.id));
                    ui.close();
                }
                if ui.button("Оновити список").clicked() {
                    self.command(Command::Inspect {
                        id: job.id,
                        offset: 0,
                    });
                    let offset = self
                        .state
                        .files
                        .as_ref()
                        .filter(|p| p.job_id == job.id)
                        .map_or(0, |p| p.offset);
                    self.load_files(job.id, offset);
                    ui.close();
                }
            });
        });
    }
    pub(super) fn table(&mut self, ui: &mut egui::Ui, job: &Job) {
        let Some(page) = self.state.files.clone().filter(|p| p.job_id == job.id) else {
            ui.label("Завантажую список файлів…");
            return;
        };
        ui.separator();
        ui.horizontal(|ui| {
            let width = ui.available_width();
            ui.add_sized(
                [(width - 218.0).max(90.0), 28.0],
                egui::Label::new(RichText::new("Назва").strong()),
            );
            ui.add_sized(
                [124.0, 28.0],
                egui::Label::new(RichText::new("Стан").strong()),
            );
            ui.add_sized(
                [78.0, 28.0],
                egui::Label::new(RichText::new("Розмір").strong()),
            );
        });
        ui.separator();
        let mut needed = None;
        let spacing = ui.spacing().item_spacing.y;
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::vertical()
            .id_salt(("files", job.id))
            .auto_shrink([false, false])
            .show_rows(ui, 28.0, page.total, |ui, range| {
                if (range.start < page.offset || range.end > page.offset + page.entries.len())
                    && self.files_pending.is_none()
                {
                    needed = Some(range.start.saturating_sub(16));
                }
                for ordinal in range {
                    let entry = ordinal
                        .checked_sub(page.offset)
                        .and_then(|n| page.entries.get(n));
                    let Some(e) = entry else {
                        ui.add_sized(
                            [ui.available_width(), 28.0],
                            egui::Label::new("Завантаження…"),
                        );
                        continue;
                    };
                    let filename = if e.relative.as_os_str().is_empty() {
                        name(&job.source)
                    } else {
                        e.relative.display().to_string()
                    };
                    let current = self
                        .state
                        .active
                        .as_ref()
                        .filter(|a| a.id == job.id)
                        .and_then(|a| a.progress.as_ref())
                        .filter(|p| p.entry == ordinal);
                    let state = if e.error.is_some() {
                        "Помилка"
                    } else if let Some(p) = current {
                        phase(p.phase)
                    } else {
                        entry_status(e.state)
                    };
                    let size = match e.kind {
                        EntryKind::Directory => "Каталог".into(),
                        _ => e.bytes.map(bytes).unwrap_or_else(|| "—".into()),
                    };
                    let selected = self.file_selected == Some(ordinal);
                    let response = table_row(ui, &filename, state, &size, selected);
                    if response.clicked() {
                        self.file_selected = Some(ordinal);
                    }
                    if response.double_clicked()
                        || (response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    {
                        self.file_selected = Some(ordinal);
                        self.file_open = true;
                    }
                    response.context_menu(|ui| {
                        if ui.button("Подробиці файла…").clicked() {
                            self.file_selected = Some(ordinal);
                            self.file_open = true;
                            ui.close();
                        }
                    });
                    response.on_hover_text(e.error.as_deref().unwrap_or(&filename));
                }
            });
        ui.spacing_mut().item_spacing.y = spacing;
        if let Some(offset) = needed {
            self.load_files(job.id, offset);
        }
    }
    pub(super) fn footer(&mut self, ui: &mut egui::Ui) {
        if self.closing {
            ui.label("Зберігаю стан і закриваю…");
        } else if self.state.planning {
            ui.label("Сканування джерела… Завдання ще не збережено.");
        } else if let Some(message) = self.banner.as_ref().or_else(|| self.state.errors.back()) {
            if ui
                .add(
                    egui::Button::new(RichText::new(message).color(ui.visuals().error_fg_color))
                        .frame(false)
                        .wrap_mode(egui::TextWrapMode::Truncate),
                )
                .clicked()
            {
                self.messages_open = true;
            }
        } else if let Some(message) = self.state.notice.as_ref() {
            if ui
                .add(
                    egui::Button::new(message)
                        .frame(false)
                        .wrap_mode(egui::TextWrapMode::Truncate),
                )
                .clicked()
            {
                self.messages_open = true;
            }
        } else if !self.state.ready {
            ui.label("Відкриваю журнал…");
        } else {
            ui.label(
                RichText::new("BLAKE3 · локальний журнал · відновлення після збою")
                    .small()
                    .weak(),
            );
        }
    }
}
