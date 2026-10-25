//! Approval-gated MCP transport for task tools owned by a local-daemon TUI.

use crate::app_event_sender::AppEventSender;
use crate::dynamic_tools;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
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
use std::sync::PoisonError;
use std::sync::RwLock;
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
            Self::Mcp(_) => {
                params.dynamic_tools = None;
                self.configure_mcp(&mut params.config);
            }
        }
    }

    pub(crate) fn configure_paired_side(
        &self,
        params: &mut ThreadStartParams,
        main_thread_id: codex_protocol::ThreadId,
    ) {
        match self {
            Self::Disabled => params.dynamic_tools = None,
            Self::Dynamic => {
                params.dynamic_tools = Some(dynamic_tools::non_delegation_tool_specs());
            }
            Self::Mcp(server) => {
                params.dynamic_tools = None;
                params.config.get_or_insert_default().insert(
                    format!("mcp_servers.{}", server.namespace),
                    server.paired_config(main_thread_id),
                );
            }
        }
    }

    pub(crate) fn configure_mcp(&self, config: &mut Option<HashMap<String, Value>>) {
        if let Self::Mcp(server) = self {
            config.get_or_insert_default().insert(
                format!("mcp_servers.{}", server.namespace),
                server.config.clone(),
            );
        }
    }

    pub(crate) fn configure_paired_side_mcp(
        &self,
        config: &mut Option<HashMap<String, Value>>,
        main_thread_id: codex_protocol::ThreadId,
    ) {
        if let Self::Mcp(server) = self {
            config.get_or_insert_default().insert(
                format!("mcp_servers.{}", server.namespace),
                server.paired_config(main_thread_id),
            );
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

type ToolConnection = Arc<RwLock<Option<(AppServerRequestHandle, AppEventSender)>>>;
type PairCapabilities = Arc<RwLock<HashMap<String, String>>>;

const PAIRED_MAIN_CAPABILITY_HEADER: &str = "x-codex-ai-lab-pair";

pub(crate) struct DynamicToolMcpServer {
    connection: ToolConnection,
    pair_capabilities: PairCapabilities,
    config: Value,
    namespace: &'static str,
    task: JoinHandle<()>,
}

impl DynamicToolMcpServer {
    fn paired_config(&self, main_thread_id: codex_protocol::ThreadId) -> Value {
        issue_pair_capability(&self.config, &self.pair_capabilities, main_thread_id)
    }

    pub(crate) fn suspend(&self) {
        *self
            .connection
            .write()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    pub(crate) fn reconnect(&self, handle: AppServerRequestHandle, events: AppEventSender) {
        *self
            .connection
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some((handle, events));
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
                "send_message_to_main": {"approval_mode": "prompt"},
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
        let connection = Arc::new(RwLock::new(Some((request_handle, app_event_tx))));
        let pair_capabilities = Arc::new(RwLock::new(HashMap::new()));
        let namespace = services.namespace();
        let handler = DynamicToolMcpHandler {
            connection: Arc::clone(&connection),
            pair_capabilities: Arc::clone(&pair_capabilities),
            thread_start_params,
            features,
            status_updates,
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
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                tracing::warn!(%error, "TUI task-tools MCP server stopped");
            }
        });
        Ok(Self {
            connection,
            pair_capabilities,
            config: server_config,
            namespace,
            task,
        })
    }
}

fn issue_pair_capability(
    base_config: &Value,
    pair_capabilities: &PairCapabilities,
    main_thread_id: codex_protocol::ThreadId,
) -> Value {
    let capability = Uuid::new_v4().to_string();
    let main_thread_id = main_thread_id.to_string();
    let mut capabilities = pair_capabilities
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    // Resuming a pair rotates its bearer capability. A stale SIDE config
    // must not remain authorized after the durable pairing is reattached.
    capabilities.retain(|_, paired_main| paired_main != &main_thread_id);
    capabilities.insert(capability.clone(), main_thread_id);
    drop(capabilities);
    let mut config = base_config.clone();
    config["http_headers"][PAIRED_MAIN_CAPABILITY_HEADER] = Value::String(capability);
    config
}

fn trusted_paired_main_from_headers(
    headers: &axum::http::HeaderMap,
    pair_capabilities: &PairCapabilities,
) -> Option<String> {
    headers
        .get(PAIRED_MAIN_CAPABILITY_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|capability| {
            pair_capabilities
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .get(capability)
                .cloned()
        })
}

impl Drop for DynamicToolMcpServer {
    fn drop(&mut self) {
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
    connection: ToolConnection,
    pair_capabilities: PairCapabilities,
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
                        // Strict SIDE policy treats this as its sole explicit
                        // state-changing exception; core narrows the codex_tui
                        // namespace back down to the paired-target-only bridge.
                        | "send_message_to_main"
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
        let trusted_paired_main = context.extensions.get::<Parts>().and_then(|parts| {
            trusted_paired_main_from_headers(&parts.headers, &self.pair_capabilities)
        });
        if request.name.as_ref() == "send_message_to_main" && trusted_paired_main.is_none() {
            return Err(McpError::invalid_params(
                "send_message_to_main requires an authenticated SIDE pairing",
                None,
            ));
        }
        let params = DynamicToolCallParams {
            thread_id: thread_id.to_string(),
            turn_id,
            call_id,
            namespace: Some(dynamic_tools::NAMESPACE.to_string()),
            tool: request.name.into_owned(),
            arguments: Value::Object(request.arguments.unwrap_or_default()),
        };
        let mut thread_start_params = self.thread_start_params.clone();
        thread_start_params.config.get_or_insert_default().insert(
            format!("mcp_servers.{}", self.services.namespace()),
            self.server_config.clone(),
        );
        // Snapshot once: an in-flight call must never switch connections or replay a mutation.
        let (request_handle, app_event_tx) = self
            .connection
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
                McpError::internal_error("TUI is reconnecting; tool was not sent", None)
            })?;
        let response = if matches!(
            params.tool.as_str(),
            "create_worktree" | "get_worktree_creation_status" | "list_worktrees"
        ) {
            let service = self.services.worktrees.as_ref().ok_or_else(|| {
                McpError::invalid_params("Worktree tools are unavailable on this connection", None)
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
            dynamic_tools::execute_with_paired_main(
                request_handle,
                params,
                thread_start_params,
                self.features.clone(),
                self.status_updates.subscribe(),
                Some(&app_event_tx),
                trusted_paired_main,
            )
            .await
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
mod tests {
    use super::*;

    fn pair_token(config: &Value) -> &str {
        config["http_headers"][PAIRED_MAIN_CAPABILITY_HEADER]
            .as_str()
            .expect("paired capability header")
    }

    #[test]
    fn pair_capabilities_are_isolated_and_rotate_on_resume() {
        let capabilities: PairCapabilities = Arc::new(RwLock::new(HashMap::new()));
        let base = json!({
            "url": "http://127.0.0.1:1234/mcp",
            "http_headers": {"Authorization": "Bearer shared"}
        });
        let main_a = codex_protocol::ThreadId::new();
        let main_b = codex_protocol::ThreadId::new();

        let fresh_a = issue_pair_capability(&base, &capabilities, main_a);
        let fresh_b = issue_pair_capability(&base, &capabilities, main_b);
        let token_a = pair_token(&fresh_a).to_string();
        let token_b = pair_token(&fresh_b).to_string();
        assert_ne!(token_a, token_b);
        assert_eq!(fresh_a["http_headers"]["Authorization"], "Bearer shared");
        assert_eq!(fresh_b["http_headers"]["Authorization"], "Bearer shared");

        let resumed_a = issue_pair_capability(&base, &capabilities, main_a);
        let resumed_token_a = pair_token(&resumed_a).to_string();
        assert_ne!(resumed_token_a, token_a);
        let capability_map = capabilities.read().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(capability_map.get(&token_a), None);
        assert_eq!(capability_map.get(&token_b), Some(&main_b.to_string()));
        assert_eq!(
            capability_map.get(&resumed_token_a),
            Some(&main_a.to_string())
        );
        drop(capability_map);

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            PAIRED_MAIN_CAPABILITY_HEADER,
            resumed_token_a.parse().expect("valid capability header"),
        );
        assert_eq!(
            trusted_paired_main_from_headers(&headers, &capabilities),
            Some(main_a.to_string())
        );
        headers.insert(
            PAIRED_MAIN_CAPABILITY_HEADER,
            "unknown-capability".parse().expect("valid header"),
        );
        assert_eq!(
            trusted_paired_main_from_headers(&headers, &capabilities),
            None
        );
    }
}
