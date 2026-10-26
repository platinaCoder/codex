//! Compact ticket navigation above the paired research panes.

use super::*;
use crate::ticket_store::{ActorRole, Document, Status, Summary, TicketStore};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_millis(750);
const CARD_TITLE_CHARS: usize = 24;

#[derive(Default)]
pub(super) struct TicketUiState {
    available: bool,
    focused: bool,
    archive: bool,
    selected: usize,
    tickets: Vec<Summary>,
    last_refresh: Option<Instant>,
    error: Option<String>,
}

enum KeyResult {
    Ignored,
    Consumed,
    Open(u64),
}

impl TicketUiState {
    fn refresh(&mut self, roots: &[AbsolutePathBuf], force: bool) {
        if !force
            && self
                .last_refresh
                .is_some_and(|last| last.elapsed() < REFRESH_INTERVAL)
        {
            return;
        }
        self.last_refresh = Some(Instant::now());
        let store = match TicketStore::from_workspace_roots(roots) {
            Ok(store) => store,
            Err(_) => {
                self.available = false;
                self.tickets.clear();
                return;
            }
        };
        self.available = true;
        match store.list(self.archive, 100) {
            Ok(tickets) => {
                self.tickets = tickets;
                self.selected = self.selected.min(self.tickets.len().saturating_sub(1));
                self.error = None;
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    fn height(&self) -> u16 {
        if !self.available {
            0
        } else if self.focused {
            3
        } else {
            1
        }
    }

    fn key(&mut self, key: KeyEvent) -> KeyResult {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return KeyResult::Ignored;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('t' | 'T'))
        {
            self.focused = !self.focused;
            return KeyResult::Consumed;
        }
        if !self.focused {
            return KeyResult::Ignored;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('h' | 'j' | 'k' | 'l'))
        {
            self.focused = false;
            return KeyResult::Ignored;
        }
        match key.code {
            KeyCode::Esc => self.focused = false,
            KeyCode::Left | KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Right | KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.tickets.len().saturating_sub(1))
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.tickets.len().saturating_sub(1),
            KeyCode::Char('a' | 'A') => {
                self.archive = !self.archive;
                self.last_refresh = None;
            }
            KeyCode::Enter => {
                return self
                    .tickets
                    .get(self.selected)
                    .map_or(KeyResult::Consumed, |ticket| KeyResult::Open(ticket.id));
            }
            _ => {}
        }
        KeyResult::Consumed
    }

    pub(super) fn render(&self, area: Rect, buffer: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let active = self
            .tickets
            .iter()
            .filter(|ticket| !ticket.status.terminal())
            .count();
        let review = self
            .tickets
            .iter()
            .filter(|ticket| ticket.review_required)
            .count();
        if !self.focused {
            Paragraph::new(Line::from(vec![
                Span::styled(" TICKETS ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(format!("{active} active · {review} review · ctrl+t focus")),
            ]))
            .render(area, buffer);
            return;
        }
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(format!(
                " TICKETS · arrows navigate · enter open · a {} · esc return ",
                if self.archive { "active" } else { "archive" }
            ))
            .render(area, buffer);
        let content = Rect::new(
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        if let Some(error) = &self.error {
            Paragraph::new(format!("ticket store error: {error}")).render(content, buffer);
            return;
        }
        if self.tickets.is_empty() {
            Paragraph::new("No tickets in this view.").render(content, buffer);
            return;
        }
        let mut spans = Vec::new();
        if self.selected > 0 {
            spans.push(Span::raw(format!(" …{} earlier · ", self.selected)));
        }
        spans.extend(
            self.tickets
                .iter()
                .enumerate()
                .skip(self.selected)
                .flat_map(|(index, ticket)| {
                    let mut title = ticket
                        .title
                        .chars()
                        .take(CARD_TITLE_CHARS)
                        .collect::<String>();
                    if ticket.title.chars().count() > CARD_TITLE_CHARS {
                        title.push('…');
                    }
                    let text = format!(
                        " #{} {} {}{} ",
                        ticket.id,
                        ticket.status.label(),
                        title,
                        if ticket.review_required { " !" } else { "" }
                    );
                    let style = if index == self.selected {
                        Style::default().reversed().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    [Span::styled(text, style), Span::raw(" ")]
                }),
        );
        Paragraph::new(Line::from(spans)).render(content, buffer);
    }
}

impl App {
    pub(super) fn refresh_ticket_strip(&mut self) {
        self.ticket_ui.refresh(&self.config.workspace_roots, false);
    }
    pub(super) fn refresh_ticket_strip_now(&mut self) {
        self.ticket_ui.refresh(&self.config.workspace_roots, true);
    }
    pub(super) fn ticket_strip_height(&self) -> u16 {
        self.ticket_ui.height()
    }
    pub(super) fn handle_ticket_focus_key(&mut self, tui: &mut tui::Tui, key: KeyEvent) -> bool {
        if self.overlay.is_some() {
            return false;
        }
        match self.ticket_ui.key(key) {
            KeyResult::Ignored => false,
            KeyResult::Consumed => {
                if self.ticket_ui.last_refresh.is_none() {
                    self.ticket_ui.refresh(&self.config.workspace_roots, true);
                }
                tui.frame_requester().schedule_frame();
                true
            }
            KeyResult::Open(id) => {
                let role = self
                    .paired_main_and_side()
                    .filter(|(_, side)| self.current_displayed_thread_id() == Some(*side))
                    .map_or(ActorRole::Main, |_| ActorRole::Side);
                let document = TicketStore::from_workspace_roots(&self.config.workspace_roots)
                    .and_then(|store| store.read(id));
                let lines = match document {
                    Ok(document) => modal_lines(&document, role),
                    Err(error) => vec![Line::from(format!("Unable to read ticket: {error}"))],
                };
                let _ = tui.enter_alt_screen();
                self.overlay = Some(Overlay::new_static_with_lines(
                    lines,
                    format!("Ticket #{id} · untrusted stored data"),
                    self.keymap.pager.clone(),
                ));
                tui.frame_requester().schedule_frame();
                true
            }
        }
    }
}

fn modal_lines(document: &Document, role: ActorRole) -> Vec<Line<'static>> {
    let ticket = &document.ticket;
    let deps = if ticket.dependencies.is_empty() {
        "none".into()
    } else {
        ticket
            .dependencies
            .iter()
            .map(|id| format!("#{id}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut lines = vec![
        Line::from(crate::ticket_store::UNTRUSTED_NOTICE).bold(),
        Line::default(),
        Line::from(format!("#{} — {}", ticket.id, ticket.title)).bold(),
        Line::from(format!(
            "Status: {} · revision {} · scope: {}",
            ticket.status.label(),
            ticket.revision,
            ticket.scope.join(", ")
        )),
        Line::from(format!(
            "Dependencies: {deps} · review required: {}{}",
            ticket.review_required,
            ticket
                .review_reason
                .as_deref()
                .map_or_else(String::new, |reason| format!(" ({reason})"))
        )),
        Line::default(),
    ];
    for (heading, text) in [
        ("Original idea", &document.proposal.original_idea),
        ("Motivation", &document.proposal.motivation),
        ("Evidence", &document.proposal.evidence),
        ("Reasoning", &document.proposal.reasoning),
        (
            "Proposed implementation/experiment",
            &document.proposal.proposed_implementation_experiment,
        ),
        (
            "Acceptance criteria",
            &document.proposal.acceptance_criteria,
        ),
        ("Uncertainties", &document.proposal.uncertainties),
        ("Findings", &document.proposal.findings),
    ] {
        lines.push(Line::from(heading).bold());
        lines.extend(text.lines().map(|line| Line::from(line.to_string())));
        lines.push(Line::default());
    }
    lines.push(Line::from("Valid actions").bold());
    lines.push(Line::from(actions(&ticket.status, role)));
    lines.push(Line::from("Actions require an explicit user request and approved ticket tool call; approval never authorizes implementation."));
    lines
}

fn actions(status: &Status, role: ActorRole) -> String {
    if role == ActorRole::Side {
        return "Append finding; edit proposal metadata. SIDE cannot transition lifecycle state."
            .into();
    }
    match status {
        Status::Proposed => "Approve; Cancel".into(),
        Status::Approved => "Start implementation; Cancel".into(),
        Status::InProgress => "Mark Awaiting Review; Cancel".into(),
        Status::AwaitingReview => "Return to In Progress; Accept; Cancel".into(),
        Status::Accepted | Status::Cancelled => {
            "No mutations: terminal. Create a linked successor ticket.".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn title_truncation_and_keys() {
        let mut state = TicketUiState {
            available: true,
            focused: true,
            tickets: vec![Summary {
                id: 7,
                revision: 1,
                title: "é".repeat(CARD_TITLE_CHARS + 1),
                status: Status::Proposed,
                scope: vec!["runtime".into()],
                review_required: false,
            }],
            ..Default::default()
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 3));
        state.render(buffer.area, &mut buffer);
        assert!(
            buffer
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
                .contains('…')
        );
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            KeyResult::Consumed
        ));
        assert!(!state.focused);
        state.focused = true;
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL)),
            KeyResult::Ignored
        ));
        assert!(!state.focused);
    }
    #[test]
    fn actor_actions_are_bounded() {
        assert!(!actions(&Status::Proposed, ActorRole::Side).contains("Approve;"));
        assert!(actions(&Status::AwaitingReview, ActorRole::Main).contains("Accept"));
        assert!(actions(&Status::Accepted, ActorRole::Main).contains("terminal"));
    }
}
