use super::*;
use crate::legacy_core::config::ConfigBuilder;
use crate::ticket_store::{ActorRole, Create, ImplementCheck, Proposal, Source, TicketStore};
use codex_protocol::ThreadId;

// Real loopback HTTP, real embedded app-server, isolated canonical store.
// No model calls or production daemon/session state are involved.
#[test]
fn concurrent_main_side_ticket_calls_through_one_mcp_endpoint() -> color_eyre::Result<()> {
    std::thread::Builder::new()
        .name("ticket-mcp-concurrency".into())
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(concurrent_ticket_calls())
        })?
        .join()
        .map_err(|_| color_eyre::eyre::eyre!("ticket MCP test panicked"))?
}

async fn concurrent_ticket_calls() -> color_eyre::Result<()> {
    let lab = tempfile::tempdir()?;
    std::fs::create_dir(lab.path().join("runtime"))?;
    std::fs::create_dir(lab.path().join(".git"))?;
    std::fs::write(lab.path().join("runtime/codex-upstream.toml"), "url='test'")?;
    let home = tempfile::tempdir()?;
    let mut config = ConfigBuilder::default()
        .codex_home(home.path().into())
        .build()
        .await?;
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(lab.path().to_path_buf())?;
    config.cwd = root.clone();
    config.workspace_roots = vec![root.clone()];
    let mut app = crate::start_embedded_app_server_for_picker(&config).await?;
    let (initial_events, _initial_rx) = tokio::sync::mpsc::unbounded_channel();
    let (initial_status, _) = broadcast::channel(8);
    app.start_dynamic_tool_mcp(
        config.clone(),
        AppEventSender::new(initial_events),
        initial_status,
    )
    .await?;
    let main = app.start_thread(&config).await?.session.thread_id;
    app.thread_inject_items(
        main,
        vec![serde_json::from_value(json!({
            "type":"message", "role":"assistant",
            "content":[{"type":"output_text","text":"test history"}]
        }))?],
    )
    .await?;
    let side = app
        .start_side_thread(
            &crate::local_settings::LocalSettings::from(&config),
            &config,
            main,
            None,
        )
        .await?
        .session
        .thread_id;
    app.thread_inject_items(
        side,
        vec![serde_json::from_value(json!({
            "type":"message", "role":"assistant",
            "content":[{"type":"output_text","text":"SIDE test history"}]
        }))?],
    )
    .await?;
    let store = TicketStore::test(lab.path());
    let source = Source {
        thread_id: side.to_string(),
        turn_id: "side-turn".into(),
        role: ActorRole::Side,
    };
    let doc = store.create(Create {
        title: "concurrency fixture".into(),
        scope: vec!["runtime".into()],
        source: source.clone(),
        dependencies: vec![],
        relevant_revisions: vec![],
        proposal: Proposal::default(),
    })?;
    let pending = match store.implement(
        doc.ticket.id,
        doc.ticket.revision,
        source,
        main.to_string(),
        "fixture approval".into(),
    )? {
        ImplementCheck::Pending(doc) => doc,
        _ => unreachable!(),
    };
    store.bind_assignment_delivery(
        doc.ticket.id,
        &pending.ticket.assignment.unwrap().notification_id,
        &main.to_string(),
        "main-turn",
    )?;
    let (events, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (statuses, _) = broadcast::channel(8);
    let bridge = DynamicToolMcpServer::start(
        app.request_handle(),
        ThreadStartParams {
            runtime_workspace_roots: Some(vec![root]),
            ..Default::default()
        },
        config.features.get().clone(),
        AppEventSender::new(events),
        statuses,
        None,
        ToolServices {
            task_tools: true,
            worktrees: None,
        },
    )
    .await?;
    let client = codex_http_client::HttpClientBuilder::new().build_direct()?;
    let url = bridge.config["url"].as_str().unwrap();
    let auth = bridge.config["http_headers"]["Authorization"]
        .as_str()
        .unwrap();
    let call = |id: u32, thread: ThreadId, tool: &str, args: Value| {
        client.post(url).header("Authorization", auth)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("MCP-Method", "tools/call").header("MCP-Name", tool)
            .json(&json!({"jsonrpc":"2.0", "id":id, "method":"tools/call", "params":{
                "name":tool, "arguments":args, "_meta":{
                    "threadId":thread.to_string(), "turnId":"direct-question-turn", "callId":format!("call-{id}"),
                    "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities":{}
                }
            }}))
    };
    // Hold the canonical store lock until both authenticated calls have been
    // admitted. A serialized endpoint or a blocking filesystem operation on
    // the async executor would fail this barrier instead of merely running
    // two reads sequentially and calling that concurrency.
    #[cfg(unix)]
    let held = {
        use std::os::fd::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(lab.path().join(".agents/tickets/.lock"))?;
        assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
        file
    };
    let first =
        tokio::spawn(call(1, main, "read_ticket", json!({"ticketId":doc.ticket.id})).send());
    let second =
        tokio::spawn(call(2, side, "read_ticket", json!({"ticketId":doc.ticket.id})).send());
    #[cfg(unix)]
    {
        let overlapped = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while bridge.workflow.active_call_count() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        drop(held);
        assert!(
            overlapped.is_ok(),
            "both callers must be in flight before either read finishes"
        );
    }
    let a = first.await?;
    let b = second.await?;
    for response in [a?, b?] {
        let body = response.text().await?;
        assert!(body.contains("concurrency fixture"), "{body}");
        assert!(!body.contains("\"isError\":true"), "{body}");
    }
    // Competing writes use the same canonical expected comment revision.
    // Exactly one commits, with an actionable conflict from the other.
    let args = json!({"ticketId":doc.ticket.id,"expectedCommentRevision":0,
        "specificationRevision":1,"category":"finding","body":"concurrent comment"});
    let (a, b) = tokio::join!(
        call(3, main, "append_ticket_comment", args.clone()).send(),
        call(4, side, "append_ticket_comment", args).send()
    );
    let bodies = [a?.text().await?, b?.text().await?];
    assert_eq!(
        bodies
            .iter()
            .filter(|b| b.contains("stale ticket revision"))
            .count(),
        1,
        "{bodies:?}"
    );
    assert_eq!(store.read(doc.ticket.id)?.comment_revision, 1);
    // A forged conflicting caller tuple fails without affecting the other caller.
    let bad = client.post(url).header("Authorization",auth)
        .header("Accept","application/json, text/event-stream")
        .header("MCP-Protocol-Version","2026-07-28")
        .header("MCP-Method","tools/call").header("MCP-Name","read_ticket")
        .json(&json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{
            "name":"read_ticket","arguments":{"ticketId":doc.ticket.id},"_meta":{
                "threadId":main.to_string(),"x-codex-turn-metadata":{"thread_id":side.to_string()},
                "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                "io.modelcontextprotocol/clientCapabilities":{}
            }
        }}));
    let (bad, good) = tokio::join!(
        bad.send(),
        call(6, side, "read_ticket", json!({"ticketId":doc.ticket.id})).send()
    );
    assert!(bad?.text().await?.contains("conflicting threadId"));
    assert!(good?.text().await?.contains("concurrency fixture"));
    let replay = call(6, side, "read_ticket", json!({"ticketId":doc.ticket.id}))
        .send()
        .await?
        .text()
        .await?;
    assert!(replay.contains("already consumed"), "{replay}");
    // Human browsing is repository-wide; the MCP model surface must remain
    // assignment-scoped for MAIN and repository-wide for authenticated SIDE.
    let create = |title: &str| {
        store
            .create(Create {
                title: title.into(),
                scope: vec!["runtime".into()],
                source: Source {
                    thread_id: side.to_string(),
                    turn_id: "fixture".into(),
                    role: ActorRole::Side,
                },
                dependencies: vec![],
                relevant_revisions: vec![],
                proposal: Proposal::default(),
            })
            .unwrap()
    };
    let unassigned = create("unassigned visibility fixture");
    let other = create("other MAIN visibility fixture");
    let other_main = ThreadId::new().to_string();
    let pending = match store.implement(
        other.ticket.id,
        1,
        other.ticket.source.clone(),
        other_main.clone(),
        "fixture".into(),
    )? {
        ImplementCheck::Pending(doc) => doc,
        _ => unreachable!(),
    };
    store.bind_assignment_delivery(
        other.ticket.id,
        &pending.ticket.assignment.unwrap().notification_id,
        &other_main,
        "other-turn",
    )?;
    let side_catalog = call(10, side, "list_tickets", json!({}))
        .send()
        .await?
        .text()
        .await?;
    for title in [
        "concurrency fixture",
        "unassigned visibility fixture",
        "other MAIN visibility fixture",
    ] {
        assert!(side_catalog.contains(title), "{side_catalog}");
    }
    let main_catalog = call(11, main, "list_tickets", json!({}))
        .send()
        .await?
        .text()
        .await?;
    assert!(
        main_catalog.contains("concurrency fixture"),
        "{main_catalog}"
    );
    assert!(
        !main_catalog.contains("unassigned visibility fixture"),
        "{main_catalog}"
    );
    assert!(
        !main_catalog.contains("other MAIN visibility fixture"),
        "{main_catalog}"
    );
    for (offset, ticket) in [unassigned, other].into_iter().enumerate() {
        let id = 20 + offset as u32 * 3;
        let read = call(
            id,
            main,
            "read_ticket",
            json!({"ticketId":ticket.ticket.id}),
        )
        .send()
        .await?
        .text()
        .await?;
        assert!(
            read.contains("not assigned to authenticated MAIN"),
            "{read}"
        );
        let mutation = call(id+1, main, "append_ticket_comment", json!({"ticketId":ticket.ticket.id,
            "expectedCommentRevision":0,"specificationRevision":1,"category":"finding","body":"must be denied"}))
            .send().await?.text().await?;
        assert!(mutation.contains("\"isError\":true"), "{mutation}");
        assert_eq!(store.read(ticket.ticket.id)?.comment_revision, 0);
        let read = call(
            id + 2,
            side,
            "read_ticket",
            json!({"ticketId":ticket.ticket.id}),
        )
        .send()
        .await?
        .text()
        .await?;
        assert!(read.contains(&ticket.ticket.title), "{read}");
    }
    assert_eq!(bridge.workflow.active_call_count(), 0);
    drop(bridge);
    app.shutdown().await?;
    Ok(())
}
