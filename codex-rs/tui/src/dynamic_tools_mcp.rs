//! Approval-gated loopback MCP transport for task tools owned by a local TUI.

use crate::app_event_sender::AppEventSender;
use crate::dynamic_tools;
use crate::ticket_workflow::CallerKey;
use crate::ticket_workflow::TicketWorkflowCoordinator;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware;
use axum::middleware::Next;
use axum::response::Response;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallParams;
use codex_app_server_protocol::DynamicToolNamespaceTool;
use codex_app_server_protocol::DynamicToolSpec;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStatusChangedNotification;
use codex_config::McpServerConfig;
use codex_config::McpServerRequirement;
use codex_config::RawMcpServerConfig;
use codex_features::Features;
use rmcp::ErrorData as McpError;
use rmcp::handler::server::ServerHandler;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResult;
use rmcp::model::ContentBlock;
use rmcp::model::JsonObject;
use rmcp::model::ListToolsResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerConfig;
use rmcp::model::Tool;
use rmcp::model::ToolAnnotations;
use rmcp::service::RequestContext;
use rmcp::service::RoleServer;
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use serde_json::Value;
use serde_json::json;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use uuid::Uuid;

#[derive(Clone)]
pub(crate) enum ThreadToolTransport {
    Disabled,
    Dynamic,
    Mcp(Arc<DynamicToolMcpServer>),
}

impl ThreadToolTransport {
    pub(crate) fn validate_config(
        &self,
        config: &crate::legacy_core::config::Config,
    ) -> std::io::Result<()> {
        if let Self::Mcp(server) = self
            && config.mcp_servers.get().contains_key(server.namespace)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "A configured MCP server owns the TUI tools namespace; keep the current task and rename that server before changing projects",
            ));
        }
        Ok(())
    }

    pub(crate) fn configure(&self, params: &mut ThreadStartParams) {
        match self {
            Self::Disabled => params.dynamic_tools = None,
            Self::Dynamic => {
                params.dynamic_tools = Some(dynamic_tools::non_delegation_tool_specs());
            }
            Self::Mcp(server) => {
                params.dynamic_tools =
                    (!server.task_tools).then(dynamic_tools::non_delegation_tool_specs);
                self.configure_mcp(&mut params.config);
            }
        }
    }

    pub(crate) fn configure_paired_side(&self, params: &mut ThreadStartParams) {
        match self {
            // Embedded dynamic-tool requests are not owned by an external TUI
            // task and are rejected by the event dispatcher. Never advertise
            // a fallback surface that cannot execute.
            Self::Disabled => params.dynamic_tools = None,
            // External dynamic tools can retain SIDE's bounded read surface,
            // but assignment and ticket mutations remain MCP-only because
            // direct calls have no per-call approval layer.
            Self::Dynamic => {
                params.dynamic_tools = Some(dynamic_tools::side_fallback_tool_specs());
            }
            Self::Mcp(_) => {
                // Once an MCP transport is selected, direct dynamic requests
                // are rejected. A worktree-only MCP therefore cannot provide
                // SIDE's model tools and must not advertise dead fallbacks.
                params.dynamic_tools = None;
                self.configure_mcp(&mut params.config);
            }
        }
    }

    pub(crate) fn provides_paired_side_tools(&self) -> bool {
        matches!(self, Self::Dynamic) || matches!(self, Self::Mcp(server) if server.task_tools)
    }

    pub(crate) fn provides_task_tools(&self) -> bool {
        matches!(self, Self::Dynamic) || matches!(self, Self::Mcp(server) if server.task_tools)
    }

    pub(crate) fn configure_mcp(&self, config: &mut Option<HashMap<String, Value>>) {
        if let Self::Mcp(server) = self {
            config.get_or_insert_default().insert(
                format!("mcp_servers.{}", server.namespace),
                server.config.clone(),
            );
        }
    }

    pub(crate) fn configure_resume(
        &self,
        params: &mut codex_app_server_protocol::ThreadResumeParams,
    ) {
        self.configure_mcp(&mut params.config);
        if matches!(self, Self::Mcp(_)) {
            // A cold-resumed legacy SIDE can carry its original direct-tool catalog in session
            // metadata. The authenticated MCP transport replaces that catalog rather than being
            // merged with obsolete capabilities such as `send_message_to_main`.
            params.dynamic_tools = Some(Vec::new());
        }
    }
}

