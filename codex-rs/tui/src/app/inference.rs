//! Ephemeral, model-agnostic inference pane for the AI Lab runtime.
//!
//! The TUI owns presentation and sends JSON-lines requests to an adapter selected from the lab's
//! `runtime/inference.toml`. Adapters own model loading and runtime state. Closing the pane aborts
//! the adapter process and intentionally discards the entire conversation.

use super::*;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::BorderType;
use ratatui::widgets::Borders;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::Wrap;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

const INFERENCE_REGISTRY: &str = "runtime/inference.toml";

#[derive(Debug, Deserialize)]
struct InferenceRegistry {
    version: u32,
    default_model: Option<String>,
    adapters: BTreeMap<String, AdapterConfig>,
    #[serde(default)]
    models: BTreeMap<String, ModelConfig>,
}

#[derive(Debug, Deserialize)]
struct AdapterConfig {
    command: Vec<String>,
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct ModelConfig {
    adapter: String,
    model: String,
}

struct ResolvedAdapter {
    target: String,
    label: String,
    command: Vec<String>,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AdapterRequest {
    Message { text: String },
    Reset,
    Close,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AdapterEvent {
    Ready {
        model: Option<String>,
        checkpoint: Option<String>,
        device: Option<String>,
        step: Option<u64>,
    },
    Reply {
        text: String,
        #[serde(default)]
        forced_stop: bool,
    },
    Reset,
    Error {
        message: String,
    },
    Closed,
}

#[derive(Debug)]
pub(crate) enum InferenceRuntimeUpdate {
    Ready { detail: String },
    Reply { text: String, forced_stop: bool },
    Reset,
    Error(String),
    Closed,
}

#[derive(Clone, Copy)]
enum InferRole {
    System,
    User,
    Model,
    Error,
}

struct InferMessage {
    role: InferRole,
    text: String,
}

enum InferStatus {
    Loading,
    Ready(String),
    Generating,
    Failed(String),
}

enum InferInputAction {
    None,
    Submit(String),
    Reset,
    Close,
}

pub(super) struct InferPaneState {
    session_id: Uuid,
    target: String,
    adapter: String,
    focused: bool,
    status: InferStatus,
    messages: Vec<InferMessage>,
    input: String,
    cursor: usize,
    scroll_rows: u16,
    request_tx: mpsc::UnboundedSender<AdapterRequest>,
    runtime_task: JoinHandle<()>,
}

impl Drop for InferPaneState {
    fn drop(&mut self) {
        let _ = self.request_tx.send(AdapterRequest::Close);
        self.runtime_task.abort();
    }
}

#[derive(Clone)]
pub(super) struct InferPaneRenderData {
    target: String,
    adapter: String,
    focused: bool,
    status: String,
    status_is_error: bool,
    messages: Vec<(InferRole, String)>,
    input: String,
    cursor: usize,
    scroll_rows: u16,
}

fn expand(value: &str, lab_root: &Path, model: &str) -> String {
    value
        .replace("{lab_root}", &lab_root.to_string_lossy())
        .replace("{model}", model)
}

fn find_registry(starts: &[&Path]) -> Option<(PathBuf, PathBuf)> {
    starts.iter().find_map(|start| {
        start.ancestors().find_map(|ancestor| {
            let path = ancestor.join(INFERENCE_REGISTRY);
            path.is_file().then(|| (ancestor.to_path_buf(), path))
        })
    })
}

fn resolve_adapter(target: &str, starts: &[&Path]) -> anyhow::Result<ResolvedAdapter> {
    let (lab_root, registry_path) = find_registry(starts).ok_or_else(|| {
        anyhow::anyhow!(
            "could not find {INFERENCE_REGISTRY}; run Codex from the AI Lab or a subdirectory"
        )
    })?;
    let registry_text = std::fs::read_to_string(&registry_path)?;
    let registry: InferenceRegistry = toml::from_str(&registry_text)?;
    if registry.version != 1 {
        anyhow::bail!(
            "unsupported inference registry version {} in {}",
            registry.version,
            registry_path.display()
        );
    }
    let target = if target.trim().is_empty() {
        registry.default_model.as_deref().ok_or_else(|| {
            anyhow::anyhow!("{INFERENCE_REGISTRY} has no default_model; use /infer <model>")
        })?
    } else {
        target.trim()
    };
    let (adapter_name, model) = if let Some(model) = registry.models.get(target) {
        (model.adapter.as_str(), model.model.as_str())
    } else if let Some((adapter, model)) = target.split_once(':') {
        if adapter.is_empty() || model.is_empty() {
            anyhow::bail!("explicit inference targets use adapter:model");
        }
        (adapter, model)
    } else {
        anyhow::bail!(
            "unknown inference model '{target}'; add it to {} or use adapter:model",
            registry_path.display()
        );
    };
    let adapter = registry
        .adapters
        .get(adapter_name)
        .ok_or_else(|| anyhow::anyhow!("inference adapter '{adapter_name}' is not registered"))?;
    if adapter.command.is_empty() {
        anyhow::bail!("inference adapter '{adapter_name}' has an empty command");
    }
    let command = adapter
        .command
        .iter()
        .map(|part| expand(part, &lab_root, model))
        .collect();
    let cwd = adapter
        .cwd
        .as_deref()
        .map(|cwd| PathBuf::from(expand(cwd, &lab_root, model)))
        .unwrap_or_else(|| lab_root.clone());
    let env = adapter
        .env
        .iter()
        .map(|(key, value)| (key.clone(), expand(value, &lab_root, model)))
        .collect();
    Ok(ResolvedAdapter {
        target: target.to_string(),
        label: adapter_name.to_string(),
        command,
        cwd,
        env,
    })
}

fn ready_detail(
    model: Option<String>,
    checkpoint: Option<String>,
    device: Option<String>,
    step: Option<u64>,
) -> String {
    let mut parts = Vec::new();
    if let Some(model) = model.filter(|value| !value.is_empty()) {
        parts.push(model);
    } else if let Some(checkpoint) = checkpoint.filter(|value| !value.is_empty()) {
        parts.push(checkpoint);
    }
    if let Some(step) = step {
        parts.push(format!("step {step}"));
    }
    if let Some(device) = device.filter(|value| !value.is_empty()) {
        parts.push(device);
    }
    if parts.is_empty() {
        "ready".to_string()
    } else {
        parts.join(" · ")
    }
}

fn send_runtime_update(
    app_event_tx: &AppEventSender,
    session_id: Uuid,
    update: InferenceRuntimeUpdate,
) {
    app_event_tx.send(AppEvent::InferenceRuntimeUpdate { session_id, update });
}

async fn run_adapter(
    mut child: tokio::process::Child,
    mut requests: mpsc::UnboundedReceiver<AdapterRequest>,
    app_event_tx: AppEventSender,
    session_id: Uuid,
) {
    let Some(mut stdin) = child.stdin.take() else {
        send_runtime_update(
            &app_event_tx,
            session_id,
            InferenceRuntimeUpdate::Error("adapter stdin was unavailable".to_string()),
        );
        return;
    };
    let Some(stdout) = child.stdout.take() else {
        send_runtime_update(
            &app_event_tx,
            session_id,
            InferenceRuntimeUpdate::Error("adapter stdout was unavailable".to_string()),
        );
        return;
    };
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(target: "ai_lab_inference", "{line}");
            }
        });
    }
    let mut stdout = BufReader::new(stdout).lines();
    loop {
        tokio::select! {
            request = requests.recv() => {
                let Some(request) = request else {
                    break;
                };
                let close = matches!(&request, AdapterRequest::Close);
                let mut line = match serde_json::to_vec(&request) {
                    Ok(line) => line,
                    Err(error) => {
                        send_runtime_update(&app_event_tx, session_id,
                            InferenceRuntimeUpdate::Error(format!("failed to encode adapter request: {error}")));
                        continue;
                    }
                };
                line.push(b'\n');
                if let Err(error) = stdin.write_all(&line).await {
                    send_runtime_update(&app_event_tx, session_id,
                        InferenceRuntimeUpdate::Error(format!("failed to write to inference adapter: {error}")));
                    break;
                }
                if let Err(error) = stdin.flush().await {
                    send_runtime_update(&app_event_tx, session_id,
                        InferenceRuntimeUpdate::Error(format!("failed to flush inference adapter input: {error}")));
                    break;
                }
                if close {
                    break;
                }
            }
            line = stdout.next_line() => {
                match line {
                    Ok(Some(line)) => match serde_json::from_str::<AdapterEvent>(&line) {
                        Ok(AdapterEvent::Ready { model, checkpoint, device, step }) => {
                            send_runtime_update(&app_event_tx, session_id,
                                InferenceRuntimeUpdate::Ready {
                                    detail: ready_detail(model, checkpoint, device, step),
                                });
                        }
                        Ok(AdapterEvent::Reply { text, forced_stop }) => {
                            send_runtime_update(&app_event_tx, session_id,
                                InferenceRuntimeUpdate::Reply { text, forced_stop });
                        }
                        Ok(AdapterEvent::Reset) => {
                            send_runtime_update(&app_event_tx, session_id, InferenceRuntimeUpdate::Reset);
                        }
                        Ok(AdapterEvent::Error { message }) => {
                            send_runtime_update(&app_event_tx, session_id,
                                InferenceRuntimeUpdate::Error(message));
                        }
                        Ok(AdapterEvent::Closed) => {
                            send_runtime_update(&app_event_tx, session_id, InferenceRuntimeUpdate::Closed);
                            break;
                        }
                        Err(error) => {
                            send_runtime_update(&app_event_tx, session_id,
                                InferenceRuntimeUpdate::Error(format!(
                                    "invalid adapter event: {error}; line={line}"
                                )));
                        }
                    },
                    Ok(None) => {
                        let status = child.wait().await;
                        send_runtime_update(&app_event_tx, session_id,
                            InferenceRuntimeUpdate::Error(match status {
                                Ok(status) => format!("inference adapter exited with {status}"),
                                Err(error) => format!("inference adapter disappeared: {error}"),
                            }));
                        return;
                    }
                    Err(error) => {
                        send_runtime_update(&app_event_tx, session_id,
                            InferenceRuntimeUpdate::Error(format!("failed to read inference adapter: {error}")));
                        break;
                    }
                }
            }
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

impl InferPaneState {
    fn start(
        target: String,
        starts: &[&Path],
        app_event_tx: AppEventSender,
    ) -> anyhow::Result<Self> {
        let resolved = resolve_adapter(&target, starts)?;
        let executable = resolved
            .command
            .first()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("inference adapter command was empty"))?;
        let mut command = tokio::process::Command::new(executable);
        command
            .args(&resolved.command[1..])
            .current_dir(&resolved.cwd)
            .envs(&resolved.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = command.spawn().map_err(|error| {
            anyhow::anyhow!(
                "failed to start inference adapter '{}': {error}",
                resolved.label
            )
        })?;
        let session_id = Uuid::new_v4();
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let runtime_task = tokio::spawn(run_adapter(child, request_rx, app_event_tx, session_id));
        Ok(Self {
            session_id,
            target: resolved.target,
            adapter: resolved.label,
            focused: true,
            status: InferStatus::Loading,
            messages: vec![InferMessage {
                role: InferRole::System,
                text: "Loading model adapter…".to_string(),
            }],
            input: String::new(),
            cursor: 0,
            scroll_rows: 0,
            request_tx,
            runtime_task,
        })
    }

    fn render_data(&self) -> InferPaneRenderData {
        let (status, status_is_error) = match &self.status {
            InferStatus::Loading => ("loading model…".to_string(), false),
            InferStatus::Ready(detail) => (detail.clone(), false),
            InferStatus::Generating => ("generating…".to_string(), false),
            InferStatus::Failed(error) => (error.clone(), true),
        };
        InferPaneRenderData {
            target: self.target.clone(),
            adapter: self.adapter.clone(),
            focused: self.focused,
            status,
            status_is_error,
            messages: self
                .messages
                .iter()
                .map(|message| (message.role, message.text.clone()))
                .collect(),
            input: self.input.clone(),
            cursor: self.cursor,
            scroll_rows: self.scroll_rows,
        }
    }

    fn apply_update(&mut self, update: InferenceRuntimeUpdate) {
        match update {
            InferenceRuntimeUpdate::Ready { detail } => {
                self.status = InferStatus::Ready(detail.clone());
                self.messages.clear();
                self.messages.push(InferMessage {
                    role: InferRole::System,
                    text: format!("Model ready: {detail}"),
                });
            }
            InferenceRuntimeUpdate::Reply {
                mut text,
                forced_stop,
            } => {
                if forced_stop {
                    text.push_str("\n[forced stop at token limit]");
                }
                self.messages.push(InferMessage {
                    role: InferRole::Model,
                    text,
                });
                self.status = InferStatus::Ready("ready".to_string());
            }
            InferenceRuntimeUpdate::Reset => {
                self.messages.clear();
                self.messages.push(InferMessage {
                    role: InferRole::System,
                    text: "Conversation state reset.".to_string(),
                });
                self.status = InferStatus::Ready("ready".to_string());
            }
            InferenceRuntimeUpdate::Error(error) => {
                self.messages.push(InferMessage {
                    role: InferRole::Error,
                    text: error.clone(),
                });
                self.status = InferStatus::Failed(error);
            }
            InferenceRuntimeUpdate::Closed => {
                self.status = InferStatus::Failed("adapter closed".to_string());
            }
        }
        self.scroll_rows = 0;
    }

    fn insert_text(&mut self, text: &str) {
        self.input.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    fn previous_boundary(&self) -> usize {
        self.input[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0)
    }

    fn next_boundary(&self) -> usize {
        self.input[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(index, _)| self.cursor + index)
            .unwrap_or(self.input.len())
    }

    fn handle_key(&mut self, key: KeyEvent) -> InferInputAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return InferInputAction::None;
        }
        match key {
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                ..
            } => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return InferInputAction::None;
                }
                if text == "/close" {
                    return InferInputAction::Close;
                }
                if text == "/reset" {
                    self.input.clear();
                    self.cursor = 0;
                    return InferInputAction::Reset;
                }
                if !matches!(self.status, InferStatus::Ready(_)) {
                    return InferInputAction::None;
                }
                self.input.clear();
                self.cursor = 0;
                InferInputAction::Submit(text)
            }
            KeyEvent {
                code: KeyCode::Char(character),
                modifiers,
                ..
            } if !modifiers.intersects(
                KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::SUPER
                    | KeyModifiers::HYPER
                    | KeyModifiers::META,
            ) =>
            {
                self.insert_text(&character.to_string());
                InferInputAction::None
            }
            KeyEvent {
                code: KeyCode::Backspace,
                ..
            } => {
                if self.cursor > 0 {
                    let previous = self.previous_boundary();
                    self.input.drain(previous..self.cursor);
                    self.cursor = previous;
                }
                InferInputAction::None
            }
            KeyEvent {
                code: KeyCode::Delete,
                ..
            } => {
                if self.cursor < self.input.len() {
                    let next = self.next_boundary();
                    self.input.drain(self.cursor..next);
                }
                InferInputAction::None
            }
            KeyEvent {
                code: KeyCode::Left,
                ..
            } => {
                self.cursor = self.previous_boundary();
                InferInputAction::None
            }
            KeyEvent {
                code: KeyCode::Right,
                ..
            } => {
                self.cursor = self.next_boundary();
                InferInputAction::None
            }
            KeyEvent {
                code: KeyCode::Home,
                ..
            } => {
                self.cursor = 0;
                InferInputAction::None
            }
            KeyEvent {
                code: KeyCode::End, ..
            } => {
                self.cursor = self.input.len();
                InferInputAction::None
            }
            KeyEvent {
                code: KeyCode::PageUp | KeyCode::Up,
                ..
            } => {
                self.scroll_rows = self.scroll_rows.saturating_add(3);
                InferInputAction::None
            }
            KeyEvent {
                code: KeyCode::PageDown | KeyCode::Down,
                ..
            } => {
                self.scroll_rows = self.scroll_rows.saturating_sub(3);
                InferInputAction::None
            }
            _ => InferInputAction::None,
        }
    }

    fn submit(&mut self, text: String) {
        self.messages.push(InferMessage {
            role: InferRole::User,
            text: text.clone(),
        });
        if self
            .request_tx
            .send(AdapterRequest::Message { text })
            .is_err()
        {
            self.apply_update(InferenceRuntimeUpdate::Error(
                "inference adapter is no longer running".to_string(),
            ));
            return;
        }
        self.status = InferStatus::Generating;
        self.scroll_rows = 0;
    }

    fn reset(&mut self) {
        if self.request_tx.send(AdapterRequest::Reset).is_ok() {
            self.status = InferStatus::Generating;
        }
    }
}

