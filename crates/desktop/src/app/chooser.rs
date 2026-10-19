use super::*;
use vartacopy_core::journal::DispatchEngine;
use vartacopy_shell_windows::transfer::{
    TransferOperation, TransferOrigin, TransferRequest, encode_transfer, new_transfer_id,
};

impl App {
    pub fn receive_transfer(
        &mut self,
        delivery: vartacopy_shell_windows::transfer::TransferDelivery,
    ) -> std::io::Result<()> {
        use vartacopy_shell_windows::transfer::TransferDelivery;
        match delivery {
            TransferDelivery::Choose(request) => self.offer_transfer(request),
            TransferDelivery::SelectedVarta(request) => {
                encode_transfer(&request)?;
                if request.origin != TransferOrigin::Explorer || self.pending_varta.is_some() {
                    return Err(std::io::Error::other(
                        "Недопустимий або зайнятий канал вибору Провідника",
                    ));
                }
                self.pending_varta = Some(request);
                self.context.request_repaint();
                Ok(())
            }
        }
    }
    pub(super) fn dispatch_pending_varta(&mut self) {
        if self.state.ready
            && !self.state.planning
            && !self.state.dispatching
            && !self.closing
            && self.chooser.is_none()
            && let Some(request) = self.pending_varta.clone()
            && self.command(Command::Dispatch {
                request,
                engine: DispatchEngine::VartaCopy,
                conflict: Conflict::Error,
            })
        {
            self.pending_varta = None;
            self.close_form();
        }
    }
    pub fn offer_transfer(&mut self, request: TransferRequest) -> std::io::Result<()> {
        encode_transfer(&request)?;
        if self.chooser.is_some() {
            return Err(std::io::Error::other(
                "Спочатку завершіть поточний вибір обробника",
            ));
        }
        self.chooser = Some(request);
        self.chooser_focus = true;
        self.context.request_repaint();
        Ok(())
    }
    pub(super) fn offer_paths(
        &mut self,
        sources: Vec<PathBuf>,
        origin: TransferOrigin,
        operation: Option<TransferOperation>,
    ) -> std::io::Result<()> {
        let operation = operation.unwrap_or(match self.operation {
            OperationKind::Copy => TransferOperation::Copy,
            OperationKind::Move => TransferOperation::Move,
        });
        self.offer_transfer(TransferRequest {
            id: new_transfer_id()?,
            operation,
            origin,
            sources,
            destination: self.destination.path(),
        })
    }
    pub(super) fn paste_files(&mut self) {
        if self.destination.text.trim().is_empty() {
            self.banner = Some("Спочатку оберіть призначення для вставлення файлів".into());
            return;
        }
        let result = vartacopy_shell_windows::clipboard::capture_clipboard().and_then(|snapshot| {
            self.offer_paths(
                snapshot.sources,
                TransferOrigin::AppPaste,
                Some(snapshot.preferred_operation),
            )
        });
        if let Err(error) = result {
            self.banner = Some(error.to_string());
        }
    }
    pub(super) fn choose_form_engine(&mut self) {
        let mut paths = vec![self.source.path()];
        paths.extend(self.additional_sources.clone());
        if let Err(error) = self.offer_paths(paths, TransferOrigin::AppForm, None) {
            self.banner = Some(error.to_string());
        }
    }
    pub(super) fn engine_chooser(&mut self) {
        let Some(request) = self.chooser.clone() else {
            return;
        };
        let ctx = self.context.clone();
        let mut choice = None;
        let mut cancel = false;
        let mut modal = egui::Modal::new(egui::Id::new("engine_chooser"));
        // A top-left anchor does not reuse the previous viewport's cached width
        // when a compact window shrinks between frames.
        let width = (ctx.content_rect().width() - 64.0).clamp(280.0, 560.0);
        modal = modal.area(
            egui::Modal::default_area(egui::Id::new("engine_chooser")).anchor(
                egui::Align2::LEFT_TOP,
                egui::vec2(
                    ((ctx.content_rect().width() - width - 16.0) / 2.0).max(8.0),
                    8.0,
                ),
            ),
        );
        if self.contrast.is_some() {
            modal = modal.backdrop_color(egui::Color32::TRANSPARENT);
        }
        let response = modal.show(&ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 64.0).clamp(280.0, 560.0));
            ui.heading(match request.operation {
                TransferOperation::Copy => "Копіювати файли",
                TransferOperation::Move => "Перемістити файли",
            });
            egui::ScrollArea::vertical()
                .id_salt("chooser_body")
                .max_height((ctx.content_rect().height() - 115.0).max(80.0))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("chooser_paths")
                        .max_height((ctx.content_rect().height() - 245.0).clamp(60.0, 180.0))
                        .show(ui, |ui| {
                            ui.label(format!("Джерел: {}", request.sources.len()));
                            for path in request.sources.iter().take(4) {
                                ui.add(egui::Label::new(path.display().to_string()).truncate())
                                    .on_hover_text(path.display().to_string());
                            }
                            if request.sources.len() > 4 {
                                ui.label(format!("…і ще {}", request.sources.len() - 4));
                            }
                            ui.strong("Призначення:");
                            ui.add(
                                egui::Label::new(request.destination.display().to_string())
                                    .truncate(),
                            )
                            .on_hover_text(request.destination.display().to_string());
                        });
                    ui.separator();
                    let ready = self.state.ready
                        && !self.state.planning
                        && !self.state.dispatching
                        && self.state.active.is_none()
                        && !self.closing;
                    if !ready {
                        ui.label("Дочекайтеся завершення або призупиніть поточну операцію.");
                    }
                    let windows = ui.add_enabled(
                        ready,
                        egui::Button::new("Explorer — стандартні правила Windows")
                            .min_size(egui::vec2(ui.available_width(), 40.0)),
                    );
                    ui.label("Системний діалог, конфлікти та скасування. Без перевірки VartaCopy.");
                    if windows.clicked() {
                        choice = Some(DispatchEngine::Explorer);
                    }
                    let varta = ui.add_enabled(
                        ready,
                        egui::Button::new("VartaCopy — перевірка та відновлення")
                            .min_size(egui::vec2(ui.available_width(), 40.0)),
                    );
                ui.label("BLAKE3 та журнал. Для цього запиту: наявна назва — помилка; посилання блокуються.");
                    if varta.clicked() {
                        choice = Some(DispatchEngine::VartaCopy);
                    }
                });
            let close = ui.button("Скасувати");
            if std::mem::take(&mut self.chooser_focus) {
                close.request_focus();
            }
            cancel = close.clicked();
        });
        if cancel || response.should_close() {
            self.chooser = None;
            self.restore_focus();
            return;
        }
        if let Some(engine) = choice
            && self.command(Command::Dispatch {
                request,
                engine,
                conflict: Conflict::Error,
            })
        {
            self.chooser = None;
            self.close_form();
            self.banner = None;
            self.history = false;
        }
    }
}
