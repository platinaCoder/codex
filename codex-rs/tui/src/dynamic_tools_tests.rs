use super::*;
use crate::app_server_session::AppServerSession;
use crate::app_server_session::ResumeModelSettings;
use crate::legacy_core::config::ConfigBuilder;
use app_test_support::create_fake_paginated_rollout;
use app_test_support::create_fake_rollout;
use app_test_support::rollout_path;
use codex_protocol::ThreadId;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

async fn test_server(
    tool_arguments: Value,
) -> color_eyre::Result<(TempDir, AppServerSession, String, String)> {
    test_server_with_cwd(tool_arguments, /*cwd*/ None).await
}

async fn test_server_with_cwd(
    tool_arguments: Value,
    cwd: Option<&std::path::Path>,
) -> color_eyre::Result<(TempDir, AppServerSession, String, String)> {
    let codex_home = tempfile::tempdir()?;
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await?;
    if let Some(cwd) = cwd {
        config.cwd = codex_utils_absolute_path::AbsolutePathBuf::try_from(cwd.to_path_buf())?;
    }
    let target = create_fake_paginated_rollout(
        codex_home.path(),
        "2026-01-02T00-00-00",
        "2026-01-02T00:00:00Z",
        "Persisted test task",
        Some(config.model_provider_id.as_str()),
        /*git_info*/ None,
    )
    .map_err(|error| color_eyre::eyre::eyre!("failed to create test rollout: {error}"))?;
    let path = rollout_path(codex_home.path(), "2026-01-02T00-00-00", &target);
    let mut records = std::fs::read_to_string(&path)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    for payload in [
        json!({
            "type": "task_started",
            "turn_id": "persisted-turn",
            "model_context_window": null
        }),
        json!({
            "type": "item_completed",
            "thread_id": target,
            "turn_id": "persisted-turn",
            "item": {
                "type": "AgentMessage",
                "id": "persisted-message",
                "phase": "final_answer",
                "content": [{"type": "Text", "text": "Persisted assistant output".repeat(120)}]
            },
            "completed_at_ms": 0
        }),
        json!({
            "type": "item_completed",
            "thread_id": target,
            "turn_id": "persisted-turn",
            "item": {
                "type": "DynamicToolCall",
                "id": "persisted-tool",
                "namespace": "codex_tui",
                "tool": "list_threads",
                "arguments": tool_arguments,
                "status": "completed",
                "success": true
            },
            "completed_at_ms": 0
        }),
        json!({
            "type": "task_complete",
            "turn_id": "persisted-turn",
            "last_agent_message": "Persisted assistant output"
        }),
    ] {
        serde_json::from_value::<codex_protocol::protocol::EventMsg>(payload.clone())?;
        records.push(json!({
            "timestamp": "2026-01-02T00:00:00Z",
            "ordinal": records.len(),
            "type": "event_msg",
            "payload": payload
        }));
    }
    let records = records
        .into_iter()
        .map(|record| record.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, format!("{records}\n"))?;
    let mut server = crate::start_embedded_app_server_for_picker(&config).await?;
    let source = server
        .start_thread(&config)
        .await?
        .session
        .thread_id
        .to_string();
    server
        .resume_thread(
            &crate::local_settings::LocalSettings::from(&config),
            config,
            ThreadId::from_string(&target)?,
            ResumeModelSettings::RestoreFromThread,
        )
        .await?;
    Ok((codex_home, server, source, target))
}

async fn call_tool(
    server: &AppServerSession,
    source: &str,
    name: &str,
    arguments: Value,
) -> DynamicToolCallResponse {
    let (_status_sender, status_receiver) = broadcast::channel(/*capacity*/ 8);
    execute(
        server.request_handle(),
        DynamicToolCallParams {
            thread_id: source.to_string(),
            turn_id: "persisted-turn".to_string(),
            call_id: "call-1".to_string(),
            namespace: Some(NAMESPACE.to_string()),
            tool: name.to_string(),
            arguments,
        },
        ThreadStartParams {
            dynamic_tools: Some(tool_specs()),
            ephemeral: Some(true),
            ..ThreadStartParams::default()
        },
        Features::with_defaults(),
        status_receiver,
        /*app_event_tx*/ None,
    )
    .await
}

async fn call_ticket_tool(
    server: &AppServerSession,
    source: &str,
    name: &str,
    arguments: Value,
    workspace_root: &std::path::Path,
) -> DynamicToolCallResponse {
    call_ticket_tool_at_turn(
        server,
        source,
        "ticket-turn",
        name,
        arguments,
        workspace_root,
    )
    .await
}

async fn call_ticket_tool_at_turn(
    server: &AppServerSession,
    source: &str,
    turn_id: &str,
    name: &str,
    arguments: Value,
    workspace_root: &std::path::Path,
) -> DynamicToolCallResponse {
    let (_status_sender, status_receiver) = broadcast::channel(/*capacity*/ 8);
    execute(
        server.request_handle(),
        DynamicToolCallParams {
            thread_id: source.to_string(),
            turn_id: turn_id.to_string(),
            call_id: "ticket-call".to_string(),
            namespace: Some(NAMESPACE.to_string()),
            tool: name.to_string(),
            arguments,
        },
        ThreadStartParams {
            dynamic_tools: Some(tool_specs()),
            runtime_workspace_roots: Some(vec![
                codex_utils_absolute_path::AbsolutePathBuf::try_from(workspace_root.to_path_buf())
                    .expect("ticket test root must be absolute"),
            ]),
            ephemeral: Some(true),
            ..ThreadStartParams::default()
        },
        Features::with_defaults(),
        status_receiver,
        /*app_event_tx*/ None,
    )
    .await
}

