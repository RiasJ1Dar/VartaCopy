use super::*;
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Pane {
    #[default]
    Commands,
    Jobs,
    Files,
}
impl App {
    pub(super) fn open_form(&mut self) {
        self.pending_focus = None;
        if !self.new_operation {
            self.return_focus = self.context.memory(|m| m.focused());
        }
        self.new_operation = true;
        self.form_focus = true;
    }
    pub(super) fn close_form(&mut self) {
        self.new_operation = false;
        self.restore_focus();
    }
    pub(super) fn restore_focus(&mut self) {
        self.pending_focus = None;
        if let Some(id) = self.return_focus.take().or(self.command_focus) {
            self.context.memory_mut(|m| m.request_focus(id));
        }
    }
    pub(super) fn files_id(&self) -> egui::Id {
        egui::Id::new(("file_grid", self.selected))
    }
    pub(super) fn jobs_id(&self) -> egui::Id {
        egui::Id::new("job_list")
    }
    pub(super) fn main_keys(&self) -> bool {
        !(self.new_operation
            || self.file_open
            || self.settings_open
            || self.details_open
            || self.messages_open
            || self.picker.is_some())
    }
    pub(super) fn shortcuts(&mut self, ctx: &egui::Context) {
        if !self.new_operation
            && self.picker.is_none()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            if self.file_open {
                self.file_open = false;
                self.restore_focus();
            } else if self.settings_open {
                self.settings_open = false;
                self.restore_focus();
            } else if self.details_open {
                self.details_open = false;
                self.restore_focus();
            } else if self.messages_open {
                self.messages_open = false;
                self.restore_focus();
            } else {
                self.jobs_open = false;
                self.pending_focus = None;
            }
        }
        if !self.main_keys() {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::F6)) {
            self.switch_pane(true);
        } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F6)) {
            self.switch_pane(false);
        }
    }
    fn switch_pane(&mut self, reverse: bool) {
        let focused = self.context.memory(|m| m.focused());
        if focused == Some(self.files_id()) {
            self.pane = Pane::Files;
        } else if focused == Some(self.jobs_id()) {
            self.pane = Pane::Jobs;
        }
        let mut next = match (self.pane, reverse) {
            (Pane::Commands, false) | (Pane::Files, true) => Pane::Jobs,
            (Pane::Jobs, false) | (Pane::Commands, true) => Pane::Files,
            _ => Pane::Commands,
        };
        if next == Pane::Files && self.selected_job().is_none() {
            next = if reverse { Pane::Jobs } else { Pane::Commands };
        }
        self.pane = next;
        let id = match next {
            Pane::Commands => {
                self.jobs_open = false;
                self.command_focus
            }
            Pane::Jobs => {
                self.jobs_open = self.context.content_rect().width() < 680.0;
                self.job_scroll = true;
                Some(self.jobs_id())
            }
            Pane::Files => {
                self.jobs_open = false;
                Some(self.files_id())
            }
        };
        self.request_focus(id);
    }
    pub(super) fn request_focus(&mut self, id: Option<egui::Id>) {
        self.pending_focus = id;
        self.focus_attempts = 0;
    }
    pub(super) fn inspect_file(&mut self) {
        let Some(job) = self.selected_job() else {
            return;
        };
        self.file_details = self
            .state
            .files
            .as_ref()
            .filter(|p| p.job_id == job.id)
            .and_then(|p| {
                p.entries
                    .iter()
                    .find(|e| Some(e.ordinal) == self.file_selected)
            })
            .cloned()
            .map(|e| (job, e));
        self.return_focus = Some(self.files_id());
        if !self.file_open {
            self.file_focus = true;
        }
        self.file_open = true;
    }
}
pub(super) fn lock_arrows(ctx: &egui::Context, id: egui::Id) {
    ctx.memory_mut(|m| {
        m.set_focus_lock_filter(
            id,
            egui::EventFilter {
                horizontal_arrows: true,
                vertical_arrows: true,
                ..Default::default()
            },
        )
    });
}
pub(super) fn navigate(
    ui: &egui::Ui,
    selected: Option<usize>,
    total: usize,
    step: usize,
) -> Option<usize> {
    if total == 0 {
        return None;
    }
    ui.input_mut(|i| {
        let current = selected.unwrap_or(0).min(total - 1);
        if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
            Some(if selected.is_some() {
                (current + 1).min(total - 1)
            } else {
                0
            })
        } else if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
            Some(current.saturating_sub(1))
        } else if i.consume_key(egui::Modifiers::NONE, egui::Key::Home) {
            Some(0)
        } else if i.consume_key(egui::Modifiers::NONE, egui::Key::End) {
            Some(total - 1)
        } else if i.consume_key(egui::Modifiers::NONE, egui::Key::PageDown) {
            Some(current.saturating_add(step.max(1)).min(total - 1))
        } else if i.consume_key(egui::Modifiers::NONE, egui::Key::PageUp) {
            Some(current.saturating_sub(step.max(1)))
        } else {
            None
        }
    })
}
pub(super) fn composite(
    response: &egui::Response,
    role: egui::accesskit::Role,
    label: &str,
    count: usize,
) {
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, label));
    response.ctx.accesskit_node_builder(response.id, |n| {
        n.set_role(role);
        n.clear_active_descendant();
        if role == egui::accesskit::Role::Grid {
            n.set_row_count(count);
            n.set_column_count(3);
        } else {
            n.set_size_of_set(count);
        }
    });
    if response.has_focus() {
        response.ctx.layer_painter(response.layer_id).rect_stroke(
            response.rect.shrink(1.0),
            0,
            response
                .ctx
                .global_style()
                .visuals
                .widgets
                .noninteractive
                .fg_stroke,
            egui::StrokeKind::Inside,
        );
    }
}