impl App {
    pub(super) fn start_inference(&mut self, target: String) -> anyhow::Result<()> {
        let target = target.trim().to_string();
        let state = InferPaneState::start(
            target,
            &[self.config.cwd.as_path(), self.launch_cwd.as_path()],
            self.app_event_tx.clone(),
        )?;
        self.infer_pane = Some(state);
        Ok(())
    }

    pub(super) fn close_inference(&mut self) -> bool {
        self.infer_pane.take().is_some()
    }

    pub(super) fn inference_focused(&self) -> bool {
        self.infer_pane.as_ref().is_some_and(|pane| pane.focused)
    }

    pub(super) fn set_inference_focus(&mut self, focused: bool) {
        if let Some(pane) = self.infer_pane.as_mut() {
            pane.focused = focused;
        }
    }

    pub(super) fn handle_inference_focus_key(&mut self, key: KeyEvent) -> bool {
        if self.infer_pane.is_none() {
            return false;
        }
        if crate::key_hint::ctrl(KeyCode::Char('j')).is_press(key) {
            self.set_inference_focus(true);
            return true;
        }
        let target = self.paired_main_and_side().and_then(|(main, side)| {
            if crate::key_hint::ctrl(KeyCode::Char('h')).is_press(key)
                || crate::key_hint::ctrl(KeyCode::Char('k')).is_press(key)
            {
                Some(main)
            } else if crate::key_hint::ctrl(KeyCode::Char('l')).is_press(key) {
                Some(side)
            } else {
                None
            }
        });
        if let Some(target) = target {
            self.set_inference_focus(false);
            if self.current_displayed_thread_id() != Some(target) {
                self.app_event_tx.send(AppEvent::SelectAgentThread(target));
            }
            return true;
        }
        false
    }