fn response_json(response: DynamicToolCallResponse) -> Value {
    assert!(response.success, "tool call failed: {response:?}");
    let [DynamicToolCallOutputContentItem::InputText { text }] = response.content_items.as_slice()
    else {
        panic!("expected one JSON text response")
    };
    serde_json::from_str(text).expect("dynamic tool response should contain JSON")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_read_preserves_answer_in_next_model_request() -> color_eyre::Result<()> {
    let (_home, app_server, source, target) =
        test_server(json!({"padding": "supporting tool arguments".repeat(4_000)})).await?;
    let mock_server = responses::start_mock_server().await;
    let call_id = "read-task";
    let mock = responses::mount_sse_sequence(
        &mock_server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-1"),
                responses::ev_function_call_with_namespace(
                    call_id,
                    NAMESPACE,
                    "read_thread",
                    &json!({"threadId": target}).to_string(),
                ),
                responses::ev_completed("resp-1"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-2"),
                responses::ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let home = tempfile::tempdir()?;
    let base_url = mock_server.uri();
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            r#"
model = "gpt-5.5"
model_provider = "readback-test"
tool_output_token_limit = 2500
[model_providers.readback-test]
name = "Readback test"
base_url = "{base_url}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#
        ),
    )?;
    let config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .build()
        .await?;
    let mut model_server = crate::start_embedded_app_server_for_picker(&config).await?;
    let started: codex_app_server_protocol::ThreadStartResponse =
        request(&model_server.request_handle(), |request_id| {
            ClientRequest::ThreadStart {
                request_id,
                params: ThreadStartParams {
                    dynamic_tools: Some(tool_specs()),
                    ..ThreadStartParams::default()
                },
            }
        })
        .await
        .map_err(|error| color_eyre::eyre::eyre!(error))?;
    let _: TurnStartResponse = request(&model_server.request_handle(), |request_id| {
        ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: started.thread.id,
                input: vec![UserInput::Text {
                    text: "Read the completed task".to_string(),
                    text_elements: Vec::new(),
                }],
                ..TurnStartParams::default()
            },
        }
    })
    .await
    .map_err(|error| color_eyre::eyre::eyre!(error))?;
    let result = call_tool(
        &app_server,
        &source,
        "read_thread",
        json!({"threadId": target}),
    )
    .await;
    let expected = response_json(result.clone());
    assert_eq!(expected["truncated"], true);
    assert_eq!(
        expected["turns"][0]["items"],
        json!([{
            "type": "agentMessage", "id": "persisted-message", "phase": "final_answer",
            "text": "Persisted assistant output".repeat(120)
        }])
    );
    assert_eq!(expected["page"]["order"], "newest_first");
    tokio::time::timeout(Duration::from_secs(/*secs*/ 30), async {
        while let Some(event) = model_server.next_event().await {
            match event.event {
                codex_app_server_client::AppServerEvent::ServerRequest(request) => {
                    if let codex_app_server_protocol::ServerRequest::DynamicToolCall {
                        request_id,
                        params,
                    } = *request
                    {
                        assert_eq!(params.call_id, call_id);
                        model_server
                            .resolve_server_request(request_id, serde_json::to_value(&result)?)
                            .await?;
                    }
                }
                codex_app_server_client::AppServerEvent::ServerNotification(notification) => {
                    if let codex_app_server_protocol::ServerNotification::TurnCompleted(_) =
                        *notification
                    {
                        return Ok::<_, color_eyre::eyre::Report>(());
                    }
                }
                codex_app_server_client::AppServerEvent::Lagged { .. }
                | codex_app_server_client::AppServerEvent::Disconnected { .. } => {
                    panic!("lost app-server events during the tool round trip");
                }
            }
        }
        panic!("app server disconnected before turn completion");
    })
    .await??;
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let output = requests[1].function_call_output(call_id);
    let text = output["output"].as_str().expect("text response");
    assert!(text.len() > 999);
    assert!(text.len() <= response::MAX_RESPONSE_BYTES);
    assert_eq!(serde_json::from_str::<Value>(text)?, expected);
    model_server.shutdown().await?;
    Ok(())
}

#[test]
fn delegated_prompts_match_desktop_xml_contract() {
    let output = FunctionCallOutputBody::Text(delegated_prompt("thread-1", "Check status"));
    for namespace in ["codex_tui", "codex_app"] {
        assert_eq!(
            parse_delegated_tool_output("send_message_to_thread", Some(namespace), &output),
            Some(("thread-1".to_string(), "Check status".to_string()))
        );
    }
    assert_eq!(
        parse_delegated_tool_output("send_message_to_thread", Some("untrusted"), &output),
        None
    );
    assert_eq!(
        delegated_prompt("thread-1", "Check <main> & report > status"),
        "<codex_delegation>\n  <source_thread_id>thread-1</source_thread_id>\n  <input>Check &lt;main&gt; &amp; report &gt; status</input>\n</codex_delegation>"
    );
    assert!(
        validate_prompt(
            &delegated_prompt("thread-1", &"&".repeat(MAX_INPUT_BYTES)),
            MAX_DELEGATED_INPUT_BYTES,
        )
        .is_err()
    );
}

