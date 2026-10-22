use super::*;
use crate::worker::Active;
use std::time::Duration;

impl App {
    pub(super) fn compact_controls_height(&self, ui: &egui::Ui) -> f32 {
        let mut labels = Vec::new();
        if let Some(job) = self.panel_job() {
            if self.state.active.as_ref().is_some_and(|a| a.id == job.id) {
                labels.extend(["Пауза", "Зупинити"]);
            } else {
                if matches!(
                    job.status.as_str(),
                    "queued" | "paused" | "interrupted" | "cancelled"
                ) || (job.operation == OperationKind::Move && job.status == "partial")
                {
                    labels.push(if job.status == "queued" {
                        "Запустити"
                    } else if job.operation == OperationKind::Move {
                        "Відновити переміщення"
                    } else {
                        "Відновити"
                    });
                }
                if job.operation == OperationKind::Copy
                    && matches!(job.status.as_str(), "completed" | "partial")
                {
                    labels.push("Перевірити");
                }
            }
            labels.push("Ще…");
        }
        compact_bar_height(ui, &labels)
    }
    pub(super) fn panel_job(&self) -> Option<Job> {
        self.selected_job()
            .or_else(|| self.state.active.as_ref().map(|active| active.job.clone()))
    }

    /// Called only by an explicit user action, never by transfer events.
    pub(super) fn set_compact(&mut self, compact: bool) {
        if self.compact == compact {
            return;
        }
        self.compact = compact;
        self.pending_focus = None;
        self.jobs_open = false;
        self.mode_focus_pending = true;
        self.pane = Pane::Commands;
        if compact {
            self.expanded_size = Some(self.context.content_rect().size());
            self.transfer_details = false;
            self.context
                .send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(
                    560.0, 280.0,
                )));
            self.context
                .send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(760.0, 300.0)));
        } else {
            self.context
                .send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(
                    760.0, 480.0,
                )));
            let size = self
                .expanded_size
                .take()
                .unwrap_or(egui::vec2(860.0, 520.0));
            self.context
                .send_viewport_cmd(egui::ViewportCommand::InnerSize(
                    size.max(egui::vec2(760.0, 480.0)),
                ));
        }
    }

    pub(super) fn compact_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let expand = ui.button("Повний вигляд");
            self.command_focus = Some(expand.id);
            if std::mem::take(&mut self.mode_focus_pending) {
                expand.request_focus();
            }
            if expand.clicked() {
                self.set_compact(false);
            }
            ui.toggle_value(&mut self.transfer_details, "Деталі");
            if ui.button("Черга").clicked() {
                self.jobs_open = !self.jobs_open;
                if self.jobs_open {
                    self.request_focus(Some(self.jobs_id()));
                }
            }
            if ui.button("Нова…").clicked() {
                self.open_form();
            }
        });
    }

    pub(super) fn transfer_graph(&self, ui: &mut egui::Ui, job: &Job, active: Option<&Active>) {
        let transfer =
            active.or_else(|| self.state.last_transfer.as_ref().filter(|a| a.id == job.id));
        if transfer.is_none() && !self.compact {
            return;
        }
        let now = transfer
            .map(|a| {
                if active.is_some() {
                    a.run_started.elapsed()
                } else {
                    a.telemetry.observed_at()
                }
            })
            .unwrap_or_default();
        let running = active.is_some_and(|a| a.request.is_empty());
        let rate = transfer
            .filter(|_| running)
            .and_then(|a| a.telemetry.rate_at(now));
        let speed = rate.map(rate_text).unwrap_or_else(|| "— МіБ/с".into());
        let eta = active
            .filter(|a| a.request.is_empty())
            .and_then(|a| {
                a.progress
                    .as_ref()
                    .and_then(|p| a.telemetry.stream_eta(now, p))
            })
            .map(duration_text)
            .unwrap_or_else(|| "—".into());
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("Швидкість запису: {speed}"));
            ui.label(RichText::new(format!("До кінця потоку: {eta}")).small());
            if let Some(transfer) = transfer {
                let written = if transfer.telemetry.overflowed {
                    "— (переповнення лічильника)".into()
                } else {
                    bytes(transfer.telemetry.written)
                };
                ui.label(RichText::new(format!("За запуск: {written}")).small());
            }
        });
        if transfer.is_none() {
            ui.label(
                RichText::new("Історія швидкості цього запуску недоступна")
                    .small()
                    .weak(),
            );
        }
        let history = transfer
            .map(|a| a.telemetry.graph_at(now))
            .unwrap_or_default();
        let maximum = history.iter().map(|(_, rate)| *rate).fold(1.0f64, f64::max);
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(
                ui.available_width().max(100.0),
                if self.compact { 60.0 } else { 90.0 },
            ),
            egui::Sense::hover(),
        );
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true,
            format!("Історія швидкості запису за 30 секунд; {speed}. Шкала: 0 – {}. Нові байти програми, без читання перевірки.", rate_text(maximum))));
        ui.ctx().accesskit_node_builder(response.id, |node| {
            node.set_role(egui::accesskit::Role::Image)
        });
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
        let plot = rect.shrink(5.0);
        let grid = egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color);
        for fraction in [0.0, 0.5, 1.0] {
            let y = egui::lerp(plot.y_range(), fraction);
            painter.line_segment(
                [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
                grid,
            );
        }
        if transfer.is_some() {
            let left = (now.as_secs_f64() - 30.0).max(0.0);
            let span = now.as_secs_f64().max(1.0) - left;
            let points: Vec<_> = history
                .iter()
                .map(|(at, rate)| {
                    egui::pos2(
                        plot.left() + (((at - left) / span).clamp(0.0, 1.0) as f32) * plot.width(),
                        plot.bottom() - ((rate / maximum).clamp(0.0, 1.0) as f32) * plot.height(),
                    )
                })
                .collect();
            if points.len() > 1 {
                painter.add(egui::Shape::line(
                    points,
                    egui::Stroke::new(2.0, ui.visuals().selection.stroke.color),
                ));
            }
            painter.text(
                plot.right_top(),
                egui::Align2::RIGHT_TOP,
                rate_text(maximum),
                egui::TextStyle::Small.resolve(ui.style()),
                ui.visuals().text_color(),
            );
        }
        response.on_hover_text("Фактично записані нові байти програми за секунду. Це не вимір фізичної швидкості диска.");
        if active.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
    }

    pub(super) fn compact_summary(&self, ui: &mut egui::Ui, job: &Job, active: Option<&Active>) {
        let state = active
            .map(|a| {
                if !a.request.is_empty() {
                    "Зупиняю, зберігаю прогрес…"
                } else {
                    a.progress
                        .as_ref()
                        .map(|p| phase(p.phase))
                        .unwrap_or("Підготовка…")
                }
            })
            .unwrap_or_else(|| job_status(job));
        ui.add(
            egui::Label::new(
                RichText::new(format!(
                    "{} · #{} · {state}",
                    operation_name(job.operation),
                    job.id
                ))
                .strong(),
            )
            .truncate(),
        )
        .on_hover_text(state);
        if ui.available_width() >= 620.0 {
            ui.columns(2, |columns| {
                source_line(&mut columns[0], job);
                path_line(
                    &mut columns[1],
                    "До:",
                    job.output_root.as_ref().unwrap_or(&job.destination),
                );
            });
        } else {
            source_line(ui, job);
            path_line(
                ui,
                "До:",
                job.output_root.as_ref().unwrap_or(&job.destination),
            );
        }
        if let Some(path) = active.and_then(|a| a.current_source.as_ref()) {
            path_line(ui, "Файл:", path);
        } else {
            ui.label(RichText::new("Поточний файл: —").weak());
        }
        if let Some(progress) = active
            .and_then(|a| a.progress.as_ref())
            .filter(|p| p.stream_index.is_some() && p.total > 0)
        {
            ui.add(
                egui::ProgressBar::new(
                    (progress.bytes as f64 / progress.total as f64).clamp(0.0, 1.0) as f32,
                )
                .desired_width(ui.available_width())
                .text(format!(
                    "Поточний потік: {} / {}",
                    bytes(progress.bytes),
                    bytes(progress.total)
                )),
            );
        } else if let Some(details) = self.state.details.as_ref().filter(|d| d.job.id == job.id) {
            let counts = job_counts(details);
            ui.add(egui::Label::new(&counts).truncate())
                .on_hover_text(counts);
        } else {
            ui.label("Поточний потік: —");
        }
        self.transfer_graph(ui, job, active);
    }
}