    pub(super) fn inference_render_data(&self) -> Option<InferPaneRenderData> {
        self.infer_pane.as_ref().map(InferPaneState::render_data)
    }

    pub(super) fn handle_inference_key(&mut self, key: KeyEvent) {
        let action = self
            .infer_pane
            .as_mut()
            .map(|pane| pane.handle_key(key))
            .unwrap_or(InferInputAction::None);
        match action {
            InferInputAction::None => {}
            InferInputAction::Submit(text) => {
                if let Some(pane) = self.infer_pane.as_mut() {
                    pane.submit(text);
                }
            }
            InferInputAction::Reset => {
                if let Some(pane) = self.infer_pane.as_mut() {
                    pane.reset();
                }
            }
            InferInputAction::Close => {
                self.close_inference();
            }
        }
    }

    pub(super) fn handle_inference_paste(&mut self, pasted: &str) {
        if let Some(pane) = self.infer_pane.as_mut() {
            pane.insert_text(&pasted.replace("\r\n", "\n").replace('\r', "\n"));
        }
    }

    pub(super) fn apply_inference_update(
        &mut self,
        session_id: Uuid,
        update: InferenceRuntimeUpdate,
    ) {
        if let Some(pane) = self
            .infer_pane
            .as_mut()
            .filter(|pane| pane.session_id == session_id)
        {
            pane.apply_update(update);
        }
    }

