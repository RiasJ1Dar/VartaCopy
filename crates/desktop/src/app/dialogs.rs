use super::*;
impl App {
    fn form(&mut self) {
        if !self.new_operation || self.chooser.is_some() {
            return;
        }
        let ctx = self.context.clone();
        let mut modal = egui::Modal::new(egui::Id::new("new_operation"));
        if self.contrast.is_some() {
            modal = modal.backdrop_color(egui::Color32::TRANSPARENT);
        }
        let response = modal.show(&ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 64.0).clamp(280.0, 560.0));
            ui.label(RichText::new("Нова операція").size(20.0).strong());
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Дія:");
                ui.radio_value(&mut self.operation, OperationKind::Copy, "Копіювати");
                ui.radio_value(&mut self.operation, OperationKind::Move, "Перемістити");
            });
            egui::ScrollArea::vertical()
                .id_salt("operation_fields")
                .max_height((ctx.content_rect().height() - 130.0).max(64.0))
                .show(ui, |ui| {
                    let label = ui.label("Джерело — файл або каталог");
                    let focus = std::mem::take(&mut self.form_focus);
                    self.source.show(ui, "source", label.id, focus);
                    if !self.additional_sources.is_empty() {
                        egui::CollapsingHeader::new(format!(
                            "Ще {} джерел",
                            self.additional_sources.len()
                        ))
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical().max_height(100.0).show_rows(
                                ui,
                                20.0,
                                self.additional_sources.len(),
                                |ui, range| {
                                    for i in range {
                                        ui.add(
                                            egui::Label::new(
                                                self.additional_sources[i].display().to_string(),
                                            )
                                            .truncate(),
                                        );
                                    }
                                },
                            );
                            if ui.button("Прибрати додаткові джерела").clicked()
                            {
                                self.additional_sources.clear();
                            }
                        });
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.add_enabled_ui(self.picker.is_none(), |ui| {
                            if ui.button("Обрати файл…").clicked() {
                                self.pick(Picker::File);
                            }
                            if ui.button("Обрати каталог…").clicked() {
                                self.pick(Picker::Folder);
                            }
                            if ui.button("Вставити файли…").clicked(){self.paste_files();}
                        });
                    });
                    let label = ui.label("Призначення — наявний каталог");
                    self.destination.show(ui, "destination", label.id, false);
                    if ui
                        .add_enabled(
                            self.picker.is_none(),
                            egui::Button::new("Обрати призначення…"),
                        )
                        .clicked()
                    {
                        self.pick(Picker::Destination);
                    }
                    egui::CollapsingHeader::new("Додаткові параметри").show(ui, |ui| {
                        ui.label("Якщо назва зайнята:");
                        egui::ComboBox::from_id_salt("conflict")
                            .selected_text(
                                ["Показати помилку", "Зберегти обидва", "Пропустити"]
                                    [self.conflict],
                            )
                            .show_ui(ui, |ui| {
                                for (i, name) in
                                    ["Показати помилку", "Зберегти обидва", "Пропустити"]
                                        .iter()
                                        .enumerate()
                                {
                                    ui.selectable_value(&mut self.conflict, i, *name);
                                }
                            });
                        ui.checkbox(
                            &mut self.skip_links,
                            "Пропускати посилання (частковий результат)",
                        );
                    });
                    if ui.add_enabled(
                        self.state.ready && !self.state.dispatching && !self.source.text.trim().is_empty() && !self.destination.text.trim().is_empty(),
                        egui::Button::new("Обрати Explorer або VartaCopy…")
                    ).clicked(){self.choose_form_engine();}
                    ui.label(
                        RichText::new(match self.operation {
                            OperationKind::Copy => "Копія створюється всередині призначення. Перевірка BLAKE3 увімкнена.",
                            OperationKind::Move => "Джерело видаляється лише після перевірки копії. При зупинці збережені файли залишаються для відновлення.",
                        })
                        .small()
                        .weak(),
                    );
                });
            let enabled = self.state.ready
                && !self.state.planning
                && !self.state.dispatching
                && !self.closing
                && self.picker.is_none()
                && !self.source.text.trim().is_empty()
                && !self.destination.text.trim().is_empty();
            ui.separator();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(enabled, egui::Button::new(operation_action(self.operation)).selected(true))
                    .clicked()
                {
                    self.submit(true);
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("До черги"))
                    .clicked()
                {
                    self.submit(false);
                }
                if ui.button("Скасувати").clicked() {
                    self.close_form();
                }
            });
        });
        if response.should_close() && self.picker.is_none() {
            self.close_form();
        }
    }
    pub(super) fn dialogs(&mut self) {
        self.form();
        self.engine_chooser();
        let ctx = self.context.clone();
        if self.jobs_open {
            let mut open = true;
            egui::Window::new("Черга та історія")
                .open(&mut open)
                .default_width(280.0)
                .max_height((ctx.content_rect().height() - 48.0).max(100.0))
                .show(&ctx, |ui| self.sidebar(ui));
            self.jobs_open = open;
            if !open {
                self.pending_focus = None;
                self.restore_focus();
            }
        }
        if self.details_open {
            let mut open = true;
            egui::Window::new("Подробиці операції")
                .open(&mut open)
                .default_width(580.0)
                .max_width((ctx.content_rect().width() - 32.0).max(240.0))
                .resizable(true)
                .show(&ctx, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height((ctx.content_rect().height() - 110.0).clamp(80.0, 400.0))
                        .show(ui, |ui| {
                            if let Some(job) = self.selected_job() {
                                ui.label(format!(
                                    "{} #{} · {}",
                                    operation_name(job.operation),
                                    job.id,
                                    job_status(&job)
                                ));
                                ui.label(job.source.display().to_string());
                                ui.label(
                                    job.output_root
                                        .as_ref()
                                        .unwrap_or(&job.destination)
                                        .display()
                                        .to_string(),
                                );
                                if let Some(e) = &job.last_error {
                                    ui.colored_label(ui.visuals().error_fg_color, e);
                                }
                                if let Some(d) =
                                    self.state.details.clone().filter(|d| d.job.id == job.id)
                                {
                                    ui.label(job_counts(&d));
                                    for issue in &d.issues {
                                        ui.colored_label(
                                            ui.visuals().error_fg_color,
                                            format!(
                                                "{}: {}",
                                                issue.relative.display(),
                                                issue.message
                                            ),
                                        );
                                    }
                                    if !d.issues.is_empty() || d.issues_offset > 0 {
                                        ui.horizontal(|ui| {
                                            if ui
                                                .add_enabled(
                                                    d.issues_offset > 0,
                                                    egui::Button::new("Попередні помилки"),
                                                )
                                                .clicked()
                                            {
                                                self.command(Command::Inspect {
                                                    id: job.id,
                                                    offset: d.issues_offset.saturating_sub(32),
                                                });
                                            }
                                            if ui
                                                .add_enabled(
                                                    d.has_more,
                                                    egui::Button::new("Наступні помилки"),
                                                )
                                                .clicked()
                                            {
                                                self.command(Command::Inspect {
                                                    id: job.id,
                                                    offset: d.issues_offset + 32,
                                                });
                                            }
                                        });
                                    }
                                }
                                if ui
                                    .add_enabled(
                                        self.picker.is_none()
                                            && !self
                                                .state
                                                .active
                                                .as_ref()
                                                .is_some_and(|a| a.id == job.id),
                                        egui::Button::new("Експорт JSON…"),
                                    )
                                    .clicked()
                                {
                                    self.pick(Picker::Report(job.id));
                                }
                            }
                        });
                });
            self.details_open = open;
        }
        if self.file_open {
            let mut open = true;
            egui::Window::new("Подробиці файла")
                .open(&mut open)
                .default_width(560.0)
                .max_width((ctx.content_rect().width() - 32.0).max(240.0))
                .show(&ctx, |ui| {
                    if let Some((job, e)) = self.file_details.as_ref() {
                        let source = if e.relative.as_os_str().is_empty() {
                            job.source.clone()
                        } else {
                            job.source.join(&e.relative)
                        };
                        ui.label(entry_status(e.state));
                        ui.label(source.display().to_string());
                        let copy = ui.button("Копіювати повний шлях");
                        if copy.clicked() {
                            ui.ctx().copy_text(source.to_string_lossy().into_owned());
                        }
                        if let Some(error) = &e.error {
                            ui.colored_label(ui.visuals().error_fg_color, error);
                        }
                        if let Some(size) = e.bytes {
                            ui.label(format!("Розмір усіх потоків: {}", bytes(size)));
                        }
                        if self.file_focus {
                            self.request_focus(Some(copy.id));
                            self.file_focus = false;
                        }
                    } else {
                        ui.label("Завантажую подробиці файла…");
                    }
                });
            if !open {
                self.restore_focus();
            }
            self.file_open = open;
        }
        if self.messages_open {
            let mut open = true;
            egui::Window::new("Повідомлення")
                .open(&mut open)
                .default_width(580.0)
                .max_width((ctx.content_rect().width() - 32.0).max(240.0))
                .show(&ctx, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height((ctx.content_rect().height() - 110.0).clamp(80.0, 360.0))
                        .show(ui, |ui| {
                            if let Some(e) = &self.banner {
                                ui.colored_label(ui.visuals().error_fg_color, e);
                            }
                            for e in &self.state.errors {
                                ui.colored_label(ui.visuals().error_fg_color, e);
                                ui.separator();
                            }
                            if let Some(n) = &self.state.notice {
                                ui.label(n);
                            }
                        });
                    if ui.button("Очистити повідомлення").clicked() {
                        self.banner = None;
                        self.command(Command::ClearErrors);
                    }
                });
            self.messages_open = open;
        }
        if self.settings_open {
            let mut open = true;
            egui::Window::new("Вигляд і правила")
                .open(&mut open)
                .default_width(560.0)
                .max_width((ctx.content_rect().width() - 32.0).max(240.0))
                .show(&ctx, |ui| self.settings_contents(ui));
            self.settings_open = open;
        }
    }
    fn settings_contents(&mut self, ui: &mut egui::Ui) {
        let ctx = self.context.clone();
        egui::ScrollArea::vertical()
            .max_height((ctx.content_rect().height() - 110.0).clamp(80.0, 400.0))
            .show(ui, |ui| {
                if self.contrast.is_some() {
                    ui.label("Застосовано контрастну тему Windows.");
                }
                ui.add_enabled_ui(self.contrast.is_none(), |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Тема:");
                        for (value, label) in [
                            (egui::ThemePreference::System, "Системна"),
                            (egui::ThemePreference::Light, "Світла"),
                            (egui::ThemePreference::Dark, "Темна"),
                        ] {
                            if ui.selectable_value(&mut self.theme, value, label).changed() {
                                ctx.set_theme(self.theme);
                            }
                        }
                    });
                });
                let mut zoom = ctx.zoom_factor();
                egui::ComboBox::from_label("Масштаб")
                    .selected_text(format!("{:.0}%", zoom * 100.0))
                    .show_ui(ui, |ui| {
                        for value in [1.0, 1.25, 1.5, 2.0] {
                            let label = format!("{:.0}%", value * 100.0);
                            if ui.selectable_value(&mut zoom, value, label).changed() {
                                ctx.set_zoom_factor(value);
                            }
                        }
                    });
                ui.label("Масштаб інтерфейсу: Ctrl+плюс / Ctrl+мінус; Ctrl+0 — скинути.");
                ui.label(concat!(
                    "F6 / Shift+F6 — між командами, чергою й файлами. ",
                    "У списках: стрілки, Home/End, Page Up/Down. ",
                    "Enter — подробиці файла, Esc — закрити.",
                ));
                ui.separator();
                ui.label(format!("Журнал: {}", self.journal.display()));
                ui.separator();
                self.integration.show(ui,!self.closing);
                ui.separator();
                ui.strong("Додаткова команда в контекстному меню");
                ui.label("Меню Провідника для поточного користувача. Після перенесення програми додайте меню з нового розташування.");
                ui.add_enabled_ui(self.shell_action.is_none(),|ui| {ui.horizontal_wrapped(|ui| {
                    if ui.button("Додати меню Провідника").clicked() {self.shell_registration(true);}
                    if ui.button("Прибрати меню VartaCopy").clicked() {self.shell_registration(false);}
                });});
                ui.label(concat!(
                    "Після збою виберіть операцію в історії та натисніть «Відновити». ",
                    "Записані блоки перевіряються повторно.",
                ));
                ui.label(concat!(
                    "Зайняті каталоги не об’єднуються. ",
                    "Пропущені елементи дають частковий результат. ",
                    "ACL і власник успадковуються від призначення.",
                ));
            });
    }
}