#[derive(Clone)]
pub(crate) struct ToolServices {
    pub(crate) task_tools: bool,
    pub(crate) worktrees: Option<crate::managed_worktree_tools::ManagedWorktreeTools>,
}

impl ToolServices {
    pub(crate) fn namespace(&self) -> &'static str {
        if self.task_tools {
            dynamic_tools::NAMESPACE
        } else {
            "codex_worktrees"
        }
    }
}

pub(crate) struct DynamicToolMcpServer {
    workflow: Arc<TicketWorkflowCoordinator>,
    config: Value,
    namespace: &'static str,
    task_tools: bool,
    thread_start_params: ThreadStartParams,
    features: Features,
    task: JoinHandle<()>,
}

impl DynamicToolMcpServer {
    pub(crate) fn suspend(&self) {
        self.workflow.suspend();
    }

    pub(crate) fn reconnect(&self, handle: AppServerRequestHandle, events: AppEventSender) {
        self.workflow.reconnect(handle, events);
        self.retry_pending_ticket_notifications();
    }

    pub(crate) fn retry_pending_ticket_notifications(&self) {
        launch_ticket_retry(
            &self.workflow,
            self.task_tools,
            &self.thread_start_params,
            self.namespace,
            &self.config,
            &self.features,
        );
    }

    pub(crate) async fn start(
        request_handle: AppServerRequestHandle,
        mut thread_start_params: ThreadStartParams,
        features: Features,
        app_event_tx: AppEventSender,
        status_updates: broadcast::Sender<ThreadStatusChangedNotification>,
        managed_requirement: Option<&McpServerRequirement>,
        services: ToolServices,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let authorization = Arc::new(format!("Bearer {}", Uuid::new_v4()));
        let server_config = json!({
            "url": format!("http://{address}/mcp"),
            "http_headers": {"Authorization": authorization.as_str()},
            "default_tools_approval_mode": "approve",
            "tools": {
                "create_thread": {"approval_mode": "prompt"},
                "send_message_to_thread": {"approval_mode": "prompt"},
                "create_ticket": {"approval_mode": "prompt"},
                "update_ticket": {"approval_mode": "prompt"},
                "implement_ticket": {"approval_mode": "prompt"},
                "resume_ticket_review": {"approval_mode": "prompt"},
                "reassign_ticket": {"approval_mode": "prompt"},
                "retry_ticket_notification": {"approval_mode": "prompt"},
                "append_ticket_comment": {"approval_mode": "prompt"},
                "fork_thread": {"approval_mode": "prompt"},
                "create_worktree": {"approval_mode": "approve"}
            }
        });
        if let Some(requirement) = managed_requirement {
            let raw_config = serde_json::from_value::<RawMcpServerConfig>(server_config.clone())
                .map_err(|error| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
                })?;
            let configured_server = McpServerConfig::try_from(raw_config)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            if !configured_server.matches_requirement(requirement) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "managed MCP requirements do not permit the TUI task-tools server",
                ));
            }
        }
        if let Some(overrides) = thread_start_params.config.as_mut() {
            overrides.remove("web_search");
        }
        let task_tools = services.task_tools;
        let workflow = TicketWorkflowCoordinator::new(request_handle, app_event_tx);
        let namespace = services.namespace();
        let handler = DynamicToolMcpHandler {
            workflow: Arc::clone(&workflow),
            thread_start_params: thread_start_params.clone(),
            features: features.clone(),
            status_updates: status_updates.clone(),
            server_config: server_config.clone(),
            services,
        };
        let service = StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );
        let router =
            Router::new()
                .nest_service("/mcp", service)
                .layer(middleware::from_fn_with_state(
                    authorization,
                    require_authorization,
                ));
        let retry_workflow = Arc::clone(&workflow);
        let retry_params = thread_start_params.clone();
        let retry_config = server_config.clone();
        let retry_features = features.clone();
        let mut statuses = status_updates.subscribe();
        let task = tokio::spawn(async move {
            use std::future::IntoFuture;
            let serving = axum::serve(listener, router).into_future();
            tokio::pin!(serving);
            let mut timer = tokio::time::interval(std::time::Duration::from_secs(5));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    result = &mut serving => {
                        if let Err(error) = result { tracing::warn!(%error, "TUI task-tools MCP server stopped"); }
                        break;
                    }
                    _ = timer.tick() => {}
                    status = statuses.recv() => {
                        match status {
                            Ok(status) if matches!(status.status, codex_app_server_protocol::ThreadStatus::Idle) => {}
                            Ok(_) => continue,
                            Err(broadcast::error::RecvError::Lagged(_)) => {}
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
                launch_ticket_retry(
                    &retry_workflow,
                    task_tools,
                    &retry_params,
                    namespace,
                    &retry_config,
                    &retry_features,
                );
            }
        });
        let server = Self {
            workflow,
            config: server_config,
            namespace,
            task_tools,
            thread_start_params,
            features,
            task,
        };
        server.retry_pending_ticket_notifications();
        Ok(server)
    }
}