fn duration_text(duration: Duration) -> String {
    let seconds = duration.as_secs().max(1);
    if seconds >= 60 {
        format!("{} хв {} с", seconds / 60, seconds % 60)
    } else {
        format!("{seconds} с")
    }
}

fn rate_text(rate: f64) -> String {
    if rate > 0.0 && rate < 10_485.76 {
        "<0.01 МіБ/с".into()
    } else {
        format!("{:.2} МіБ/с", rate / 1_048_576.0)
    }
}

/// Explicit size avoids Panel's previous-frame cached height clipping a font-size change.
pub(super) fn compact_bar_height(ui: &egui::Ui, labels: &[&str]) -> f32 {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let width = (ui.available_width() - 16.0).max(1.0);
    let mut used = 0.0;
    let mut rows = 1;
    for label in labels {
        let item = ui.fonts_mut(|fonts| {
            fonts
                .layout_no_wrap((*label).to_owned(), font.clone(), ui.visuals().text_color())
                .size()
                .x
        }) + ui.spacing().button_padding.x * 2.0;
        if used > 0.0 && used + ui.spacing().item_spacing.x + item > width {
            rows += 1;
            used = 0.0;
        }
        if used > 0.0 {
            used += ui.spacing().item_spacing.x;
        }
        used += item;
    }
    let row = (ui.text_style_height(&egui::TextStyle::Button)
        + ui.spacing().button_padding.y * 2.0)
        .max(ui.spacing().interact_size.y);
    rows as f32 * row + (rows - 1) as f32 * ui.spacing().item_spacing.y + 8.0
}