    #[cfg(test)]
    pub(super) fn install_test_inference(&mut self, target: &str) {
        let (request_tx, _request_rx) = mpsc::unbounded_channel();
        self.infer_pane = Some(InferPaneState {
            session_id: Uuid::new_v4(),
            target: target.to_string(),
            adapter: "test-adapter".to_string(),
            focused: true,
            status: InferStatus::Ready("ready · test".to_string()),
            messages: vec![InferMessage {
                role: InferRole::Model,
                text: "INFER_VISIBLE_MARKER".to_string(),
            }],
            input: String::new(),
            cursor: 0,
            scroll_rows: 0,
            request_tx,
            runtime_task: tokio::spawn(std::future::pending()),
        });
    }
}

fn role_style(role: InferRole) -> (&'static str, Style) {
    match role {
        InferRole::System => ("system", Style::default().dim()),
        InferRole::User => ("you", Style::default().fg(Color::Cyan).bold()),
        InferRole::Model => ("model", Style::default().fg(Color::Green).bold()),
        InferRole::Error => ("error", Style::default().fg(Color::Red).bold()),
    }
}

fn visible_input(input: &str, cursor: usize, width: usize) -> (String, usize) {
    if width == 0 {
        return (String::new(), 0);
    }
    let before = &input[..cursor];
    let before_chars: Vec<char> = before.chars().collect();
    let keep_before = before_chars.len().min(width.saturating_sub(1));
    let start = before_chars.len().saturating_sub(keep_before);
    let mut visible: String = before_chars[start..].iter().collect();
    let cursor_column = visible.chars().count();
    for character in input[cursor..]
        .chars()
        .take(width.saturating_sub(cursor_column))
    {
        visible.push(character);
    }
    (visible, cursor_column)
}

