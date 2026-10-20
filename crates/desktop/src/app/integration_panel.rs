use eframe::egui;
use std::{io, sync::mpsc};
use vartacopy_shell_windows::{
    setup,
    transfer_registration::{self, Installation, Status},
};
#[derive(Default)]
pub(super) struct Panel {
    state: Option<Result<Status, String>>,
    pending: Option<mpsc::Receiver<io::Result<Status>>>,
    task: Option<setup::Action>,
}
impl Panel {
    fn start(&mut self, ctx: &egui::Context, action: Option<setup::Action>) {
        if self.pending.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        match std::thread::Builder::new()
            .name("vartacopy-setup".into())
            .spawn(move || {
                let result = std::env::current_exe()
                    .and_then(|gui| Installation::beside(&gui))
                    .and_then(|installation| match action {
                        Some(action) => setup::request(&installation, action),
                        None => transfer_registration::status(&installation),
                    });
                if let Err(error) = tx.send(result) {
                    eprintln!("VartaCopy integration result undelivered: {error}");
                }
                ctx.request_repaint();
            }) {
            Ok(_) => {
                self.pending = Some(rx);
                self.task = action;
            }
            Err(error) => self.state = Some(Err(error.to_string())),
        }
    }
    pub fn poll(&mut self) -> Option<String> {
        let receiver = self.pending.as_ref()?;
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(io::Error::other("Налаштування завершилося без відповіді."))
            }
        };
        let changed = self.task.take();
        self.pending = None;
        let banner=match &result {
            Ok(Status::Enabled {..}) if changed.is_some()=>Some("Вибір Explorer/VartaCopy увімкнено. Для завантаження нової DLL може знадобитися перезапуск Провідника.".into()),
            Ok(Status::Disabled) if changed.is_some()=>Some("Вибір перед передачею вимкнено. Файли та історія збережені.".into()),
            Err(error)=>Some(format!("Інтеграція Провідника: {error}")),_=>None,
        };
        self.state = Some(result.map_err(|error| error.to_string()));
        banner
    }
    pub fn show(&mut self, ui: &mut egui::Ui, can_change: bool) {
        if self.state.is_none() && self.pending.is_none() {
            self.start(ui.ctx(), None);
        }
        ui.strong("Вибір під час передачі у Провіднику");
        ui.label("Показувати Explorer / VartaCopy / Скасувати при Ctrl+V та перетягуванні.");
        match &self.state {
            Some(Ok(Status::Enabled { .. })) => {
                ui.label("Увімкнено для цієї копії VartaCopy.");
            }
            Some(Ok(Status::Disabled)) => {
                ui.label("Вимкнено. Передачу виконує Windows.");
            }
            Some(Ok(Status::OtherInstallation { dll })) => {
                ui.label("Вибір увімкнений з іншої копії. Можна підключити цю.");
                ui.collapsing("Розташування", |ui| {
                    ui.label(dll.display().to_string());
                });
            }
            Some(Ok(Status::PendingRecovery { .. })) => {
                ui.label("Попереднє налаштування перерване. Його можна відновити.");
            }
            Some(Ok(Status::Incomplete { reason })) => {
                ui.label("Записи змінені або неповні. Автоматична заміна заблокована.");
                ui.collapsing("Подробиці", |ui| {
                    ui.label(reason);
                });
            }
            Some(Err(error)) => {
                ui.label("Не вдалося перевірити інтеграцію.");
                ui.collapsing("Подробиці", |ui| {
                    ui.label(error);
                });
            }
            None => {}
        }
        let busy = self.pending.is_some();
        if busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(if self.task.is_some() {
                    "Очікую підтвердження Windows або завершення налаштування…"
                } else {
                    "Перевіряю стан…"
                });
            });
        }
        let enable = matches!(
            self.state,
            Some(Ok(Status::Disabled
                | Status::OtherInstallation { .. }
                | Status::PendingRecovery { .. }))
        );
        let disable = matches!(
            self.state,
            Some(Ok(Status::Enabled { .. } | Status::PendingRecovery { .. }))
        );
        let label = if matches!(self.state, Some(Ok(Status::PendingRecovery { .. }))) {
            "Відновити інтеграцію"
        } else {
            "Увімкнути вибір"
        };
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!busy && can_change && enable, egui::Button::new(label))
                .clicked()
            {
                self.start(ui.ctx(), Some(setup::Action::Enable));
            }
            if ui
                .add_enabled(
                    !busy && can_change && disable,
                    egui::Button::new("Вимкнути вибір"),
                )
                .clicked()
            {
                self.start(ui.ctx(), Some(setup::Action::Disable));
            }
            if ui
                .add_enabled(!busy, egui::Button::new("Оновити стан"))
                .clicked()
            {
                self.start(ui.ctx(), None);
            }
        });
        ui.small("Увімкнення та вимкнення потребують підтвердження адміністратора Windows.");
    }
    #[cfg(test)]
    pub fn test_state(&mut self, status: Status) {
        self.state = Some(Ok(status));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn panel_exposes_the_available_actions_in_both_themes_without_elevation() {
        use egui::accesskit::Role;
        for dark in [false, true] {
            for enabled in [false, true] {
                let mut panel = Panel::default();
                panel.test_state(if enabled {
                    Status::Enabled {
                        dll: "D:/VartaCopy/vartacopy_transfer.dll".into(),
                    }
                } else {
                    Status::Disabled
                });
                let ctx = egui::Context::default();
                ctx.enable_accesskit();
                ctx.set_visuals(if dark {
                    egui::Visuals::dark()
                } else {
                    egui::Visuals::light()
                });
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(380.0, 400.0),
                        )),
                        ..Default::default()
                    },
                    |ui| panel.show(ui, true),
                );
                let nodes = output
                    .platform_output
                    .accesskit_update
                    .take()
                    .unwrap()
                    .nodes;
                let button = |label: &str| {
                    nodes
                        .iter()
                        .find(|(_, n)| n.role() == Role::Button && n.label() == Some(label))
                        .unwrap()
                };
                assert_eq!(button("Увімкнути вибір").1.is_disabled(), enabled);
                assert_eq!(button("Вимкнути вибір").1.is_disabled(), !enabled);
                assert!(!button("Оновити стан").1.is_disabled());
                assert!(panel.pending.is_none()); // Rendering never launches UAC or changes registration.
                output.drop_without_applying_deltas();
            }
        }
    }
}
