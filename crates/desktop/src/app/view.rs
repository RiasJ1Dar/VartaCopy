use super::*;
impl App {
    fn job_row(&mut self, ui: &mut egui::Ui, job: &Job, index: usize, list_id: egui::Id) {
        let running = self.state.active.as_ref().is_some_and(|a| a.id == job.id);
        let selected = self.selected == Some(job.id);
        let caption = format!(
            "#{} · {} · {}",
            job.id,
            operation_name(job.operation),
            short_status(if running { "running" } else { &job.status })
        );
        let row = ui
            .scope_builder(
                egui::UiBuilder::new()
                    .id(egui::Id::new(("job", job.id)))
                    .sense(egui::Sense::CLICK),
                |ui| {
                    egui::Frame::new()
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
                            ui.add(egui::Label::new(source_caption(job)).truncate());
                            ui.label(RichText::new(&caption).small());
                        });
                },
            )
            .response;
        row.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Other,
                ui.is_enabled(),
                format!("{} {caption}", source_caption(job)),
            )
        });
        ui.ctx().accesskit_node_builder(row.id, |n| {
            n.set_role(egui::accesskit::Role::ListBoxOption);
            n.set_selected(selected);
            n.set_position_in_set(index + 1);
        });
        if selected {
            if self.job_scroll {
                row.scroll_to_me(Some(egui::Align::Center));
                self.job_scroll = false;
            }
            ui.ctx().accesskit_node_builder(list_id, |n| {
                n.set_active_descendant(row.id.accesskit_id())
            });
        }
        if row.clicked() {
            self.select(job.id);
            self.pane = Pane::Jobs;
            ui.memory_mut(|m| m.request_focus(list_id));
        }
        row.on_hover_text(job.source.display().to_string());
    }
    pub(super) fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.history, false, "Черга");
            ui.selectable_value(&mut self.history, true, "Історія");
        });
        ui.separator();
        let mut jobs = if self.history {
            self.state.history.clone()
        } else {
            self.state.queue.clone()
        };
        if !self.history
            && let Some(active) = self.state.active.as_ref()
        {
            jobs.insert(0, active.job.clone());
        }
        let id = self.jobs_id();
        if !self.history && jobs.len() >= 32 {
            ui.label(RichText::new("Перші 32 завдання; усі — в історії").small());
        }
        if ui.memory(|m| m.has_focus(id)) && self.main_keys() {
            self.pane = Pane::Jobs;
            lock_arrows(ui.ctx(), id);
            let selected = jobs.iter().position(|j| Some(j.id) == self.selected);
            if let Some(index) = navigate(ui, selected, jobs.len(), 8) {
                self.select(jobs[index].id);
                self.job_scroll = true;
            }
        }
        let bottom = if self.history { 36.0 } else { 0.0 };
        let label = if self.history {
            "Історія операцій"
        } else {
            "Черга операцій"
        };
        let list = ui.scope_builder(
            egui::UiBuilder::new()
                .id(id)
                .sense(egui::Sense::focusable_noninteractive()),
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("queue")
                    .max_height((ui.available_height() - bottom).max(36.0))
                    .show(ui, |ui| {
                        for (index, job) in jobs.iter().enumerate() {
                            self.job_row(ui, job, index, id);
                        }
                        if jobs.is_empty() {
                            ui.label(if self.history {
                                "Історія порожня"
                            } else {
                                "Черга порожня"
                            });
                        }
                    });
            },
        );
        composite(
            &list.response,
            egui::accesskit::Role::ListBox,
            label,
            jobs.len(),
        );
        // composite clears active descendant so it cannot point at a removed job.
        if let Some(index) = jobs.iter().position(|j| Some(j.id) == self.selected) {
            ui.ctx().accesskit_node_builder(id, |n| {
                n.set_active_descendant(egui::Id::new(("job", jobs[index].id)).accesskit_id())
            });
        }
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
        if self.compact {
            self.compact_toolbar(ui);
            return;
        }
        ui.horizontal(|ui| {
            let new = ui
                .add_enabled(
                    self.state.ready && !self.closing,
                    egui::Button::new("Нова операція…"),
                )
                .on_hover_text("Ctrl+N");
            self.command_focus = Some(new.id);
            if new.has_focus() {
                self.pane = Pane::Commands;
            }
            if new.clicked() {
                self.open_form();
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
            let compact = ui.button("Компактний вигляд");
            if std::mem::take(&mut self.mode_focus_pending) {
                compact.request_focus();
            }
            if compact.clicked() {
                self.set_compact(true);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button("Меню", |ui| {
                    if ui.button("Черга та історія…").clicked() {
                        self.jobs_open = ui.ctx().content_rect().width() < 680.0;
                        self.job_scroll = true;
                        self.request_focus(Some(self.jobs_id()));
                        ui.close();
                    }

                    if ui.button("Вигляд і правила…").clicked() {
                        self.settings_open = true;
                        ui.close();
                    }
                    if ui.button("Повідомлення…").clicked() {
                        self.messages_open = true;
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            !self.closing && self.tray.as_ref().is_some_and(|t| t.available()),
                            egui::Button::new("Згорнути у трей"),
                        )
                        .clicked()
                    {
                        self.hide_to_tray();
                        ui.close();
                    }
                });
            });
        });
    }
    pub(super) fn operation(&mut self, ui: &mut egui::Ui, job: &Job) {
        let active = self.state.active.clone().filter(|a| a.id == job.id);
        if self.compact {
            self.compact_summary(ui, job, active.as_ref());
            return;
        }
        ui.add(
            egui::Label::new(
                RichText::new(format!(
                    "{} · {} · #{}",
                    operation_name(job.operation),
                    source_caption(job),
                    job.id
                ))
                .strong(),
            )
            .truncate(),
        );
        if self.compact || ui.available_height() >= 330.0 {
            source_line(ui, job);
            path_line(
                ui,
                "До:",
                job.output_root.as_ref().unwrap_or(&job.destination),
            );
        }
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
            if let Some(path) = a.current_source.as_ref() {
                path_line(ui, "Файл:", path);
            }
            let (fraction, text) = a
                .progress
                .as_ref()
                .filter(|p| p.total > 0 && p.stream_index.is_some())
                .map(|p| {
                    (
                        (p.bytes as f64 / p.total as f64).clamp(0.0, 1.0) as f32,
                        format!("Поточний потік: {} / {}", bytes(p.bytes), bytes(p.total)),
                    )
                })
                .unwrap_or((0.0, "Розмір поточного потоку: —".into()));
            ui.add(
                egui::ProgressBar::new(fraction)
                    .desired_width(ui.available_width())
                    .text(text),
            );
        } else {
            ui.label(job_status(job));
            if let Some(d) = self.state.details.as_ref().filter(|d| d.job.id == job.id) {
                ui.label(RichText::new(job_counts(d)).weak());
            }
        }
        self.transfer_graph(ui, job, active.as_ref());
        if let Some(e) = &job.last_error
            && ui
                .add(
                    egui::Button::new(
                        RichText::new(crate::error_text::explain(e).summary)
                            .color(ui.visuals().error_fg_color),
                    )
                    .frame(false)
                    .wrap_mode(egui::TextWrapMode::Truncate),
                )
                .clicked()
        {
            self.details_open = true;
        }
        if !self.compact {
            self.operation_controls(ui, job, active.as_ref());
        }
    }
    pub(super) fn operation_controls(
        &mut self,
        ui: &mut egui::Ui,
        job: &Job,
        active: Option<&crate::worker::Active>,
    ) {
        ui.horizontal_wrapped(|ui| {
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
                let resumable = matches!(
                    job.status.as_str(),
                    "queued" | "paused" | "interrupted" | "cancelled"
                ) || (job.operation == OperationKind::Move
                    && job.status == "partial");
                if resumable
                    && ui
                        .button(if job.status == "queued" {
                            "Запустити"
                        } else if job.operation == OperationKind::Move {
                            "Відновити переміщення"
                        } else {
                            "Відновити"
                        })
                        .clicked()
                {
                    self.command(Command::Resume(job.id));
                }
                if job.operation == OperationKind::Copy
                    && matches!(job.status.as_str(), "completed" | "partial")
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
                        self.command(Command::Reorder {
                            id: job.id,
                            earlier: true,
                        });
                        ui.close();
                    }
                    if ui.button("Нижче в черзі").clicked() {
                        self.command(Command::Reorder {
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
        let id = self.files_id();
        if ui.memory(|m| m.has_focus(id)) && self.main_keys() {
            self.pane = Pane::Files;
            lock_arrows(ui.ctx(), id);
            if let Some(target) = navigate(
                ui,
                self.file_selected,
                page.total,
                self.file_visible.len().saturating_sub(1),
            ) {
                self.file_selected = Some(target);
                if !self.file_visible.contains(&target) {
                    self.file_scroll = Some(target.saturating_sub(self.file_visible.len() / 2));
                }
                if target < page.offset || target >= page.offset + page.entries.len() {
                    self.load_files(job.id, target.saturating_sub(16));
                }
            }
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
                self.inspect_file();
            }
        }
        let mut needed = None;
        let mut active_row = None;
        let grid = ui.scope_builder(
            egui::UiBuilder::new()
                .id(id)
                .sense(egui::Sense::focusable_noninteractive()),
            |ui| {
                ui.separator();
                let widths = column_widths(ui.available_width());
                ui.horizontal(|ui| {
                    for (column, (label, width)) in ["Назва", "Стан", "Розмір"]
                        .into_iter()
                        .zip(widths)
                        .enumerate()
                    {
                        let r = table_label(
                            ui,
                            width,
                            24.0,
                            RichText::new(label).strong(),
                            column == 2,
                        );
                        ui.ctx().accesskit_node_builder(r.id, |n| {
                            n.set_role(egui::accesskit::Role::ColumnHeader);
                            n.set_label(label);
                            n.set_column_index(column);
                        });
                    }
                });
                ui.separator();
                ui.spacing_mut().item_spacing.y = 0.0;
                let mut scroll = egui::ScrollArea::vertical()
                    .id_salt(("files", job.id))
                    .auto_shrink([false, false]);
                if let Some(target) = self.file_scroll.take() {
                    scroll = scroll.vertical_scroll_offset(target as f32 * 28.0);
                }
                scroll.show_rows(ui, 28.0, page.total, |ui, range| {
                    self.file_visible = range.clone();
                    if (range.start < page.offset || range.end > page.offset + page.entries.len())
                        && self.files_pending.is_none()
                    {
                        needed = Some(range.start.saturating_sub(16));
                    }
                    for ordinal in range {
                        let Some(e) = ordinal
                            .checked_sub(page.offset)
                            .and_then(|n| page.entries.get(n))
                        else {
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
                        let row_id = egui::Id::new(("file_row", job.id, ordinal));
                        let row = ui
                            .scope_builder(
                                egui::UiBuilder::new().id(row_id).sense(egui::Sense::CLICK),
                                |ui| {
                                    if selected {
                                        ui.visuals_mut().override_text_color =
                                            Some(ui.visuals().selection.stroke.color);
                                    }
                                    let rect = egui::Rect::from_min_size(
                                        ui.cursor().min,
                                        egui::vec2(ui.available_width(), 28.0),
                                    );
                                    if selected {
                                        ui.painter().rect_filled(
                                            rect,
                                            0,
                                            ui.visuals().selection.bg_fill,
                                        );
                                    }
                                    ui.horizontal(|ui| {
                                        for (column, (text, width)) in
                                            [filename.as_str(), state, size.as_str()]
                                                .into_iter()
                                                .zip(column_widths(ui.available_width()))
                                                .enumerate()
                                        {
                                            let r = table_label(ui, width, 28.0, text, column == 2);
                                            ui.ctx().accesskit_node_builder(r.id, |n| {
                                                n.set_role(egui::accesskit::Role::Cell);
                                                n.set_label(text);
                                                n.set_row_index(ordinal);
                                                n.set_column_index(column);
                                            });
                                        }
                                    });
                                },
                            )
                            .response;
                        row.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Other,
                                true,
                                format!("{filename}, {state}, {size}"),
                            )
                        });
                        ui.ctx().accesskit_node_builder(row.id, |n| {
                            n.set_role(egui::accesskit::Role::Row);
                            n.set_row_index(ordinal);
                            n.set_selected(selected);
                        });
                        if selected {
                            active_row = Some(row.id);
                        }
                        if row.clicked() {
                            self.file_selected = Some(ordinal);
                            self.pane = Pane::Files;
                            ui.memory_mut(|m| m.request_focus(id));
                        }
                        if row.double_clicked() {
                            self.file_selected = Some(ordinal);
                            self.inspect_file();
                        }
                        row.context_menu(|ui| {
                            if ui.button("Подробиці файла…").clicked() {
                                self.file_selected = Some(ordinal);
                                self.inspect_file();
                                ui.close();
                            }
                        });
                        row.on_hover_text(e.error.as_deref().unwrap_or(&filename));
                    }
                });
            },
        );
        composite(
            &grid.response,
            egui::accesskit::Role::Grid,
            &format!("Файли операції #{}", job.id),
            page.total,
        );
        if let Some(row) = active_row {
            ui.ctx()
                .accesskit_node_builder(id, |n| n.set_active_descendant(row.accesskit_id()));
        }
        if let Some(offset) = needed {
            self.load_files(job.id, offset);
        }
    }
    pub(super) fn footer(&mut self, ui: &mut egui::Ui) {
        if self.closing {
            ui.label("Зберігаю стан і закриваю…");
        } else if self.state.dispatching {
            ui.label(
                "Передаю запит обраному обробнику… Для Explorer скасування — у системному діалозі.",
            );
        } else if self.state.planning {
            ui.label("Сканування джерела… Завдання ще не збережено.");
        } else if let Some(message) = self.banner.as_ref().or(self.state.current_error.as_ref()) {
            if ui
                .add(
                    egui::Button::new(
                        RichText::new(crate::error_text::explain(message).summary)
                            .color(ui.visuals().error_fg_color),
                    )
                    .frame(false)
                    .wrap_mode(egui::TextWrapMode::Truncate),
                )
                .clicked()
            {
                self.messages_open = true;
            }
        } else if let Some(message) = self.state.notice.as_ref() {
            let caption = if crate::error_text::technical_notice(message) {
                crate::error_text::explain(message).summary
            } else {
                message
            };
            if ui
                .add(
                    egui::Button::new(caption)
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
