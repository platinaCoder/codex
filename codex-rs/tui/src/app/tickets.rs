//! Compact ticket navigation above the paired research panes.

use super::*;
use crate::ticket_store::{ActorRole, Document, Status, Summary, TicketStore};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
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
    modal: Option<TicketModalState>,
}

#[derive(Clone)]
struct TicketModalState {
    id: u64,
    revision: u64,
    status: Status,
    role: ActorRole,
    review_required: bool,
}

enum ModalKeyResult {
    Ignored,
    Consumed,
    Compose(String),
}

enum KeyResult {
    Ignored,
    Consumed,
    FocusMain,
    FocusSide,
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
                self.focused = false;
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

    fn focus(&mut self) -> bool {
        if !self.available {
            return false;
        }
        self.focused = true;
        true
    }

    fn key(&mut self, key: KeyEvent) -> KeyResult {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return KeyResult::Ignored;
        }
        if !self.focused {
            return KeyResult::Ignored;
        }
        if crate::key_hint::ctrl(KeyCode::Char('t')).is_press(key) {
            self.focused = false;
            return KeyResult::Ignored;
        }
        if crate::key_hint::ctrl(KeyCode::Char('j')).is_press(key) {
            self.focused = false;
            return KeyResult::Consumed;
        }
        if crate::key_hint::ctrl(KeyCode::Char('h')).is_press(key) {
            self.focused = false;
            return KeyResult::FocusMain;
        }
        if crate::key_hint::ctrl(KeyCode::Char('l')).is_press(key) {
            self.focused = false;
            return KeyResult::FocusSide;
        }
        if crate::key_hint::ctrl(KeyCode::Char('k')).is_press(key) {
            return KeyResult::Consumed;
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

    fn modal_key(&mut self, key: KeyEvent) -> ModalKeyResult {
        if key.kind != KeyEventKind::Press {
            return ModalKeyResult::Ignored;
        }
        let Some(modal) = self.modal.as_ref() else {
            return ModalKeyResult::Ignored;
        };
        if key.code == KeyCode::Esc || matches!(key.code, KeyCode::Char('q' | 'Q')) {
            self.modal = None;
            return ModalKeyResult::Ignored;
        }
        let KeyCode::Char(key) = key.code else {
            return ModalKeyResult::Ignored;
        };
        let Some(update) = modal_action(modal, key.to_ascii_lowercase()) else {
            return if matches!(
                key.to_ascii_lowercase(),
                'a' | 'b' | 'c' | 'p' | 'r' | 's' | 'v'
            ) {
                ModalKeyResult::Consumed
            } else {
                ModalKeyResult::Ignored
            };
        };
        let prompt = format!(
            "Call codex_tui.update_ticket exactly once with ticketId {}, expectedRevision {}, and {update}. Set its required reason to exactly the text after `Reason:`. Do not implement the ticket contents. Reason: ",
            modal.id, modal.revision
        );
        self.modal = None;
        ModalKeyResult::Compose(prompt)
    }

    pub(super) fn dismiss_modal(&mut self) {
        self.modal = None;
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
                Span::raw(format!("{active} active · {review} review · ctrl+k focus")),
            ]))
            .render(area, buffer);
            return;
        }
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(format!(
                " TICKETS · arrows navigate · enter open · a {} · ctrl+h/l panes · ctrl+j/esc return ",
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
    pub(crate) fn ticket_modal_key(&mut self, key: KeyEvent) -> Option<Option<String>> {
        match self.ticket_ui.modal_key(key) {
            ModalKeyResult::Ignored => None,
            ModalKeyResult::Consumed => Some(None),
            ModalKeyResult::Compose(prompt) => Some(Some(prompt)),
        }
    }

    pub(crate) fn dismiss_ticket_modal(&mut self) {
        self.ticket_ui.dismiss_modal();
    }

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
        let paired = self.paired_main_and_side();
        if !self.ticket_ui.available {
            self.ticket_ui.focused = false;
        }
        if !self.ticket_ui.focused {
            if paired.is_none()
                || self.inference_focused()
                || !crate::key_hint::ctrl(KeyCode::Char('k')).is_press(key)
            {
                return false;
            }
            if self.ticket_ui.last_refresh.is_none() {
                self.ticket_ui.refresh(&self.config.workspace_roots, true);
            }
            if !self.ticket_ui.focus() {
                return false;
            }
            tui.frame_requester().schedule_frame();
            return true;
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
            result @ (KeyResult::FocusMain | KeyResult::FocusSide) => {
                let target = paired.map(|(main, side)| match result {
                    KeyResult::FocusMain => main,
                    KeyResult::FocusSide => side,
                    _ => unreachable!(),
                });
                if let Some(target) = target
                    && self.current_displayed_thread_id() != Some(target)
                {
                    self.app_event_tx.send(AppEvent::SelectAgentThread(target));
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
                    Ok(document) => {
                        self.ticket_ui.modal = Some(TicketModalState {
                            id,
                            revision: document.ticket.revision,
                            status: document.ticket.status.clone(),
                            role,
                            review_required: document.ticket.review_required,
                        });
                        modal_lines(&document, role)
                    }
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
    lines.push(Line::from(actions(
        &ticket.status,
        role,
        ticket.review_required,
    )));
    lines.push(Line::from("Actions require an explicit user request and approved ticket tool call; approval never authorizes implementation."));
    lines
}

fn actions(status: &Status, role: ActorRole, review_required: bool) -> String {
    if role == ActorRole::Main {
        return "Read only. MAIN has no ticket mutation or lifecycle controls.".into();
    }
    let mut actions = match status {
        Status::Proposed => vec!["[p] Approve", "[c] Cancel"],
        Status::Approved => vec!["[s] Start implementation", "[c] Cancel"],
        Status::InProgress => vec!["[r] Mark Awaiting Review", "[c] Cancel"],
        Status::AwaitingReview => vec!["[b] Return to In Progress", "[a] Accept", "[c] Cancel"],
        Status::Accepted | Status::Cancelled => Vec::new(),
    };
    if review_required && *status != Status::Cancelled {
        actions.push("[v] Resolve dependency review");
    }
    if actions.is_empty() {
        "No available actions. Terminal tickets require a linked successor.".into()
    } else {
        actions.join(" · ")
    }
}

fn modal_action(modal: &TicketModalState, key: char) -> Option<&'static str> {
    if modal.role != ActorRole::Side {
        return None;
    }
    if key == 'v' && modal.review_required && modal.status != Status::Cancelled {
        return Some("resolveReview.reason set from the text below");
    }
    match (&modal.status, key) {
        (Status::Proposed, 'p') => Some("transition.to = `approved`"),
        (
            Status::Proposed | Status::Approved | Status::InProgress | Status::AwaitingReview,
            'c',
        ) => Some("transition.to = `cancelled`"),
        (Status::Approved, 's') => Some("transition.to = `in_progress`"),
        (Status::InProgress, 'r') => Some("transition.to = `awaiting_review`"),
        (Status::AwaitingReview, 'b') => Some("transition.to = `in_progress`"),
        (Status::AwaitingReview, 'a') => Some("transition.to = `accepted`"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::owned_transcript::tests::attach_thread;
    use crossterm::event::KeyModifiers;

    fn install_lab_store(app: &mut App) -> tempfile::TempDir {
        let lab = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(lab.path().join("runtime")).unwrap();
        std::fs::write(lab.path().join("runtime/codex-upstream.toml"), "url='test'").unwrap();
        std::fs::create_dir(lab.path().join(".git")).unwrap();
        app.config.workspace_roots = vec![
            AbsolutePathBuf::try_from(lab.path().to_path_buf())
                .expect("temporary lab path must be absolute"),
        ];
        app.refresh_ticket_strip_now();
        lab
    }

    fn install_pair(app: &mut App, main: ThreadId, side: ThreadId, active: ThreadId) {
        attach_thread(app, active);
        app.active_thread_id = Some(active);
        app.primary_thread_id = Some(main);
        app.research_workspace_enabled = true;
        app.side_threads.insert(side, SideThreadState::new(main));
    }

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
            KeyResult::FocusMain
        ));
        assert!(!state.focused);
    }
    #[test]
    fn actor_actions_are_bounded() {
        assert!(!actions(&Status::Proposed, ActorRole::Main, false).contains("Approve"));
        assert!(actions(&Status::AwaitingReview, ActorRole::Side, false).contains("Accept"));
        assert!(actions(&Status::Accepted, ActorRole::Side, false).contains("Terminal"));
    }

    #[test]
    fn modal_keys_are_actor_status_and_revision_bounded() {
        let mut state = TicketUiState {
            modal: Some(TicketModalState {
                id: 7,
                revision: 3,
                status: Status::AwaitingReview,
                role: ActorRole::Side,
                review_required: true,
            }),
            ..Default::default()
        };
        assert!(matches!(
            state.modal_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
            ModalKeyResult::Consumed
        ));
        let ModalKeyResult::Compose(prompt) =
            state.modal_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
        else {
            panic!("accept should open a reason draft")
        };
        assert!(prompt.contains("ticketId 7"));
        assert!(prompt.contains("expectedRevision 3"));
        assert!(prompt.contains("accepted"));

        state.modal = Some(TicketModalState {
            id: 7,
            revision: 3,
            status: Status::AwaitingReview,
            role: ActorRole::Main,
            review_required: true,
        });
        assert!(matches!(
            state.modal_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            ModalKeyResult::Consumed
        ));
        assert!(state.modal.is_some());
    }

    #[test]
    fn forced_refresh_observes_an_approved_tool_style_update() {
        let lab = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(lab.path().join("runtime")).unwrap();
        std::fs::write(lab.path().join("runtime/codex-upstream.toml"), "url='test'").unwrap();
        std::fs::create_dir(lab.path().join(".git")).unwrap();
        let root = AbsolutePathBuf::try_from(lab.path().to_path_buf()).unwrap();
        let roots = vec![root];
        let store = TicketStore::from_workspace_roots(&roots).unwrap();
        let created = store
            .create(crate::ticket_store::Create {
                title: "before refresh".into(),
                scope: vec!["runtime".into()],
                source: crate::ticket_store::Source {
                    thread_id: "side".into(),
                    role: ActorRole::Side,
                    turn_id: "turn".into(),
                },
                dependencies: vec![],
                relevant_revisions: vec![],
                proposal: crate::ticket_store::Proposal {
                    original_idea: "refresh test".into(),
                    ..Default::default()
                },
            })
            .unwrap();
        let mut state = TicketUiState::default();
        state.refresh(&roots, true);
        assert_eq!(state.tickets[0].title, "before refresh");

        store
            .update(
                created.ticket.id,
                created.ticket.revision,
                created.ticket.source,
                crate::ticket_store::Update {
                    patch: crate::ticket_store::Patch {
                        title: Some("after refresh".into()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .unwrap();
        state.refresh(&roots, true);
        assert_eq!(state.tickets[0].title, "after refresh");
        assert_eq!(state.tickets[0].revision, 2);
    }

    #[tokio::test]
    async fn full_app_ctrl_t_opens_transcript_without_focusing_tickets() -> Result<()> {
        let mut app = crate::app::test_support::make_test_app().await;
        let main = ThreadId::new();
        let side = ThreadId::new();
        install_pair(&mut app, main, side, main);
        let _lab = install_lab_store(&mut app);
        let mut tui = crate::tui::test_support::make_test_tui()?;
        let mut server = crate::start_embedded_app_server_for_picker(&app.config).await?;

        app.handle_tui_event(
            &mut tui,
            &mut server,
            TuiEvent::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
        )
        .await?;
        assert!(app.ticket_ui.focused);
        app.handle_tui_event(
            &mut tui,
            &mut server,
            TuiEvent::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)),
        )
        .await?;

        assert!(matches!(app.overlay, Some(Overlay::Transcript(_))));
        assert!(!app.ticket_ui.focused);
        server.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn full_app_ticket_focus_enters_and_returns_from_main_and_side() -> Result<()> {
        for enter_from_side in [false, true] {
            let (mut app, mut app_events, _ops) =
                crate::app::tests::make_test_app_with_channels().await;
            let main = ThreadId::new();
            let side = ThreadId::new();
            let active = if enter_from_side { side } else { main };
            install_pair(&mut app, main, side, active);
            let _lab = install_lab_store(&mut app);
            let mut tui = crate::tui::test_support::make_test_tui()?;
            let mut server = crate::start_embedded_app_server_for_picker(&app.config).await?;
            while app_events.try_recv().is_ok() {}

            app.handle_tui_event(
                &mut tui,
                &mut server,
                TuiEvent::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
            )
            .await?;
            assert!(app.ticket_ui.focused);
            assert_eq!(app.current_displayed_thread_id(), Some(active));
            assert!(app_events.try_recv().is_err());

            app.handle_tui_event(
                &mut tui,
                &mut server,
                TuiEvent::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            )
            .await?;
            assert!(!app.ticket_ui.focused);
            assert_eq!(app.current_displayed_thread_id(), Some(active));
            assert!(app_events.try_recv().is_err());

            app.handle_tui_event(
                &mut tui,
                &mut server,
                TuiEvent::Key(KeyEvent::new(
                    KeyCode::Char('k'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                )),
            )
            .await?;
            assert!(!app.ticket_ui.focused, "modifier matching must be exact");

            app.handle_tui_event(
                &mut tui,
                &mut server,
                TuiEvent::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
            )
            .await?;
            let (horizontal_key, expected) = if enter_from_side {
                ('h', main)
            } else {
                ('l', side)
            };
            app.handle_tui_event(
                &mut tui,
                &mut server,
                TuiEvent::Key(KeyEvent::new(
                    KeyCode::Char(horizontal_key),
                    KeyModifiers::CONTROL,
                )),
            )
            .await?;
            assert!(!app.ticket_ui.focused);
            assert!(matches!(
                app_events.try_recv(),
                Ok(AppEvent::SelectAgentThread(thread_id)) if thread_id == expected
            ));
            server.shutdown().await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn unavailable_or_unpaired_ticket_strip_does_not_consume_ctrl_k() -> Result<()> {
        let (mut app, mut app_events, _ops) =
            crate::app::tests::make_test_app_with_channels().await;
        let main = ThreadId::new();
        let side = ThreadId::new();
        install_pair(&mut app, main, side, side);
        let not_a_lab = tempfile::tempdir().unwrap();
        app.config.workspace_roots = vec![
            AbsolutePathBuf::try_from(not_a_lab.path().to_path_buf())
                .expect("temporary non-lab path must be absolute"),
        ];
        let mut tui = crate::tui::test_support::make_test_tui()?;
        let mut server = crate::start_embedded_app_server_for_picker(&app.config).await?;
        while app_events.try_recv().is_ok() {}

        app.handle_tui_event(
            &mut tui,
            &mut server,
            TuiEvent::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
        )
        .await?;
        assert!(!app.ticket_ui.focused);
        assert!(matches!(
            app_events.try_recv(),
            Ok(AppEvent::SelectAgentThread(thread_id)) if thread_id == main
        ));

        app.side_threads.clear();
        let _lab = install_lab_store(&mut app);
        while app_events.try_recv().is_ok() {}
        app.handle_tui_event(
            &mut tui,
            &mut server,
            TuiEvent::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
        )
        .await?;
        assert!(!app.ticket_ui.focused);
        assert!(app_events.try_recv().is_err());
        server.shutdown().await?;
        Ok(())
    }
}