#[test]
fn activity_metadata_is_retained_without_including_outputs() -> color_eyre::Result<()> {
    let assistant_text = "Working".repeat(400);
    let turn: Turn = serde_json::from_value(json!({
        "id": "turn-1",
        "status": "completed",
        "items": [
            {"type": "reasoning", "id": "thought-1", "summary": ["Thinking"], "content": ["Private reasoning"]},
            {"type": "commandExecution", "id": "command-1", "command": "cargo test", "cwd": "/tmp",
                "status": "completed", "commandActions": [], "aggregatedOutput": "Command output", "exitCode": 0},
            {"type": "fileChange", "id": "patch-1", "status": "completed",
                "changes": [{"path": "src/main.rs", "kind": {"type": "add"}, "diff": "+hello"}]},
            {"type": "mcpToolCall", "id": "mcp-1", "server": "docs", "tool": "search",
                "status": "completed", "arguments": {}},
            {"type": "userMessage", "id": "user-1", "content": [
                {"type": "text", "text": delegated_prompt("source-1", "Check <main> & status")},
                {"type": "skill", "name": "debug", "path": "/tmp/SKILL.md"},
                {"type": "mention", "name": "docs", "path": "app://docs"}
            ]},
            {"type": "agentMessage", "id": "assistant-1", "text": assistant_text, "phase": "commentary"},
            {"type": "webSearch", "id": "web-1", "query": "latest docs", "action": null},
            {"type": "sleep", "id": "sleep-1", "durationMs": 1000},
            {"type": "imageGeneration", "id": "image-1", "status": "completed",
                "revisedPrompt": "a cat", "result": "image bytes"},
            {"type": "enteredReviewMode", "id": "review-1", "review": "review changes"},
            {"type": "functionCallOutput", "id": "delegation-1", "name": "send_message_to_thread",
                "namespace": "codex_tui", "output": delegated_prompt("source-2", "Follow <up> & report")}
        ]
    }))?;

    let summary = response_json(
        success_response(turn_summary(
            &turn,
            /*include_outputs*/ false,
            DEFAULT_OUTPUT_CHARS,
        ))
        .expect("task response"),
    );
    assert_eq!(
        summary["items"],
        json!([
            {"type": "reasoning", "id": "thought-1", "summary": ["Thinking"]},
            {"type": "commandExecution", "id": "command-1", "command": "cargo test", "cwd": "/tmp", "exitCode": 0, "status": "completed", "durationMs": null},
            {"type": "fileChange", "id": "patch-1", "status": "completed",
                "changes": [{"path": "src/main.rs", "kind": {"type": "add"}}]},
            {"type": "mcpToolCall", "id": "mcp-1", "server": "docs", "tool": "search", "arguments": {}, "status": "completed", "durationMs": null},
            {"type": "userMessage", "id": "user-1", "content": [
                {"type": "text", "text": delegated_prompt("source-1", "Check <main> & status"),
                    "codexDelegation": {"sourceThreadId": "source-1", "input": "Check <main> & status"}},
                {"type": "skill", "name": "debug", "path": "/tmp/SKILL.md"},
                {"type": "mention", "name": "docs", "path": "app://docs"}
            ]},
            {"type": "agentMessage", "id": "assistant-1", "text": assistant_text, "phase": "commentary"},
            {"type": "webSearch", "id": "web-1", "query": "latest docs", "action": null},
            {"type": "sleep", "id": "sleep-1", "durationMs": 1000},
            {"type": "imageGeneration", "id": "image-1", "status": "completed",
                "revisedPrompt": "a cat", "savedPath": null},
            {"type": "enteredReviewMode", "id": "review-1", "review": "review changes"},
            {"type": "functionCallOutput", "id": "delegation-1", "name": "send_message_to_thread",
                "namespace": "codex_tui", "codexDelegation": {
                    "sourceThreadId": "source-2", "input": "Follow <up> & report"
                }}
        ])
    );

    let full = turn_summary(&turn, /*include_outputs*/ true, DEFAULT_OUTPUT_CHARS);
    assert_eq!(
        full["items"][0]["content"],
        json!([{"text": "Private reasoning", "truncated": false}])
    );
    assert_eq!(
        full["items"][1]["output"],
        json!({"text": "Command output", "truncated": false})
    );
    assert_eq!(
        full["items"][2]["changes"][0]["diff"],
        json!({"text": "+hello", "truncated": false})
    );
    assert_eq!(
        full["items"][8]["result"],
        json!({"text": "image bytes", "truncated": false})
    );

    let no_outputs = turn_summary(
        &turn, /*include_outputs*/ true, /*output_chars*/ 0,
    );
    assert_eq!(no_outputs["items"][5]["text"], assistant_text);
    assert_eq!(
        no_outputs["items"][1]["output"],
        json!({"text": "", "truncated": true, "originalChars": 14})
    );
    assert_eq!(
        no_outputs["items"][8]["result"],
        json!({"text": "", "truncated": true, "originalChars": 11})
    );
    Ok(())
}

/// File-backed generated images retain their reference when task output is requested without
/// exposing that output when the caller asks for metadata only.
#[test]
fn file_backed_image_generation_is_retained_as_a_reference() -> color_eyre::Result<()> {
    let turn: Turn = serde_json::from_value(json!({
        "id": "turn-1",
        "status": "completed",
        "items": [{
            "type": "imageGeneration",
            "id": "image-1",
            "status": "completed",
            "fileId": "file-generated-image"
        }]
    }))?;

    assert_eq!(
        turn_summary(&turn, /*include_outputs*/ true, DEFAULT_OUTPUT_CHARS)["items"][0],
        json!({
            "type": "imageGeneration",
            "id": "image-1",
            "status": "completed",
            "revisedPrompt": null,
            "savedPath": null,
            "fileId": "file-generated-image"
        })
    );
    assert_eq!(
        turn_summary(&turn, /*include_outputs*/ false, DEFAULT_OUTPUT_CHARS)["items"][0],
        json!({
            "type": "imageGeneration",
            "id": "image-1",
            "status": "completed",
            "revisedPrompt": null,
            "savedPath": null
        })
    );
    Ok(())
}