pub(super) fn render_inference_pane(
    data: &InferPaneRenderData,
    area: Rect,
    buffer: &mut Buffer,
) -> Option<(u16, u16)> {
    Clear.render(area, buffer);
    let focus = if data.focused { " • focused" } else { "" };
    let title = format!(
        " INFER{focus} · {} · {} · /close ",
        data.target, data.adapter
    );
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if data.focused {
            Style::default()
        } else {
            Style::default().dim()
        })
        .title(if data.focused {
            Line::from(title).bold()
        } else {
            Line::from(title).dim()
        })
        .render(area, buffer);
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    if inner.height == 0 || inner.width == 0 {
        return None;
    }
    let composer_height = inner.height.min(2);
    let transcript_height = inner.height.saturating_sub(composer_height);
    let transcript_area = Rect::new(inner.x, inner.y, inner.width, transcript_height);
    let mut lines = Vec::new();
    for (role, text) in &data.messages {
        let (label, style) = role_style(*role);
        let mut fragments = text.lines();
        let first = fragments.next().unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(format!("{label}> "), style),
            Span::raw(first.to_string()),
        ]));
        for fragment in fragments {
            lines.push(Line::from(format!("       {fragment}")));
        }
        lines.push(Line::default());
    }
    let transcript = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total_rows =
        u16::try_from(transcript.line_count(transcript_area.width)).unwrap_or(u16::MAX);
    let bottom_scroll = total_rows.saturating_sub(transcript_area.height);
    let scroll = bottom_scroll.saturating_sub(data.scroll_rows.min(bottom_scroll));
    transcript
        .scroll((scroll, 0))
        .render(transcript_area, buffer);

    let input_area = Rect::new(
        inner.x,
        inner.y.saturating_add(transcript_height),
        inner.width,
        composer_height.min(1),
    );
    if input_area.height > 0 {
        let input_width = usize::from(input_area.width.saturating_sub(2));
        let (visible, cursor_column) = visible_input(&data.input, data.cursor, input_width);
        let display = if visible.is_empty() && !data.focused {
            "› Ask the model…".to_string()
        } else {
            format!("› {visible}")
        };
        Paragraph::new(display).render(input_area, buffer);
        if data.focused {
            let x = input_area
                .x
                .saturating_add(2)
                .saturating_add(u16::try_from(cursor_column).unwrap_or(u16::MAX))
                .min(input_area.right().saturating_sub(1));
            let y = input_area.y;
            if composer_height > 1 {
                let status_area = Rect::new(
                    inner.x.saturating_add(2),
                    inner.y.saturating_add(transcript_height + 1),
                    inner.width.saturating_sub(2),
                    1,
                );
                let status = Line::from(format!(
                    "{} · enter send · ↑/↓ scroll · ctrl+h/k main · ctrl+l side",
                    data.status
                ));
                Paragraph::new(if data.status_is_error {
                    status.red()
                } else {
                    status.dim()
                })
                .render(status_area, buffer);
            }
            return Some((x, y));
        }
    }
    if composer_height > 1 {
        let status_area = Rect::new(
            inner.x.saturating_add(2),
            inner.y.saturating_add(transcript_height + 1),
            inner.width.saturating_sub(2),
            1,
        );
        Paragraph::new(Line::from(data.status.clone()).dim()).render(status_area, buffer);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_resolves_alias_and_explicit_adapter_targets() {
        let root = tempfile::tempdir().expect("lab root");
        let runtime = root.path().join("runtime");
        std::fs::create_dir_all(&runtime).expect("runtime directory");
        std::fs::write(
            runtime.join("inference.toml"),
            r#"
version = 1
default_model = "small"
[adapters.demo]
command = ["{lab_root}/python", "adapter.py", "{model}"]
cwd = "{lab_root}/models/demo"
[models.small]
adapter = "demo"
model = "checkpoint-small"
"#,
        )
        .expect("registry");

        let alias = resolve_adapter("small", &[root.path()]).expect("alias");
        assert_eq!(alias.label, "demo");
        assert_eq!(alias.command[2], "checkpoint-small");
        assert_eq!(alias.cwd, root.path().join("models/demo"));

        let default = resolve_adapter("", &[root.path()]).expect("default alias");
        assert_eq!(default.target, "small");
        assert_eq!(default.command[2], "checkpoint-small");

        let explicit = resolve_adapter("demo:other", &[root.path()]).expect("explicit target");
        assert_eq!(explicit.command[2], "other");
    }

    #[tokio::test]
    async fn input_close_is_local_and_unicode_editing_keeps_boundaries() {
        let (request_tx, _request_rx) = mpsc::unbounded_channel();
        let mut pane = InferPaneState {
            session_id: Uuid::new_v4(),
            target: "test".to_string(),
            adapter: "test".to_string(),
            focused: true,
            status: InferStatus::Ready("ready".to_string()),
            messages: Vec::new(),
            input: "éx".to_string(),
            cursor: "éx".len(),
            scroll_rows: 0,
            request_tx,
            runtime_task: tokio::spawn(async {}),
        };
        pane.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(pane.input, "é");
        pane.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(pane.input, "");
        pane.input = "/close".to_string();
        pane.cursor = pane.input.len();
        assert!(matches!(
            pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InferInputAction::Close
        ));
    }
}