fn launch_ticket_retry(
    workflow: &Arc<TicketWorkflowCoordinator>,
    task_tools: bool,
    thread_start_params: &ThreadStartParams,
    namespace: &str,
    server_config: &Value,
    features: &Features,
) {
    if !task_tools {
        return;
    }
    if workflow.connection().is_err() {
        return;
    }
    let Some(mut retry_permit) = workflow.claim_retry() else {
        return;
    };
    let mut thread_start_params = thread_start_params.clone();
    thread_start_params
        .config
        .get_or_insert_default()
        .insert(format!("mcp_servers.{}", namespace), server_config.clone());
    let features = features.clone();
    let workflow = Arc::clone(workflow);
    tokio::spawn(async move {
        loop {
            let Ok((handle, app_event_tx)) = workflow.connection() else {
                break;
            };
            if let Err(error) = dynamic_tools::retry_pending_ticket_notifications(
                handle,
                thread_start_params.clone(),
                features.clone(),
                app_event_tx,
            )
            .await
            {
                tracing::warn!(%error, "pending ticket notification retry pump failed");
            }
            if !retry_permit.finish_pass() {
                break;
            }
        }
    });
}

impl Drop for DynamicToolMcpServer {
    fn drop(&mut self) {
        self.workflow.suspend();
        self.task.abort();
    }
}

