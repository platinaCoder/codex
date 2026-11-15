//! Desktop notification coalescing for `ChatWidget`.

use super::*;

impl ChatWidget {
    pub(super) fn notify(&mut self, notification: Notification) {
        if !notification.allowed_for(&self.local_settings.tui.notification_settings.notifications) {
            return;
        }
        match self.pending_notification.take() {
            None => self.pending_notification = Some(notification),
            Some(existing) if existing.priority() > notification.priority() => {
                self.pending_notification = Some(existing);
                self.pending_notifications.push_back(notification);
            }
            Some(existing) => {
                // Preserve the old lower/equal-priority event instead of
                // replacing it. The newest higher-priority event remains the
                // visible head and the queue drains on subsequent draws.
                self.pending_notifications.push_back(existing);
                self.pending_notification = Some(notification);
            }
        }
        self.request_redraw();
    }

    pub(crate) fn maybe_post_pending_notification(&mut self, tui: &mut crate::tui::Tui) {
        if let Some(notif) = self.pending_notification.take() {
            tui.notify(notif.display());
        }
        if self.pending_notification.is_none()
            && let Some(next) = self.pending_notifications.pop_front()
        {
            self.pending_notification = Some(next);
            self.request_redraw();
        }
    }

    pub(crate) fn clear_pending_notifications(&mut self) {
        self.pending_notification = None;
        self.pending_notifications.clear();
    }
}

#[derive(Debug)]
pub(crate) enum Notification {
    AgentTurnComplete { response: String },
    ExecApprovalRequested { command: String },
    EditApprovalRequested { cwd: PathBuf, changes: Vec<PathBuf> },
    ElicitationRequested { server_name: String },
    PlanModePrompt { title: String },
    AsyncQuestion { title: String },
}

impl Notification {
    pub(super) fn display(&self) -> String {
        match self {
            Notification::AgentTurnComplete { response } => {
                Notification::agent_turn_preview(response)
                    .unwrap_or_else(|| "Agent turn complete".to_string())
            }
            Notification::ExecApprovalRequested { command } => {
                format!(
                    "Approval requested: {}",
                    truncate_text(command, /*max_graphemes*/ 30)
                )
            }
            Notification::EditApprovalRequested { cwd, changes } => {
                format!(
                    "Codex wants to edit {}",
                    if changes.len() == 1 {
                        #[allow(clippy::unwrap_used)]
                        display_path_for(changes.first().unwrap(), cwd)
                    } else {
                        format!("{} files", changes.len())
                    }
                )
            }
            Notification::ElicitationRequested { server_name } => {
                format!("Approval requested by {server_name}")
            }
            Notification::PlanModePrompt { title } => {
                format!("Plan mode prompt: {title}")
            }
            Notification::AsyncQuestion { title } => {
                format!("Question: {title}")
            }
        }
    }

    fn type_name(&self) -> &str {
        match self {
            Notification::AgentTurnComplete { .. } => "agent-turn-complete",
            Notification::ExecApprovalRequested { .. }
            | Notification::EditApprovalRequested { .. }
            | Notification::ElicitationRequested { .. } => "approval-requested",
            Notification::PlanModePrompt { .. } => "plan-mode-prompt",
            Notification::AsyncQuestion { .. } => "async-question",
        }
    }

    fn priority(&self) -> u8 {
        match self {
            Notification::AgentTurnComplete { .. } => 0,
            Notification::ExecApprovalRequested { .. }
            | Notification::EditApprovalRequested { .. }
            | Notification::ElicitationRequested { .. }
            | Notification::PlanModePrompt { .. }
            | Notification::AsyncQuestion { .. } => 1,
        }
    }

    pub(super) fn allowed_for(&self, settings: &Notifications) -> bool {
        match settings {
            Notifications::Enabled(enabled) => *enabled,
            Notifications::Custom(allowed) => allowed.iter().any(|a| a == self.type_name()),
        }
    }

    pub(super) fn agent_turn_preview(response: &str) -> Option<String> {
        let mut normalized = String::new();
        for part in response.split_whitespace() {
            if !normalized.is_empty() {
                normalized.push(' ');
            }
            normalized.push_str(part);
        }
        let trimmed = normalized.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(truncate_text(trimmed, AGENT_NOTIFICATION_PREVIEW_GRAPHEMES))
        }
    }

    pub(super) fn user_input_request_summary(
        questions: &[codex_app_server_protocol::ToolRequestUserInputQuestion],
    ) -> Option<String> {
        let first_question = questions.first()?;
        let summary = if first_question.header.trim().is_empty() {
            first_question.question.trim()
        } else {
            first_question.header.trim()
        };
        if summary.is_empty() {
            None
        } else {
            Some(truncate_text(summary, /*max_graphemes*/ 30))
        }
    }
}

const AGENT_NOTIFICATION_PREVIEW_GRAPHEMES: usize = 200;
