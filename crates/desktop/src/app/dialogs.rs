use super::*;
impl App {
    fn form(&mut self) {
        if !self.new_operation {
            return;
        }
        let ctx = self.context.clone();
        let response = egui::Modal::new(egui::Id::new("new_operation")).show(&ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 64.0).clamp(280.0, 560.0));
            ui.label(RichText::new("Нова операція").size(20.0).strong());
            ui.add_space(6.0);
            egui::ScrollArea::vertical()
                .id_salt("operation_fields")
                .max_height((ctx.content_rect().height() - 130.0).max(64.0))
                .show(ui, |ui| {
                    let label = ui.label("Джерело — файл або каталог");
                    let focus = std::mem::take(&mut self.form_focus);
                    self.source.show(ui, "source", label.id, focus);
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(self.picker.is_none(), |ui| {
                            if ui.button("Обрати файл…").clicked() {
                                self.pick(Picker::File);
                            }
                            if ui.button("Обрати каталог…").clicked() {
                                self.pick(Picker::Folder);
                            }
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
                    ui.label(
                        RichText::new(
                            "Копія створюється всередині призначення. Перевірка BLAKE3 увімкнена.",
                        )
                        .small()
                        .weak(),
                    );
                });
            let enabled = self.state.ready
                && !self.state.planning
                && !self.closing
                && self.picker.is_none()
                && !self.source.text.trim().is_empty()
                && !self.destination.text.trim().is_empty();
            ui.separator();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(enabled, egui::Button::new("Копіювати").selected(true))
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
                    self.new_operation = false;
                }
            });
        });
        if response.should_close() && self.picker.is_none() {
            self.new_operation = false;
        }
    }
    pub(super) fn dialogs(&mut self) {
        self.form();
        let ctx = self.context.clone();
        if self.details_open {
            let mut open = true;
            egui::Window::new("Подробиці операції")
                .open(&mut open)
                .default_width(580.0)
                .resizable(true)
                .show(&ctx, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(400.0)
                        .show(ui, |ui| {
                            if let Some(job) = self.selected_job() {
                                ui.label(format!("Операція #{} · {}", job.id, status(&job.status)));
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
                                    ui.label(format!(
                                        "Готово {} · пропущено {} · залишилося {}",
                                        d.entries_committed, d.entries_skipped, d.entries_pending
                                    ));
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
                .show(&ctx, |ui| {
                    if let Some((job, e)) = self.selected_job().and_then(|j| {
                        self.state.files.as_ref().and_then(|p| {
                            p.entries
                                .iter()
                                .find(|e| Some(e.ordinal) == self.file_selected)
                                .map(|e| (j, e))
                        })
                    }) {
                        let source = if e.relative.as_os_str().is_empty() {
                            job.source.clone()
                        } else {
                            job.source.join(&e.relative)
                        };
                        ui.label(entry_status(e.state));
                        ui.label(source.display().to_string());
                        if ui.button("Копіювати повний шлях").clicked() {
                            ui.ctx().copy_text(source.to_string_lossy().into_owned());
                        }
                        if let Some(error) = &e.error {
                            ui.colored_label(ui.visuals().error_fg_color, error);
                        }
                        if let Some(size) = e.bytes {
                            ui.label(format!("Розмір усіх потоків: {}", bytes(size)));
                        }
                    } else {
                        ui.label("Поверніться до рядка файла, щоб завантажити подробиці.");
                    }
                });
            self.file_open = open;
        }
        if self.messages_open {
            let mut open = true;
            egui::Window::new("Повідомлення")
                .open(&mut open)
                .default_width(580.0)
                .show(&ctx, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(360.0)
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
            egui::Window::new("Вигляд і правила").open(&mut open).default_width(560.0).show(&ctx,|ui| {
                ui.horizontal(|ui| {
                    ui.label("Тема:");
                    for (value,label) in [(egui::ThemePreference::System,"Системна"),(egui::ThemePreference::Light,"Світла"),(egui::ThemePreference::Dark,"Темна")] {
                        if ui.selectable_value(&mut self.theme,value,label).changed() {ctx.set_theme(self.theme);}
                    }
                });
                ui.label("Масштаб інтерфейсу: Ctrl+плюс / Ctrl+мінус; Ctrl+0 — скинути.");
                ui.separator();
                ui.label(format!("Журнал: {}",self.journal.display()));
                ui.label("Після збою виберіть операцію в історії та натисніть «Відновити». Записані блоки перевіряються повторно.");
                ui.label("Зайняті каталоги не об’єднуються. Пропущені елементи дають частковий результат. ACL і власник успадковуються від призначення.");
            });
            self.settings_open = open;
        }
    }
}