#[tokio::test]
async fn task_management_tools_use_existing_app_server_operations() -> color_eyre::Result<()> {
    let (codex_home, server, source, target) = test_server(json!({})).await?;

    let listed = response_json(call_tool(&server, &source, "list_threads", json!({})).await);
    assert!(
        listed["threads"]
            .as_array()
            .is_some_and(|threads| { threads.iter().any(|thread| thread["id"] == target) })
    );

    let legacy = create_fake_rollout(
        codex_home.path(),
        "2026-01-03T00-00-00",
        "2026-01-03T00:00:00Z",
        "Legacy test task",
        Some("openai"),
        /*git_info*/ None,
    )
    .map_err(|error| color_eyre::eyre::eyre!("failed to create legacy rollout: {error}"))?;
    for thread_id in [&target, &legacy] {
        let read = response_json(
            call_tool(
                &server,
                &source,
                "read_thread",
                json!({"threadId": thread_id, "includeOutputs": true, "maxOutputCharsPerItem": 0}),
            )
            .await,
        );
        assert_eq!(read["schemaVersion"], 1);
        assert_eq!(read["thread"]["id"], *thread_id);
        assert_eq!(read["page"]["order"], "newest_first");
        assert!(read["turns"].is_array());
        if thread_id == &target {
            let assistant = read["turns"][0]["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["type"] == "agentMessage")
                .expect("assistant reply");
            assert_eq!(
                assistant,
                &json!({
                    "type": "agentMessage", "id": "persisted-message",
                    "text": "Persisted assistant output".repeat(120), "phase": "final_answer"
                })
            );
        }
    }

    let renamed = response_json(
        call_tool(
            &server,
            &source,
            "set_thread_title",
            json!({"threadId": target, "title": "Renamed task"}),
        )
        .await,
    );
    assert_eq!(
        renamed,
        json!({"threadId": target, "title": "Renamed task"})
    );

    let forked = response_json(
        call_tool(&server, &source, "fork_thread", json!({"threadId": target})).await,
    );
    assert_ne!(forked["threadId"], target);
    let self_forked = response_json(call_tool(&server, &target, "fork_thread", json!({})).await);
    assert_ne!(self_forked["threadId"], target);
    assert_eq!(self_forked["sourceThreadId"], target);
    assert_eq!(
        self_forked["environment"],
        json!({"type": "same-directory"})
    );

    let self_archive = call_tool(
        &server,
        &source,
        "set_thread_archived",
        json!({"threadId": source.to_uppercase(), "archived": true}),
    )
    .await;
    assert!(!self_archive.success);

    let archived = response_json(
        call_tool(
            &server,
            &source,
            "set_thread_archived",
            json!({"threadId": target, "archived": true}),
        )
        .await,
    );
    assert_eq!(archived, json!({"threadId": target, "archived": true}));

    let mut expected_archived = vec![target.clone()];
    for day in 4..12 {
        let archived_id = create_fake_paginated_rollout(
            codex_home.path(),
            &format!("2026-01-{day:02}T00-00-00"),
            &format!("2026-01-{day:02}T00:00:00Z"),
            "Archived task with a deliberately descriptive pagination title",
            Some("openai"),
            /*git_info*/ None,
        )
        .map_err(|error| color_eyre::eyre::eyre!("failed to create archived rollout: {error}"))?;
        let archived = call_tool(
            &server,
            &source,
            "set_thread_archived",
            json!({"threadId": archived_id, "archived": true}),
        )
        .await;
        assert!(archived.success, "{archived:?}");
        expected_archived.push(archived_id);
    }
    let mut archived_threads = response_json(
        call_tool(
            &server,
            &source,
            "list_archived_threads",
            json!({"limit": 2}),
        )
        .await,
    );
    assert!(
        archived_threads["threads"]
            .as_array()
            .is_some_and(|threads| threads.len() < expected_archived.len())
    );
    let mut listed_archived = Vec::new();
    loop {
        listed_archived.extend(
            archived_threads["threads"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|thread| thread["id"].as_str().map(ToString::to_string)),
        );
        let Some(cursor) = archived_threads["nextCursor"].as_str() else {
            break;
        };
        archived_threads = response_json(
            call_tool(
                &server,
                &source,
                "list_archived_threads",
                json!({"cursor": cursor}),
            )
            .await,
        );
    }
    expected_archived.sort();
    listed_archived.sort();
    assert_eq!(listed_archived, expected_archived);

    let restored = response_json(
        call_tool(
            &server,
            &source,
            "set_thread_archived",
            json!({"threadId": target, "archived": false}),
        )
        .await,
    );
    assert_eq!(restored, json!({"threadId": target, "archived": false}));

    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn wait_threads_preserves_snapshots_and_rejects_self_wait() -> color_eyre::Result<()> {
    let (_codex_home, server, source, target) = test_server(json!({})).await?;

    let snapshot = response_json(
        call_tool(
            &server,
            &source,
            "wait_threads",
            json!({"targets": [{"threadId": target}, {"threadId": "missing-task"}], "timeoutMs": 0}),
        )
        .await,
    );
    assert_eq!(snapshot["timedOut"], false);
    assert_eq!(snapshot["wake"]["threadId"], target);
    assert_eq!(snapshot["wake"]["reason"], "turnCompleted");
    assert!(snapshot.get("errors").is_none());
    assert_eq!(
        snapshot["wake"]["turnId"],
        snapshot["polls"][0]["latestTurn"]["id"]
    );
    let assistant = &snapshot["polls"][0]["latestAssistantMessage"];
    assert_eq!(assistant["id"], "persisted-message");
    assert_eq!(
        assistant["turnId"],
        snapshot["polls"][0]["latestTurn"]["id"]
    );
    assert_eq!(
        assistant,
        &json!({
            "id": "persisted-message", "turnId": "persisted-turn",
            "text": "Persisted assistant output".repeat(120), "phase": "final_answer"
        })
    );
    assert_eq!(
        snapshot["polls"][0]["latestToolMarker"],
        json!({
            "id": "persisted-tool", "turnId": "persisted-turn", "type": "dynamicToolCall",
            "name": "list_threads", "status": "completed"
        })
    );
    let cursor = snapshot["polls"][0]["cursor"]
        .as_str()
        .expect("snapshot cursor")
        .to_string();
    let cursor_value: Value = serde_json::from_str(&cursor)?;
    assert_eq!(
        cursor_value["turnId"],
        snapshot["polls"][0]["latestTurn"]["id"]
    );
    assert_eq!(
        cursor_value["turnStatus"],
        snapshot["polls"][0]["latestTurn"]["status"]
    );
    assert_eq!(cursor_value["latestItemId"], "persisted-tool");

    let unchanged = response_json(
        call_tool(
            &server,
            &source,
            "wait_threads",
            json!({"targets": [{"threadId": target, "afterCursor": cursor}], "timeoutMs": 0}),
        )
        .await,
    );
    assert_eq!(unchanged["timedOut"], true);
    assert_eq!(unchanged["wake"], Value::Null);
    assert_eq!(unchanged["polls"][0]["changed"], false);
    assert_eq!(unchanged["polls"][0]["latestAssistantMessage"], Value::Null);

    let self_wait = call_tool(
        &server,
        &source,
        "wait_threads",
        json!({"targets": [{"threadId": source.to_uppercase()}], "timeoutMs": 0}),
    )
    .await;
    assert!(!self_wait.success);

    let duplicate_wait = call_tool(
        &server,
        &source,
        "wait_threads",
        json!({"targets": [{"threadId": target}, {"threadId": target.to_uppercase()}], "timeoutMs": 0}),
    )
    .await;
    assert!(!duplicate_wait.success);

    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn task_creation_and_followup_start_background_turns() -> color_eyre::Result<()> {
    let (_codex_home, server, source, target) = test_server(json!({})).await?;

    for (tool, arguments) in [
        (
            "create_thread",
            json!({"prompt": "&".repeat(MAX_INPUT_BYTES)}),
        ),
        (
            "send_message_to_thread",
            json!({"threadId": target, "prompt": "&".repeat(MAX_INPUT_BYTES)}),
        ),
    ] {
        assert!(!call_tool(&server, &source, tool, arguments).await.success);
    }
    assert!(
        !call_tool(
            &server,
            &source,
            "send_message_to_thread",
            json!({"threadId": target, "prompt": "Follow up", "model": ""}),
        )
        .await
        .success
    );

    let ephemeral: codex_app_server_protocol::ThreadStartResponse =
        request(&server.request_handle(), |request_id| {
            ClientRequest::ThreadStart {
                request_id,
                params: ThreadStartParams {
                    ephemeral: Some(true),
                    ..ThreadStartParams::default()
                },
            }
        })
        .await
        .map_err(color_eyre::eyre::Error::msg)?;
    let rejected = call_tool(
        &server,
        &ephemeral.thread.id,
        "create_thread",
        json!({"prompt": "Start a background task"}),
    )
    .await;
    assert!(!rejected.success);

    let created = response_json(
        call_tool(
            &server,
            &target,
            "create_thread",
            json!({"prompt": "x".repeat(MAX_INPUT_BYTES), "title": "Background task"}),
        )
        .await,
    );
    assert!(created["threadId"].is_string());

    let continued = response_json(
        call_tool(
            &server,
            &source,
            "send_message_to_thread",
            json!({"threadId": target, "prompt": "x".repeat(MAX_INPUT_BYTES)}),
        )
        .await,
    );
    assert_eq!(continued["threadId"], target);

    let oversized = call_tool(
        &server,
        &source,
        "list_archived_threads",
        json!({"cursor": "x".repeat(MAX_ERROR_CHARS + 1)}),
    )
    .await;
    assert!(!oversized.success);
    assert!(
        matches!(&oversized.content_items[..], [DynamicToolCallOutputContentItem::InputText { text }] if text.chars().count() <= MAX_ERROR_CHARS)
    );

    server.shutdown().await?;
    Ok(())
}

#[test]
fn strict_side_command_write_attempts_have_no_dispatch_surface() -> color_eyre::Result<()> {
    let workspace = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    let workspace_file = workspace.path().join("redirected.txt");
    let workspace_dir = workspace.path().join("created");
    let temp_file = outside.path().join("touched.txt");
    let git_config = workspace.path().join("git-config");
    let attempts = [
        format!("printf unsafe > {}", workspace_file.display()),
        format!("touch {}", temp_file.display()),
        format!("mkdir {}", workspace_dir.display()),
        format!(
            "git config --file {} user.name unsafe",
            git_config.display()
        ),
    ];

    let tool_names = tool_specs()
        .into_iter()
        .flat_map(|spec| match spec {
            DynamicToolSpec::Namespace(namespace) => namespace
                .tools
                .into_iter()
                .map(|tool| match tool {
                    DynamicToolNamespaceTool::Function(function) => function.name,
                })
                .collect::<Vec<_>>(),
            DynamicToolSpec::Function(function) => vec![function.name],
        })
        .collect::<Vec<_>>();
    for command in attempts {
        let attempted_call = json!({"tool": "exec_command", "arguments": {"cmd": command}});
        assert!(
            !tool_names.iter().any(|name| name == "exec_command"),
            "write attempt unexpectedly had a command transport: {attempted_call}"
        );
    }
    assert!(!workspace_file.exists());
    assert!(!workspace_dir.exists());
    assert!(!temp_file.exists());
    assert!(!git_config.exists());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedicated_workspace_tools_read_without_shell_execution() -> color_eyre::Result<()> {
    let workspace = tempfile::tempdir()?;
    std::fs::create_dir(workspace.path().join("src"))?;
    std::fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"side-read-test\"\n",
    )?;
    std::fs::write(
        workspace.path().join("src/lib.rs"),
        "pub const NEEDLE: &str = \"paired-main\";\n",
    )?;
    let (_codex_home, server, source, _target) =
        test_server_with_cwd(json!({}), Some(workspace.path())).await?;

    let read = response_json(
        call_tool(
            &server,
            &source,
            "read_workspace_file",
            json!({"path": "Cargo.toml", "maxChars": 2000}),
        )
        .await,
    );
    assert!(
        read["text"]
            .as_str()
            .is_some_and(|text| text.contains("side-read-test"))
    );

    let search = response_json(
        call_tool(
            &server,
            &source,
            "search_workspace_text",
            json!({"path": "src", "query": "paired-main", "maxResults": 5}),
        )
        .await,
    );
    assert!(
        search["matches"]
            .as_array()
            .is_some_and(|matches| !matches.is_empty())
    );

    let list = response_json(
        call_tool(
            &server,
            &source,
            "list_workspace_directory",
            json!({"path": "src"}),
        )
        .await,
    );
    assert!(
        list["entries"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty())
    );

    let escape = call_tool(
        &server,
        &source,
        "read_workspace_file",
        json!({"path": "/tmp"}),
    )
    .await;
    assert!(!escape.success);

    server.shutdown().await?;
    Ok(())
}

#[test]
fn ticket_capabilities_are_exact_pathless_and_mcp_only() {
    let specs = tool_specs();
    let functions = specs
        .into_iter()
        .flat_map(|spec| match spec {
            DynamicToolSpec::Function(function) => vec![function],
            DynamicToolSpec::Namespace(namespace) => namespace
                .tools
                .into_iter()
                .map(|tool| match tool {
                    DynamicToolNamespaceTool::Function(function) => function,
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    let ticket_tools = functions
        .iter()
        .filter(|function| TICKET_TOOLS.contains(&function.name.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(ticket_tools.len(), 4);
    for function in ticket_tools {
        let properties = function.input_schema["properties"]
            .as_object()
            .expect("ticket tool properties");
        assert!(!properties.contains_key("path"));
        assert!(!properties.contains_key("root"));
        assert!(!properties.contains_key("cwd"));
    }
    let direct_names = side_fallback_tool_specs()
        .into_iter()
        .flat_map(|spec| match spec {
            DynamicToolSpec::Function(function) => vec![function.name],
            DynamicToolSpec::Namespace(namespace) => namespace
                .tools
                .into_iter()
                .map(|tool| match tool {
                    DynamicToolNamespaceTool::Function(function) => function.name,
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    for name in TICKET_TOOLS {
        assert!(direct_names.iter().any(|candidate| candidate == name));
    }
    for name in DELEGATION_TOOLS {
        assert!(!direct_names.iter().any(|candidate| candidate == name));
    }
}

#[test]
fn ticket_handlers_reject_main_mutation_and_allow_side_lifecycle_management()
-> color_eyre::Result<()> {
    std::thread::Builder::new()
        .name("ticket-dispatch-flow-test".to_string())
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(ticket_dispatch_flow_scenario())
        })?
        .join()
        .map_err(|panic| color_eyre::eyre::eyre!("ticket dispatch test panicked: {panic:?}"))?
}

async fn ticket_dispatch_flow_scenario() -> color_eyre::Result<()> {
    let lab = tempfile::tempdir()?;
    std::fs::create_dir_all(lab.path().join("runtime"))?;
    std::fs::write(lab.path().join("runtime/codex-upstream.toml"), "url='test'")?;
    std::fs::create_dir(lab.path().join(".git"))?;
    let codex_home = tempfile::tempdir()?;
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await?;
    config.cwd = codex_utils_absolute_path::AbsolutePathBuf::try_from(lab.path().to_path_buf())?;
    let mut server = crate::start_embedded_app_server_for_picker(&config).await?;
    let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
    let (status_updates, _status_rx) = broadcast::channel(/*capacity*/ 8);
    server
        .start_dynamic_tool_mcp(
            config.clone(),
            AppEventSender::new(events_tx),
            status_updates,
        )
        .await?;
    let main = server
        .start_thread(&config)
        .await?
        .session
        .thread_id
        .to_string();
    server
        .thread_inject_items(
            ThreadId::from_string(&main)?,
            vec![serde_json::from_value(json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "materialized MAIN history"}]
            }))?],
        )
        .await?;
    let mut side_config = config.clone();
    side_config.strict_read_only_tools = true;
    let side = server
        .start_side_thread(
            &crate::local_settings::LocalSettings::from(&side_config),
            &side_config,
            ThreadId::from_string(&main)?,
            /*selected_profile*/ None,
        )
        .await?
        .session
        .thread_id;
    server
        .thread_inject_items(
            side,
            vec![serde_json::from_value(json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "materialized SIDE history"}]
            }))?],
        )
        .await?;
    let create = json!({
        "title": "SIDE-owned lifecycle",
        "scope": ["runtime"],
        "proposal": {"originalIdea": "prove role ownership"}
    });
    let denied =
        call_ticket_tool(&server, &main, "create_ticket", create.clone(), lab.path()).await;
    assert!(!denied.success);
    assert!(format!("{denied:?}").contains("owned by the paired SIDE"));

    let created = response_json(
        call_ticket_tool(
            &server,
            &side.to_string(),
            "create_ticket",
            create,
            lab.path(),
        )
        .await,
    );
    assert_eq!(created["document"]["ticket"]["status"], "proposed");
    let approved = response_json(
        call_ticket_tool(
            &server,
            &side.to_string(),
            "update_ticket",
            json!({
                "ticketId": created["document"]["ticket"]["id"],
                "expectedRevision": created["document"]["ticket"]["revision"],
                "transition": {"to": "approved", "reason": "user requested approval"}
            }),
            lab.path(),
        )
        .await,
    );
    assert_eq!(approved["document"]["ticket"]["status"], "approved");

    let ordinary = response_json(
        call_ticket_tool(
            &server,
            &side.to_string(),
            "send_message_to_main",
            json!({"prompt": "ordinary advice without a ticket binding"}),
            lab.path(),
        )
        .await,
    );
    assert_eq!(ordinary["threadId"], main);
    assert!(ordinary.get("ticketId").is_none());
    assert_eq!(
        crate::ticket_store::TicketStore::test(lab.path())
            .read(1)?
            .ticket
            .status,
        crate::ticket_store::Status::Approved
    );

    let stale = call_ticket_tool(
        &server,
        &side.to_string(),
        "send_message_to_main",
        json!({
            "prompt": "stale ticket dispatch",
            "ticketContext": {
                "ticketId": approved["document"]["ticket"]["id"],
                "expectedRevision": approved["document"]["ticket"]["revision"].as_u64().unwrap() - 1,
                "implementation": true
            }
        }),
        lab.path(),
    )
    .await;
    assert!(!stale.success);

    let dispatched = response_json(
        call_ticket_tool(
            &server,
            &side.to_string(),
            "send_message_to_main",
            json!({
                "prompt": "implement and verify the approved ticket",
                "ticketContext": {
                    "ticketId": approved["document"]["ticket"]["id"],
                    "expectedRevision": approved["document"]["ticket"]["revision"],
                    "implementation": true
                }
            }),
            lab.path(),
        )
        .await,
    );
    assert_eq!(dispatched["threadId"], main);
    assert_eq!(dispatched["ticketStatus"], "in_progress");
    let implementation_turn = dispatched["turnId"].as_str().unwrap();
    let retry = response_json(
        call_ticket_tool(
            &server,
            &side.to_string(),
            "send_message_to_main",
            json!({
                "prompt": "exact retry must not start another turn",
                "ticketContext": {
                    "ticketId": approved["document"]["ticket"]["id"],
                    "expectedRevision": approved["document"]["ticket"]["revision"],
                    "implementation": true
                }
            }),
            lab.path(),
        )
        .await,
    );
    assert_eq!(retry["turnId"], implementation_turn);
    assert_eq!(retry["idempotent"], true);

    let wrong_turn = call_ticket_tool_at_turn(
        &server,
        &main,
        "unbound-main-turn",
        "report_ticket_ready",
        json!({}),
        lab.path(),
    )
    .await;
    assert!(!wrong_turn.success);
    assert_eq!(
        crate::ticket_store::TicketStore::test(lab.path())
            .read(1)?
            .ticket
            .status,
        crate::ticket_store::Status::InProgress
    );
    let ready = response_json(
        call_ticket_tool_at_turn(
            &server,
            &main,
            implementation_turn,
            "report_ticket_ready",
            json!({}),
            lab.path(),
        )
        .await,
    );
    assert_eq!(ready["ticketStatus"], "awaiting_review");
    assert_eq!(ready["idempotent"], false);
    let duplicate = response_json(
        call_ticket_tool_at_turn(
            &server,
            &main,
            implementation_turn,
            "report_ticket_ready",
            json!({}),
            lab.path(),
        )
        .await,
    );
    assert_eq!(duplicate["idempotent"], true);
    server.shutdown().await?;
    Ok(())
}

#[test]
fn paired_main_bridge_confines_target_to_side_metadata() -> color_eyre::Result<()> {
    std::thread::Builder::new()
        .name("paired-main-bridge-test".to_string())
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(paired_main_bridge_confines_target_scenario())
        })?
        .join()
        .map_err(|panic| color_eyre::eyre::eyre!("paired bridge test panicked: {panic:?}"))?
}

async fn paired_main_bridge_confines_target_scenario() -> color_eyre::Result<()> {
    let (codex_home, mut server, source, target) = test_server(json!({})).await?;
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await?;
    let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
    let (status_updates, _status_rx) = broadcast::channel(/*capacity*/ 8);
    server
        .start_dynamic_tool_mcp(
            config.clone(),
            AppEventSender::new(events_tx),
            status_updates,
        )
        .await?;
    let mut side_config = config.clone();
    side_config.strict_read_only_tools = true;
    let main = ThreadId::from_string(&target)?;
    let side = server
        .start_side_thread(
            &crate::local_settings::LocalSettings::from(&side_config),
            &side_config,
            main,
            /*selected_profile*/ None,
        )
        .await?
        .session
        .thread_id;

    let rejected = call_tool(
        &server,
        &source,
        "send_message_to_main",
        json!({"prompt": "must not select an arbitrary target"}),
    )
    .await;
    assert!(!rejected.success);

    let sent = response_json(
        call_tool(
            &server,
            &side.to_string(),
            "send_message_to_main",
            json!({"prompt": "paired-only advice"}),
        )
        .await,
    );
    assert_eq!(sent["threadId"], main.to_string());
    let forged_user_authorization = call_tool(
        &server,
        &side.to_string(),
        "send_message_to_main",
        json!({
            "prompt": "attempt to bypass approval",
            "userAuthorized": true
        }),
    )
    .await;
    assert!(!forged_user_authorization.success);
    let explicit_main_send = response_json(
        call_tool(
            &server,
            &side.to_string(),
            "send_message_to_main",
            json!({"prompt": "explicit slash-main advice"}),
        )
        .await,
    );
    assert_eq!(explicit_main_send["threadId"], main.to_string());

    let second_main = ThreadId::from_string(&source)?;
    let second_side = server
        .start_side_thread(
            &crate::local_settings::LocalSettings::from(&side_config),
            &side_config,
            second_main,
            /*selected_profile*/ None,
        )
        .await?
        .session
        .thread_id;
    server
        .thread_inject_items(
            second_main,
            vec![serde_json::from_value(json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "saved second MAIN history"}]
            }))?],
        )
        .await?;
    let second_sent = response_json(
        call_tool(
            &server,
            &second_side.to_string(),
            "send_message_to_main",
            json!({"prompt": "second paired advice"}),
        )
        .await,
    );
    assert_eq!(second_sent["threadId"], second_main.to_string());
    // Materialize rollouts for the freshly started MAIN/SIDE threads so the
    // restart below exercises cold resume from persisted identity.
    for thread_id in [side, second_side] {
        server
            .thread_inject_items(
                thread_id,
                vec![serde_json::from_value(json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "saved pair history"}]
                }))?],
            )
            .await?;
    }

    server.shutdown().await?;
    let mut server = crate::start_embedded_app_server_for_picker(&config).await?;
    let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
    let (status_updates, _status_rx) = broadcast::channel(/*capacity*/ 8);
    server
        .start_dynamic_tool_mcp(
            config.clone(),
            AppEventSender::new(events_tx),
            status_updates,
        )
        .await?;
    for thread_id in [main, second_main] {
        server
            .resume_thread(
                &crate::local_settings::LocalSettings::from(&config),
                config.clone(),
                thread_id,
                ResumeModelSettings::RestoreFromThread,
            )
            .await?;
    }
    for thread_id in [side, second_side] {
        server
            .resume_thread(
                &crate::local_settings::LocalSettings::from(&config),
                config.clone(),
                thread_id,
                ResumeModelSettings::RestoreFromThread,
            )
            .await?;
    }
    let resumed_sent = response_json(
        call_tool(
            &server,
            &side.to_string(),
            "send_message_to_main",
            json!({"prompt": "resumed paired advice"}),
        )
        .await,
    );
    assert_eq!(resumed_sent["threadId"], main.to_string());
    let resumed_second_sent = response_json(
        call_tool(
            &server,
            &second_side.to_string(),
            "send_message_to_main",
            json!({"prompt": "resumed second paired advice"}),
        )
        .await,
    );
    assert_eq!(resumed_second_sent["threadId"], second_main.to_string());

    let paired_spec = tool_specs()
        .into_iter()
        .find_map(|spec| match spec {
            DynamicToolSpec::Namespace(namespace) => namespace.tools.into_iter().find_map(|tool| {
                let DynamicToolNamespaceTool::Function(function) = tool;
                (function.name == "send_message_to_main").then_some(function)
            }),
            DynamicToolSpec::Function(_) => None,
        })
        .expect("paired MAIN tool");
    let properties = paired_spec.input_schema["properties"]
        .as_object()
        .expect("properties");
    assert!(properties.contains_key("prompt"));
    assert!(properties.contains_key("ticketContext"));
    assert!(!properties.contains_key("threadId"));
    assert_eq!(
        properties["ticketContext"]["properties"]["implementation"]["const"],
        true
    );
    let ready_spec = tool_specs()
        .into_iter()
        .find_map(|spec| match spec {
            DynamicToolSpec::Namespace(namespace) => namespace.tools.into_iter().find_map(|tool| {
                let DynamicToolNamespaceTool::Function(function) = tool;
                (function.name == "report_ticket_ready").then_some(function)
            }),
            DynamicToolSpec::Function(_) => None,
        })
        .expect("ticket completion tool");
    assert!(
        ready_spec.input_schema["properties"]
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
    );
    let side_tool_names = side_fallback_tool_specs()
        .into_iter()
        .flat_map(|spec| match spec {
            DynamicToolSpec::Function(function) => vec![function.name],
            DynamicToolSpec::Namespace(namespace) => namespace
                .tools
                .into_iter()
                .map(|tool| match tool {
                    DynamicToolNamespaceTool::Function(function) => function.name,
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    assert!(
        side_tool_names
            .iter()
            .any(|name| name == "read_workspace_file")
    );
    for ticket_tool in TICKET_TOOLS {
        assert!(
            side_tool_names.iter().any(|name| name == ticket_tool),
            "SIDE fallback must retain {ticket_tool}"
        );
    }
    for forbidden in [
        "create_thread",
        "send_message_to_thread",
        "send_message_to_main",
        "fork_thread",
    ] {
        assert!(!side_tool_names.iter().any(|name| name == forbidden));
    }
    let mut in_process_side = ThreadStartParams::default();
    crate::dynamic_tools_mcp::ThreadToolTransport::Disabled
        .configure_paired_side(&mut in_process_side);
    assert_eq!(in_process_side.dynamic_tools, None);

    server.shutdown().await?;
    Ok(())
}