async fn require_authorization(
    State(expected): State<Arc<String>>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if request
        .headers()
        .get(AUTHORIZATION)
        .is_some_and(|value| value.as_bytes() == expected.as_bytes())
    {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

#[derive(Clone)]
struct DynamicToolMcpHandler {
    workflow: Arc<TicketWorkflowCoordinator>,
    thread_start_params: ThreadStartParams,
    features: Features,
    status_updates: broadcast::Sender<ThreadStatusChangedNotification>,
    server_config: Value,
    services: ToolServices,
}

impl ServerHandler for DynamicToolMcpHandler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut tools = Vec::new();
        let mut specs = if self.services.task_tools {
            dynamic_tools::tool_specs()
        } else {
            Vec::new()
        };
        if self.services.worktrees.is_some() {
            specs.extend(crate::managed_worktree_tool_specs::specs());
        }
        for spec in specs {
            let functions = match spec {
                DynamicToolSpec::Function(function) => vec![function],
                DynamicToolSpec::Namespace(namespace) => namespace
                    .tools
                    .into_iter()
                    .map(|tool| match tool {
                        DynamicToolNamespaceTool::Function(function) => function,
                    })
                    .collect(),
            };
            for function in functions {
                let schema = serde_json::from_value::<JsonObject>(function.input_schema)
                    .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                let mut tool = Tool::new(
                    Cow::Owned(function.name),
                    Cow::Owned(function.description),
                    Arc::new(schema),
                );
                tool.annotations = Some(ToolAnnotations::new().read_only(matches!(
                    tool.name.as_ref(),
                    "list_threads"
                        | "list_archived_threads"
                        | "read_thread"
                        | "wait_threads"
                        | "read_workspace_file"
                        | "list_workspace_directory"
                        | "search_workspace_text"
                        | "list_tickets"
                        | "read_ticket"
                        // Strict SIDE policy also admits bounded ticket mutations, but those
                        // retain read_only=false and always prompt.
                        | "list_worktrees"
                        | "get_worktree_creation_status"
                )));
                tools.push(tool);
            }
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, McpError> {
        let metadata = &context.meta.0.0;
        let turn_metadata = metadata
            .get("x-codex-turn-metadata")
            .and_then(|value| match value {
                Value::Object(_) => Some(value.clone()),
                Value::String(value) => serde_json::from_str(value).ok(),
                _ => None,
            });
        if let Some(turn_metadata) = turn_metadata.as_ref().and_then(Value::as_object) {
            for (outer_key, nested_key) in [("threadId", "thread_id"), ("turnId", "turn_id")] {
                if let (Some(outer), Some(nested)) = (
                    metadata.get(outer_key).and_then(Value::as_str),
                    turn_metadata.get(nested_key).and_then(Value::as_str),
                ) && outer != nested
                {
                    return Err(McpError::invalid_params(
                        format!("conflicting {outer_key} task metadata"),
                        None,
                    ));
                }
            }
        }
        let thread_id = metadata
            .get("threadId")
            .and_then(Value::as_str)
            .or_else(|| turn_metadata.as_ref()?.get("thread_id")?.as_str())
            .filter(|thread_id| !thread_id.is_empty())
            .ok_or_else(|| McpError::invalid_params("missing task metadata", None))?;
        let turn_id = metadata
            .get("turnId")
            .and_then(Value::as_str)
            .or_else(|| turn_metadata.as_ref()?.get("turn_id")?.as_str())
            .map_or_else(|| format!("mcp-turn-{}", Uuid::new_v4()), str::to_string);
        let call_id = metadata
            .get("callId")
            .and_then(Value::as_str)
            .map_or_else(|| format!("mcp-call-{}", Uuid::new_v4()), str::to_string);
        let params = DynamicToolCallParams {
            thread_id: thread_id.to_string(),
            turn_id,
            call_id,
            namespace: Some(dynamic_tools::NAMESPACE.to_string()),
            tool: request.name.into_owned(),
            arguments: Value::Object(request.arguments.unwrap_or_default()),
        };
        let caller = CallerKey {
            thread_id: params.thread_id.clone(),
            turn_id: params.turn_id.clone(),
            call_id: params.call_id.clone(),
        };
        let _permit = self
            .workflow
            .admit(caller)
            .map_err(|error| McpError::invalid_request(error.to_string(), None))?;
        tracing::debug!(
            thread_id = %params.thread_id,
            turn_id = %params.turn_id,
            call_id = %params.call_id,
            active_calls = self.workflow.active_call_count(),
            "admitted concurrent task-tool call"
        );
        let mut thread_start_params = self.thread_start_params.clone();
        thread_start_params.config.get_or_insert_default().insert(
            format!("mcp_servers.{}", self.services.namespace()),
            self.server_config.clone(),
        );
        // Snapshot once: an in-flight call must never switch connections or replay a mutation.
        let (request_handle, app_event_tx) = self
            .workflow
            .connection()
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let operation = async {
            Ok::<_, McpError>(
                if matches!(
                    params.tool.as_str(),
                    "create_worktree" | "get_worktree_creation_status" | "list_worktrees"
                ) {
                    let service = self.services.worktrees.as_ref().ok_or_else(|| {
                        McpError::invalid_params(
                            "Worktree tools are unavailable on this connection",
                            None,
                        )
                    })?;
                    service
                        .execute(
                            request_handle,
                            params.thread_id,
                            &params.tool,
                            params.arguments,
                        )
                        .await
                } else {
                    if !self.services.task_tools {
                        return Err(McpError::invalid_params(
                            "Task tools are unavailable on this connection",
                            None,
                        ));
                    }
                    dynamic_tools::execute(
                        request_handle,
                        params,
                        thread_start_params,
                        self.features.clone(),
                        self.status_updates.subscribe(),
                        Some(&app_event_tx),
                    )
                    .await
                },
            )
        };
        let response = tokio::select! {
            result = operation => result?,
            _ = context.ct.cancelled() => return Err(McpError::internal_error(
                "task call cancelled; a submitted mutation may already have committed; inspect canonical state", None)),
            _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => return Err(McpError::internal_error(
                "task call timed out; inspect canonical state before retrying a mutation", None)),
        };
        let content = response
            .content_items
            .into_iter()
            .map(|item| match item {
                DynamicToolCallOutputContentItem::InputText { text } => ContentBlock::text(text),
                DynamicToolCallOutputContentItem::InputImage { image_url } => {
                    ContentBlock::text(image_url)
                }
                DynamicToolCallOutputContentItem::InputAudio { audio_url } => {
                    ContentBlock::text(audio_url)
                }
            })
            .collect();
        Ok(if response.success {
            CallToolResult::success(content)
        } else {
            CallToolResult::error(content)
        }
        .into())
    }
}

#[cfg(test)]
#[path = "dynamic_tools_mcp_tests.rs"]
mod tests;
