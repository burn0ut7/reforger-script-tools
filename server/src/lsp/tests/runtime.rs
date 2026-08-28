#[test]
fn external_index_progress_is_published_as_a_client_notification() {
    let mut server = LspServer::new(Vec::new(), LspServerOptions::default());

    server
        .handle_internal_event(ServerEvent::ExternalIndexProgress {
            phase: "source-rebuild-start".to_string(),
        })
        .unwrap();
    assert!(
        server.writer.is_empty(),
        "progress must wait until the client has installed its notification handler"
    );
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "initialized",
                "params": {},
            }),
            None,
            0,
            0,
        )
        .unwrap();

    let messages = read_test_messages(&server.writer);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["method"], "reforger/externalIndexProgress");
    assert_eq!(
        messages[0]["params"]["phase"],
        serde_json::Value::String("source-rebuild-start".to_string())
    );

    server.writer.clear();
    server
        .handle_internal_event(ServerEvent::ExternalIndexChanged)
        .unwrap();
    let messages = read_test_messages(&server.writer);
    assert_eq!(messages[0]["method"], "reforger/externalIndexProgress");
    assert_eq!(messages[0]["params"]["phase"], "complete");
    assert_eq!(messages[0]["params"]["status"], "missing");
}

#[test]
fn pending_hover_returns_only_current_lexical_facts_after_semantic_skip() {
    let mut server = LspServer::new(Vec::new(), LspServerOptions::default());
    let uri = "file:///Scripts/Skipped.c";
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": { "textDocument": {
                    "uri": uri,
                    "languageId": "enforce",
                    "version": 1,
                    "text": "class Skipped {}"
                }}
            }),
            None,
            0,
            0,
        )
        .unwrap();
    let task = server
        .document_runtime
        .test_admit_task(uri, TaskClass::Semantic);
    assert!(server.document_runtime.test_install_current_foreground(uri));
    server
        .handle_internal_event(ServerEvent::DocumentAnalysisSkipped {
            task: task.identity().clone(),
            reason: "superseded-during-analysis".to_string(),
            elapsed_ms: 0,
        })
        .unwrap();
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "textDocument/hover",
                "params": {
                    "textDocument": { "uri": uri },
                    "position": { "line": 0, "character": 0 }
                }
            }),
            None,
            0,
            0,
        )
        .unwrap();

    let output = String::from_utf8(server.writer).unwrap();
    let mut reader = BufReader::new(output.as_bytes());
    let response = loop {
        let message = read_message(&mut reader).unwrap();
        let message = message.expect("hover response");
        if message["id"] == 1 {
            break message;
        }
    };
    assert_eq!(response["id"], 1);
    assert_eq!(
        response["result"]["contents"]["value"],
        "**Keyword**\n\n```enforce\nclass\n```"
    );
    assert!(response.get("error").is_none());
}

#[test]
fn one_cpu_capacity_reserves_execution_for_foreground() {
    assert_eq!(
        RuntimeWorkCapacity::for_logical_cpus(1),
        RuntimeWorkCapacity {
            foreground_workers: 1,
            background_workers: 0,
        }
    );
    assert_eq!(
        RuntimeWorkCapacity::for_logical_cpus(2),
        RuntimeWorkCapacity {
            foreground_workers: 1,
            background_workers: 1,
        }
    );
}

#[test]
fn one_cpu_foreground_lane_advances_background_only_when_foreground_is_idle() {
    let now = Instant::now();
    let mut runtime = AnalysisRuntime::new(AdmissionLimits::new(2, 64));
    let mut pending = BTreeMap::new();
    let semantic = semantic_analysis_job(&mut runtime, "file:///semantic.c", 1, now);
    let foreground = foreground_document_job(
        &mut runtime,
        "file:///foreground.c",
        1,
        "class Fresh {}",
        now,
    );
    pending.insert(
        (TaskClass::Semantic, "file:///semantic.c".to_string()),
        RuntimeWorkJob::Semantic(semantic),
    );
    assert_eq!(
        next_runnable_work_key_for_lane(
            &pending,
            RuntimeWorkerLane::ForegroundWithIdleBackground,
        ),
        Some((TaskClass::Semantic, "file:///semantic.c".to_string()))
    );

    pending.insert(
        (TaskClass::Foreground, "file:///foreground.c".to_string()),
        RuntimeWorkJob::Foreground(foreground),
    );
    assert_eq!(
        next_runnable_work_key_for_lane(
            &pending,
            RuntimeWorkerLane::ForegroundWithIdleBackground,
        ),
        Some((TaskClass::Foreground, "file:///foreground.c".to_string()))
    );
}

#[test]
fn shared_executor_prioritizes_ready_semantic_work_over_ready_rich_work() {
    let now = Instant::now();
    let mut runtime = AnalysisRuntime::new(AdmissionLimits::new(2, 2));
    let mut pending = BTreeMap::new();
    let rich = rich_semantic_tokens_job(&mut runtime, "file:///rich.c", 1, now);
    let semantic = semantic_analysis_job(&mut runtime, "file:///semantic.c", 1, now);
    pending.insert(
        (TaskClass::Rich, "file:///rich.c".to_string()),
        RuntimeWorkJob::Rich(rich),
    );
    pending.insert(
        (TaskClass::Semantic, "file:///semantic.c".to_string()),
        RuntimeWorkJob::Semantic(semantic),
    );

    assert_eq!(
        next_runnable_work_key(&pending),
        Some((TaskClass::Semantic, "file:///semantic.c".to_string()))
    );
}

#[test]
fn foreground_worker_completes_while_background_semantic_work_is_in_flight() {
    let (event_sender, event_receiver) = mpsc::channel();
    let (semantic_started_sender, semantic_started_receiver) = mpsc::channel();
    let (release_semantic_sender, release_semantic_receiver) = mpsc::channel();
    let release_semantic_receiver = Arc::new(Mutex::new(Some(release_semantic_receiver)));
    let hook_release = release_semantic_receiver.clone();
    let scheduler = RuntimeWorkExecutor::start_with_capacity_and_test_hook(
        event_sender,
        RuntimeWorkCapacity::for_logical_cpus(2),
        Arc::new(move |class| {
            if class == TaskClass::Semantic {
                semantic_started_sender
                    .send(())
                    .expect("test waits for semantic start");
                hook_release
                    .lock()
                    .unwrap()
                    .take()
                    .expect("semantic hook runs once")
                    .recv()
                    .expect("test releases semantic work");
            }
        }),
    );
    let mut runtime = AnalysisRuntime::new(AdmissionLimits::new(8, 8 * 1024));
    let now = Instant::now();
    scheduler.schedule(semantic_analysis_job(
        &mut runtime,
        "file:///background.c",
        1,
        now,
    ));
    semantic_started_receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("background worker began semantic work");

    scheduler.schedule_foreground(foreground_document_job(
        &mut runtime,
        "file:///foreground.c",
        1,
        "class Fresh {}",
        Instant::now(),
    ));

    // The semantic hook cannot finish until after this assertion.  The
    // foreground-ready event therefore proves the reserved foreground
    // worker made progress independently, without wall-clock assertions.
    assert!(matches!(
        event_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("foreground worker event"),
        ServerEvent::ForegroundDocumentReady { task, .. }
            if task.uri() == "file:///foreground.c"
    ));
    release_semantic_sender
        .send(())
        .expect("release blocked semantic worker");
}

#[test]
fn completion_labels_overloads_and_uses_source_rank_as_tiebreaker() {
    let source = r#"class Widget
{
	void SetVisible(bool visible);
	void SetVisible(bool visible, bool animate);
}

class Example
{
	void Run()
	{
		Widget widget;
		widget.Set
	}
}
"#;
    let external = file_index_for_source(
        r#"class Widget
{
	void SetText(string text);
}
"#,
    )
    .index;

    let report = completion_report_for_source_position_with_external(
        source,
        position_after_needle(source, "widget.Set"),
        Some(&external),
    );

    let overload_details = report
        .list
        .items
        .iter()
        .filter(|item| item.label == "SetVisible")
        .filter_map(|item| {
            item.label_details
                .as_ref()
                .and_then(|details| details.detail.as_deref())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        overload_details,
        vec!["(bool visible)", "(bool visible, bool animate)"]
    );
    let first = report.list.items.first().unwrap();
    assert_eq!(first.label, "SetText");
    assert!(first
        .sort_text
        .as_deref()
        .unwrap_or("")
        .starts_with("014:01:"));
}

#[test]
fn position_index_stops_when_cancellation_arrives_mid_build() {
    let source = "field ".repeat(256);
    let checks = Cell::new(0usize);

    let index = LspPositionIndex::new_cancellable(
        &source,
        Some(&|| {
            checks.set(checks.get() + 1);
            checks.get() >= 2
        }),
    );

    assert!(index.is_none());
}

#[test]
fn channel_runtime_coalesces_contiguous_full_sync_changes_before_outline_request() {
    let uri = "file:///Scripts/Coalesced.c";
    let log_path = test_log_path("coalesced_channel_changes");
    let (event_sender, event_receiver) = mpsc::channel();
    let mut server = LspServer::new(
        Vec::new(),
        LspServerOptions {
            workbench_wine_prefix: None,
            external_index_mode: ExternalIndexMode::Loaded,
            log_path: Some(log_path.clone()),
            ..LspServerOptions::default()
        },
    );
    let send = |value| {
        event_sender
            .send(ServerEvent::Incoming {
                received_at: Instant::now(),
                result: Ok(value),
            })
            .unwrap();
    };
    send(json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": { "textDocument": {
            "uri": uri,
            "languageId": "enforce",
            "version": 1,
            "text": "class Initial {}"
        }}
    }));
    for (version, name) in [(2, "Second"), (3, "Third")] {
        send(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": format!("class {name} {{}}") }]
            }
        }));
    }
    send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "textDocument/documentSymbol",
        "params": { "textDocument": { "uri": uri } }
    }));
    send(json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didChange",
        "params": {
            "textDocument": { "uri": uri, "version": 4 },
            "contentChanges": [{ "text": "class Current {}" }]
        }
    }));
    send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "textDocument/documentSymbol",
        "params": { "textDocument": { "uri": uri } }
    }));
    drop(event_sender);

    server.run_message_channels(event_receiver).unwrap();

    let output = String::from_utf8(server.writer).unwrap();
    assert!(output.contains("\"id\":1"));
    assert!(output.contains("\"id\":2"));
    assert!(output.contains("\"name\":\"Third\""));
    assert!(output.contains("\"name\":\"Current\""));
    assert!(!output.contains("\"name\":\"Second\""));

    let log = std::fs::read_to_string(&log_path).unwrap();
    assert_eq!(log.matches("notification didChange uri=").count(), 2);
    assert!(log.contains("version=3"));
    assert!(log.contains("version=4"));
    assert!(log.contains("coalesced_changes=2 superseded_changes=1"));
    cleanup_log(&log_path);
}

#[test]
fn channel_runtime_worker_results_do_not_wait_for_unrelated_incoming_messages() {
    let uri = "file:///Scripts/EventDrivenRuntime.c";
    let (event_sender, event_receiver) = mpsc::channel();
    let (foreground_started_sender, foreground_started_receiver) = mpsc::channel();
    let (release_foreground_sender, release_foreground_receiver) = mpsc::channel();
    let release_foreground_receiver = Arc::new(Mutex::new(Some(release_foreground_receiver)));
    let hook_release = release_foreground_receiver.clone();
    let scheduler = RuntimeWorkExecutor::start_with_capacity_and_test_hook(
        event_sender.clone(),
        RuntimeWorkCapacity::for_logical_cpus(2),
        Arc::new(move |class| {
            if class == TaskClass::Foreground {
                foreground_started_sender
                    .send(())
                    .expect("test waits for foreground start");
                hook_release
                    .lock()
                    .unwrap()
                    .take()
                    .expect("foreground hook runs once")
                    .recv()
                    .expect("test releases foreground work");
            }
        }),
    );
    let mut server = LspServer::new_with_runtime_senders(
        Vec::new(),
        LspServerOptions::default(),
        Some(scheduler),
        None,
    );
    event_sender
        .send(ServerEvent::Incoming {
            received_at: Instant::now(),
            result: Ok(json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": { "textDocument": {
                    "uri": uri,
                    "languageId": "enforce",
                    "version": 1,
                    "text": "class EventDrivenRuntime { void Run() {} }"
                }}
            })),
        })
        .unwrap();

    let control = thread::spawn(move || {
        foreground_started_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("foreground worker starts");
        // Release the result after the coordinator has returned to its
        // blocking receive. An unrelated incoming message must not be needed
        // to make foreground -> semantic -> rich publication advance.
        thread::sleep(Duration::from_millis(10));
        release_foreground_sender
            .send(())
            .expect("release foreground worker");
        thread::sleep(Duration::from_millis(50));
        for message in [
            json!({"jsonrpc": "2.0", "id": 1, "method": "shutdown"}),
            json!({"jsonrpc": "2.0", "method": "exit"}),
        ] {
            event_sender
                .send(ServerEvent::Incoming {
                    received_at: Instant::now(),
                    result: Ok(message),
                })
                .expect("send lifecycle message");
        }
    });

    server.run_message_channels(event_receiver).unwrap();
    control.join().unwrap();

    let output = String::from_utf8(server.writer).unwrap();
    assert!(
        output.contains("\"method\":\"workspace/semanticTokens/refresh\""),
        "worker publication waited for the unrelated shutdown message: {output}"
    );
    assert!(
        output.contains("\"method\":\"reforger/foregroundReady\""),
        "foreground publication must wake the scope-decoration bridge: {output}"
    );
}

#[test]
fn document_analysis_scheduler_keeps_only_latest_pending_revision() {
    let (sender, receiver) = mpsc::channel();
    let scheduler = RuntimeWorkExecutor::start(sender);
    let mut runtime = AnalysisRuntime::new(AdmissionLimits::new(4, 1024));
    assert_eq!(
        runtime.upsert("file:///Scripts/Pending.c", 2, "class Old {}"),
        UpsertOutcome::Accepted
    );
    let old = match runtime.admit(
        TaskClass::Semantic,
        runtime.latest("file:///Scripts/Pending.c").unwrap(),
        1,
    ) {
        AdmissionDisposition::Enqueued { .. } => runtime.take_next().unwrap(),
        _ => unreachable!(),
    };
    assert_eq!(
        runtime.upsert("file:///Scripts/Pending.c", 3, "class Current {}"),
        UpsertOutcome::Accepted
    );
    let current = match runtime.admit(
        TaskClass::Semantic,
        runtime.latest("file:///Scripts/Pending.c").unwrap(),
        2,
    ) {
        AdmissionDisposition::Enqueued { .. } => runtime.take_next().unwrap(),
        _ => unreachable!(),
    };
    scheduler.schedule(OpenDocumentAnalysisJob {
        task: old,
        scheduled_at: Instant::now(),
    });
    scheduler.schedule(OpenDocumentAnalysisJob {
        task: current,
        scheduled_at: Instant::now(),
    });

    let event = (0..2)
        .filter_map(|_| receiver.recv_timeout(Duration::from_secs(2)).ok())
        .find(|event| matches!(event, ServerEvent::DocumentAnalysisReady { .. }))
        .expect("latest analysis result");
    assert!(
        matches!(event, ServerEvent::DocumentAnalysisReady { task, .. } if task.revision() == 2)
    );
}

#[test]
fn semantic_tokens_wait_for_current_analysis_instead_of_publishing_a_lexical_flash() {
    let (sender, receiver) = mpsc::channel();
    let scheduler = RuntimeWorkExecutor::start(sender);
    let mut server = LspServer::new_with_runtime_senders(
        Vec::new(),
        LspServerOptions::default(),
        Some(scheduler),
        None,
    );
    let uri = "file:///Scripts/PendingTokens.c";
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": { "textDocument": {
                    "uri": uri,
                    "languageId": "enforce",
                    "version": 1,
                    "text": "// pending\r\nclass Pending {}"
                }}
            }),
            None,
            0,
            0,
        )
        .unwrap();
    assert!(
        !server
            .document_runtime
            .test_document_state(uri)
            .unwrap()
            .analysis_ready
    );

    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "textDocument/semanticTokens/full",
                "params": { "textDocument": { "uri": uri } }
            }),
            None,
            0,
            0,
        )
        .unwrap();

    let output = String::from_utf8_lossy(&server.writer);
    assert!(
        !output.contains("\"id\":1"),
        "publishing a lexical response clears settled semantic colors: {output}"
    );

    let mut rich_response = false;
    for _ in 0..4 {
        let event = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("foreground, analysis, rich worker, then deferred response");
        server.handle_internal_event(event).unwrap();
        let output = String::from_utf8_lossy(&server.writer);
        rich_response =
            output.contains("\"id\":1") && output.contains("\"resultId\":\"reforger:1:rich:");
        if rich_response {
            break;
        }
    }
    assert!(
        rich_response,
        "the pending request must complete from current rich analysis: {}",
        String::from_utf8_lossy(&server.writer)
    );
    assert!(
        !String::from_utf8_lossy(&server.writer).contains("\"resultId\":\"reforger:1:lexical\"")
    );

    server.writer.clear();
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": { "uri": uri, "version": 2 },
                    "contentChanges": [{
                        "text": "// edited while typing\r\nclass Pending { void Run() {} }"
                    }]
                }
            }),
            None,
            0,
            0,
        )
        .unwrap();
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "textDocument/semanticTokens/full",
                "params": { "textDocument": { "uri": uri } }
            }),
            None,
            0,
            0,
        )
        .unwrap();

    let output = String::from_utf8_lossy(&server.writer);
    assert!(
        !output.contains("\"id\":2"),
        "an edit must not publish its lexical cache while rich analysis is pending: {output}"
    );
    assert!(!output.contains("\"resultId\":\"reforger:2:lexical\""));

    let mut edited_rich_response = false;
    for _ in 0..4 {
        let event = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("edited foreground, analysis, rich worker, then deferred response");
        server.handle_internal_event(event).unwrap();
        let output = String::from_utf8_lossy(&server.writer);
        edited_rich_response =
            output.contains("\"id\":2") && output.contains("\"resultId\":\"reforger:2:rich:");
        if edited_rich_response {
            break;
        }
    }
    assert!(
        edited_rich_response,
        "the edited request must complete from the current rich projection: {}",
        String::from_utf8_lossy(&server.writer)
    );
    assert!(
        !String::from_utf8_lossy(&server.writer).contains("\"resultId\":\"reforger:2:lexical\"")
    );
}

#[test]
fn rich_semantic_tokens_converge_before_the_first_editor_request() {
    let (sender, receiver) = mpsc::channel();
    let scheduler = RuntimeWorkExecutor::start(sender);
    let mut server = LspServer::new_with_runtime_senders(
        Vec::new(),
        LspServerOptions::default(),
        Some(scheduler),
        None,
    );
    let uri = "file:///Scripts/ProactiveTokens.c";
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": { "textDocument": {
                    "uri": uri,
                    "languageId": "enforce",
                    "version": 1,
                    "text": "class ProactiveTokens { void Run() {} }"
                }}
            }),
            None,
            0,
            0,
        )
        .unwrap();

    for expected in ["foreground", "semantic analysis", "rich semantic tokens"] {
        server
            .handle_internal_event(
                receiver
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap_or_else(|_| panic!("missing {expected} worker result")),
            )
            .unwrap();
    }

    assert!(
        server
            .document_runtime
            .test_document_state(uri)
            .unwrap()
            .rich_semantic_tokens,
        "the current rich projection must converge without waiting for an editor request"
    );
    assert!(
        String::from_utf8_lossy(&server.writer)
            .contains("\"method\":\"workspace/semanticTokens/refresh\""),
        "proactive convergence must ask the editor to collect the ready projection"
    );

    server.writer.clear();
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "textDocument/semanticTokens/full",
                "params": { "textDocument": { "uri": uri } }
            }),
            None,
            0,
            0,
        )
        .unwrap();

    let output = String::from_utf8_lossy(&server.writer);
    assert!(output.contains("\"id\":1"), "{output}");
    assert!(output.contains("\"resultId\":\"reforger:1:rich:"), "{output}");
}

#[test]
fn pending_semantic_tokens_receive_content_modified_when_typing_supersedes_the_revision() {
    let (sender, _receiver) = mpsc::channel();
    let scheduler = RuntimeWorkExecutor::start(sender);
    let mut server = LspServer::new_with_runtime_senders(
        Vec::new(),
        LspServerOptions::default(),
        Some(scheduler),
        None,
    );
    let uri = "file:///Scripts/SupersededTokens.c";
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": { "textDocument": {
                    "uri": uri,
                    "languageId": "enforce",
                    "version": 1,
                    "text": "class First {}"
                }}
            }),
            None,
            0,
            0,
        )
        .unwrap();
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "textDocument/semanticTokens/full",
                "params": { "textDocument": { "uri": uri } }
            }),
            None,
            0,
            0,
        )
        .unwrap();
    assert!(!String::from_utf8_lossy(&server.writer).contains("\"id\":1"));

    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": { "uri": uri, "version": 2 },
                    "contentChanges": [{ "text": "class Second {}" }]
                }
            }),
            None,
            0,
            0,
        )
        .unwrap();

    let output = String::from_utf8_lossy(&server.writer);
    assert!(output.contains("\"id\":1"), "{output}");
    assert!(output.contains("\"code\":-32801"), "{output}");
    assert!(
        output.contains("\"message\":\"Content modified\""),
        "{output}"
    );
    assert!(!output.contains("\"resultId\":\"reforger:1:lexical\""));
}

#[test]
fn completion_waits_for_current_analysis_before_rendering_callable_parameters() {
    let (sender, receiver) = mpsc::channel();
    let scheduler = RuntimeWorkExecutor::start(sender);
    let mut server = LspServer::new_with_runtime_senders(
        Vec::new(),
        LspServerOptions::default(),
        Some(scheduler),
        None,
    );
    let uri = "file:///Scripts/PendingCallableCompletion.c";
    let source = "class Example { void TestNumFun2(int input, int num) {} void Run() { TestNumFun } }";
    let offset = source.find("TestNumFun }").unwrap() + "TestNumFun".len();
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": { "textDocument": {
                    "uri": uri,
                    "languageId": "enforce",
                    "version": 1,
                    "text": source
                }}
            }),
            None,
            0,
            0,
        )
        .unwrap();
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "textDocument/completion",
                "params": {
                    "textDocument": { "uri": uri },
                    "position": { "line": 0, "character": offset }
                }
            }),
            None,
            0,
            0,
        )
        .unwrap();

    assert!(server.writer.is_empty(), "pending completion must not race analysis");
    server
        .handle_internal_event(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("foreground result"),
        )
        .unwrap();
    server
        .handle_internal_event(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("semantic analysis result"),
        )
        .unwrap();
    let output = String::from_utf8(server.writer).unwrap();
    assert!(output.contains("\"id\":1"));
    assert!(output.contains("\"newText\":\"TestNumFun2(${1:input}, ${2:num})\""), "{output}");
}

#[test]
fn completion_returns_preprocessor_directives_after_foreground_publication() {
    let (sender, receiver) = mpsc::channel();
    let scheduler = RuntimeWorkExecutor::start(sender);
    let mut server = LspServer::new_with_runtime_senders(
        Vec::new(),
        LspServerOptions::default(),
        Some(scheduler),
        None,
    );
    let uri = "file:///Scripts/PendingPreprocessorCompletion.c";
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": { "textDocument": {
                    "uri": uri,
                    "languageId": "enforce",
                    "version": 1,
                    "text": "class PendingPreprocessor : ScriptComponent\n{\n\tint value = 1;\n\tRplChannel channel = RplChannel.Reliable;\n\t#\n}"
                }}
            }),
            None,
            0,
            0,
        )
        .unwrap();
    server
        .handle_internal_event(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("foreground result"),
        )
        .unwrap();
    let state = server.document_runtime.test_document_state(uri).unwrap();
    assert!(state.foreground_ready);
    assert!(!state.analysis_ready);
    server.writer.clear();
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "textDocument/completion",
                "params": {
                    "textDocument": { "uri": uri },
                    "position": { "line": 4, "character": 2 }
                }
            }),
            None,
            0,
            0,
        )
        .unwrap();

    assert!(server.writer.is_empty(), "pending completion must wait for analysis");
    server
        .handle_internal_event(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("semantic analysis result"),
        )
        .unwrap();
    let output = String::from_utf8(server.writer).unwrap();
    for directive in ["#define", "#ifdef", "#ifndef", "#else", "#endif"] {
        assert!(output.contains(&format!("\"label\":\"{directive}\"")), "{directive}");
    }
}

#[test]
fn pending_signature_help_uses_only_the_current_unique_simple_callable() {
    let (sender, receiver) = mpsc::channel();
    let scheduler = RuntimeWorkExecutor::start(sender);
    let mut server = LspServer::new_with_runtime_senders(
        Vec::new(),
        LspServerOptions::default(),
        Some(scheduler),
        None,
    );
    let uri = "file:///Scripts/PendingSignature.c";
    let initial = "class Example { void Stale(int oldValue) {} void Test() { Stale( } }";
    let current =
        "class Example { void Current(string currentValue) {} void Test() { Current(\"\", ); } }";
    let current_position = position_after_needle(current, "Current(\"\", ");
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": { "textDocument": {
                    "uri": uri,
                    "languageId": "enforce",
                    "version": 1,
                    "text": initial
                }}
            }),
            None,
            0,
            0,
        )
        .unwrap();
    install_next_foreground(&mut server, &receiver);
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": { "uri": uri, "version": 2 },
                    "contentChanges": [{ "text": current }]
                }
            }),
            None,
            0,
            0,
        )
        .unwrap();
    assert!(!server.document_runtime.test_document_state(uri).unwrap().analysis_ready);
    install_next_foreground(&mut server, &receiver);

    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "textDocument/signatureHelp",
                "params": {
                    "textDocument": { "uri": uri },
                    "position": current_position
                }
            }),
            None,
            0,
            0,
        )
        .unwrap();

    let output = String::from_utf8(server.writer).unwrap();
    let mut reader = BufReader::new(output.as_bytes());
    let response = loop {
        let response = read_message(&mut reader)
            .unwrap()
            .expect("pending signature response");
        if response["id"] == 1 {
            break response;
        }
    };
    assert_eq!(response["id"], 1);
    assert_eq!(
        response["result"]["signatures"][0]["label"], "void Current(string currentValue)",
        "response={response}"
    );
    assert!(!response.to_string().contains("Stale"));
}

#[test]
fn pending_signature_help_rejects_ambiguous_or_member_calls() {
    let (sender, _receiver) = mpsc::channel();
    let scheduler = RuntimeWorkExecutor::start(sender);
    let mut server = LspServer::new_with_runtime_senders(
        Vec::new(),
        LspServerOptions::default(),
        Some(scheduler),
        None,
    );
    let uri = "file:///Scripts/PendingSignatureAmbiguous.c";
    let source = "class First { void Run(int value) {} } class Second { void Run(string value) {} } class Example { void Test(First receiver) { Run( ); receiver.Run( ); } }";
    let ambiguous_position = position_after_needle(source, "Run( ");
    let member_position = position_after_needle(source, "receiver.Run( ");
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": { "textDocument": {
                    "uri": uri,
                    "languageId": "enforce",
                    "version": 1,
                    "text": source
                }}
            }),
            None,
            0,
            0,
        )
        .unwrap();
    for (id, position) in [(1, ambiguous_position), (2, member_position)] {
        server
            .handle_message(
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "textDocument/signatureHelp",
                    "params": { "textDocument": { "uri": uri }, "position": position }
                }),
                None,
                0,
                0,
            )
            .unwrap();
    }
    let output = String::from_utf8(server.writer).unwrap();
    let mut reader = BufReader::new(output.as_bytes());
    let mut responses = BTreeMap::new();
    while responses.len() < 2 {
        let response = read_message(&mut reader)
            .unwrap()
            .expect("pending signature response");
        if let Some(id) = response["id"].as_i64() {
            responses.insert(id, response);
        }
    }
    for id in [1, 2] {
        let response = responses.remove(&id).unwrap();
        assert_eq!(response["id"], id);
        assert!(response["result"].is_null());
    }
}

#[test]
fn pending_definition_returns_only_a_current_snapshot_declaration_target() {
    let (sender, receiver) = mpsc::channel();
    let scheduler = RuntimeWorkExecutor::start(sender);
    let mut server = LspServer::new_with_runtime_senders(
        Vec::new(),
        LspServerOptions::default(),
        Some(scheduler),
        None,
    );
    let uri = "file:///Scripts/PendingDefinition.c";
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": { "textDocument": {
                    "uri": uri,
                    "languageId": "enforce",
                    "version": 1,
                    "text": "class Previous {}"
                }}
            }),
            None,
            0,
            0,
        )
        .unwrap();
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": { "uri": uri, "version": 2 },
                    "contentChanges": [{ "text": "class Current {}\nCurrent value;" }]
                }
            }),
            None,
            0,
            0,
        )
        .unwrap();
    install_next_foreground(&mut server, &receiver);

    for (id, line, character) in [(1, 0, 6), (2, 1, 0)] {
        server
            .handle_message(
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "textDocument/definition",
                    "params": {
                        "textDocument": { "uri": uri },
                        "position": { "line": line, "character": character }
                    }
                }),
                None,
                0,
                0,
            )
            .unwrap();
    }

    let output = String::from_utf8(server.writer).unwrap();
    let mut reader = BufReader::new(output.as_bytes());
    let mut first = None;
    let mut second = None;
    while first.is_none() || second.is_none() {
        let message = read_message(&mut reader)
            .unwrap()
            .expect("definition response");
        match message["id"].as_i64() {
            Some(1) => first = Some(message),
            Some(2) => second = Some(message),
            _ => {}
        }
    }
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first["id"], 1);
    assert_eq!(first["result"][0]["targetUri"], uri);
    assert_eq!(
        first["result"][0]["targetSelectionRange"]["start"],
        json!({ "line": 0, "character": 6 })
    );
    assert_eq!(second["id"], 2);
    assert_eq!(second["result"], json!([]));
}

#[test]
fn completion_waits_for_current_analysis_before_resolving_receiver() {
    let (sender, receiver) = mpsc::channel();
    let scheduler = RuntimeWorkExecutor::start(sender);
    let mut server = LspServer::new_with_runtime_senders(
        Vec::new(),
        LspServerOptions::default(),
        Some(scheduler),
        None,
    );
    let uri = "file:///Scripts/PendingReceiverCompletion.c";
    let source = "class Widget { void GetName() {} } class Example { void Run(Widget parameter) { parameter.Get } }";
    let offset = source.find("parameter.Get").unwrap() + "parameter.Get".len();
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": { "textDocument": {
                    "uri": uri,
                    "languageId": "enforce",
                    "version": 1,
                    "text": source
                }}
            }),
            None,
            0,
            0,
        )
        .unwrap();
    assert!(!server.document_runtime.test_document_state(uri).unwrap().foreground_ready);
    assert!(!server.document_runtime.test_document_state(uri).unwrap().analysis_ready);
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "textDocument/completion",
                "params": {
                    "textDocument": { "uri": uri },
                    "position": { "line": 0, "character": offset }
                }
            }),
            None,
            0,
            0,
        )
        .unwrap();

    assert!(server.writer.is_empty(), "pending completion must not race analysis");
    server
        .handle_internal_event(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("foreground result"),
        )
        .unwrap();
    server
        .handle_internal_event(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("semantic analysis result"),
        )
        .unwrap();
    let output = String::from_utf8(server.writer).unwrap();
    assert!(output.contains("\"id\":1"));
    assert!(output.contains("\"label\":\"GetName\""));
}

#[test]
fn completion_waits_for_current_analysis_before_returning_argument_labels() {
    let (sender, receiver) = mpsc::channel();
    let scheduler = RuntimeWorkExecutor::start(sender);
    let mut server = LspServer::new_with_runtime_senders(
        Vec::new(),
        LspServerOptions::default(),
        Some(scheduler),
        None,
    );
    let uri = "file:///Scripts/PendingArgumentCompletion.c";
    let source = "class Example { void Run(int firstValue, string secondValue) {} void Test() { Run(sec); } }";
    let offset = source.find("Run(sec").unwrap() + "Run(sec".len();
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": { "textDocument": {
                    "uri": uri,
                    "languageId": "enforce",
                    "version": 1,
                    "text": source
                }}
            }),
            None,
            0,
            0,
        )
        .unwrap();
    server
        .handle_internal_event(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("foreground result"),
        )
        .unwrap();
    server.writer.clear();
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "textDocument/completion",
                "params": {
                    "textDocument": { "uri": uri },
                    "position": { "line": 0, "character": offset }
                }
            }),
            None,
            0,
            0,
        )
        .unwrap();

    assert!(server.writer.is_empty(), "pending completion must wait for analysis");
    server
        .handle_internal_event(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("semantic analysis result"),
        )
        .unwrap();
    let output = String::from_utf8(server.writer).unwrap();
    assert!(output.contains("\"id\":1"));
    assert!(output.contains("\"label\":\"secondValue\""));
}

#[test]
fn pending_analysis_publishes_current_parser_diagnostics_before_worker_publication() {
    let (sender, receiver) = mpsc::channel();
    let scheduler = RuntimeWorkExecutor::start(sender);
    let mut server = LspServer::new_with_runtime_senders(
        Vec::new(),
        LspServerOptions::default(),
        Some(scheduler),
        None,
    );
    let uri = "file:///Scripts/PendingDiagnostics.c";
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": { "textDocument": {
                    "uri": uri,
                    "languageId": "enforce",
                    "version": 1,
                    "text": "class {"
                }}
            }),
            None,
            0,
            0,
        )
        .unwrap();
    server
        .handle_internal_event(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("foreground result"),
        )
        .unwrap();

    let output = String::from_utf8(server.writer).unwrap();
    assert!(output.contains("\"method\":\"textDocument/publishDiagnostics\""));
    assert!(output.contains("\"version\":1"));
    assert!(output.contains("Reforger Script Tools parser"));
    assert!(output.contains("\"diagnostics\":[{"));
}

#[test]
fn pending_analysis_clears_repaired_parser_diagnostics_before_worker_publication() {
    let (sender, receiver) = mpsc::channel();
    let scheduler = RuntimeWorkExecutor::start(sender);
    let mut server = LspServer::new_with_runtime_senders(
        Vec::new(),
        LspServerOptions::default(),
        Some(scheduler),
        None,
    );
    let uri = "file:///Scripts/PendingDiagnosticsRepair.c";
    for (version, text) in [(1, "class {"), (2, "class Repaired {}")] {
        let method = if version == 1 {
            "textDocument/didOpen"
        } else {
            "textDocument/didChange"
        };
        let params = if version == 1 {
            json!({ "textDocument": {
                "uri": uri, "languageId": "enforce", "version": version, "text": text
            }})
        } else {
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }]
            })
        };
        server
            .handle_message(
                json!({ "jsonrpc": "2.0", "method": method, "params": params }),
                None,
                0,
                0,
            )
            .unwrap();
        loop {
            let event = receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("foreground result");
            let is_current_foreground = matches!(
                &event,
                ServerEvent::ForegroundDocumentReady { task, .. }
                    if task.revision() == version as u64
            );
            server.handle_internal_event(event).unwrap();
            if is_current_foreground {
                break;
            }
        }
    }

    let output = String::from_utf8(server.writer).unwrap();
    let mut reader = BufReader::new(output.as_bytes());
    let mut diagnostics = Vec::new();
    while let Some(message) = read_message(&mut reader).unwrap() {
        if message["method"] == "textDocument/publishDiagnostics" {
            diagnostics.push(message);
        }
    }
    let [broken, repaired] = diagnostics.as_slice() else {
        panic!("expected broken and repaired diagnostic publications");
    };
    assert_eq!(broken["params"]["version"], 1);
    assert!(!broken["params"]["diagnostics"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(repaired["params"]["version"], 2);
    assert_eq!(repaired["params"]["diagnostics"], json!([]));
}

#[test]
fn active_scope_delimiters_use_current_foreground_syntax_before_semantic_analysis() {
    let (sender, receiver) = mpsc::channel();
    let scheduler = RuntimeWorkExecutor::start(sender);
    let mut server = LspServer::new_with_runtime_senders(
        Vec::new(),
        LspServerOptions::default(),
        Some(scheduler),
        None,
    );
    let uri = "file:///Scripts/PendingScopeDelimiters.c";
    let source = "class Example\n{\n    void Run()\n    {\n        Missing();\n    }\n}";

    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": { "textDocument": {
                    "uri": uri,
                    "languageId": "enforce",
                    "version": 1,
                    "text": source
                }}
            }),
            None,
            0,
            0,
        )
        .unwrap();
    server.writer.clear();
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "id": 0,
                "method": "reforger/activeScopeDelimiters",
                "params": {
                    "textDocument": { "uri": uri },
                    "version": 1,
                    "positions": [{ "line": 4, "character": 16 }]
                }
            }),
            None,
            0,
            0,
        )
        .unwrap();
    let pending_output = String::from_utf8(server.writer.clone()).unwrap();
    let mut pending_reader = BufReader::new(pending_output.as_bytes());
    let pending_response = loop {
        let response = read_message(&mut pending_reader)
            .unwrap()
            .expect("pending active scope delimiter response");
        if response["id"] == 0 {
            break response;
        }
    };
    assert_eq!(pending_response["result"]["version"], 1);
    assert_eq!(pending_response["result"]["pending"], true);
    assert_eq!(pending_response["result"]["pairs"], json!([]));

    server.writer.clear();
    install_next_foreground(&mut server, &receiver);
    assert!(
        !server
            .document_runtime
            .test_document_state(uri)
            .unwrap()
            .analysis_ready
    );

    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "reforger/activeScopeDelimiters",
                "params": {
                    "textDocument": { "uri": uri },
                    "version": 1,
                    "positions": [{ "line": 4, "character": 16 }]
                }
            }),
            None,
            0,
            0,
        )
        .unwrap();

    let output = String::from_utf8(server.writer).unwrap();
    let mut reader = BufReader::new(output.as_bytes());
    let response = loop {
        let response = read_message(&mut reader)
            .unwrap()
            .expect("active scope delimiter response");
        if response["id"] == 1 {
            break response;
        }
    };
    assert_eq!(response["result"]["version"], 1);
    assert_eq!(response["result"]["pairs"].as_array().unwrap().len(), 1);
    assert_eq!(
        response["result"]["pairs"][0]["opener"]["start"],
        json!({ "line": 4, "character": 15 })
    );
}

#[test]
fn active_scope_delimiters_stop_pending_after_foreground_rejection() {
    let (sender, receiver) = mpsc::channel();
    let scheduler = RuntimeWorkExecutor::start(sender);
    let mut server = LspServer::new_with_runtime_senders(
        Vec::new(),
        LspServerOptions::default(),
        Some(scheduler),
        None,
    );
    let uri = "file:///Scripts/RejectedScopeDelimiters.c";
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": { "textDocument": {
                    "uri": uri,
                    "languageId": "enforce",
                    "version": 1,
                    "text": "class Example {}"
                }}
            }),
            None,
            0,
            0,
        )
        .unwrap();

    let skipped = match receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("foreground result")
    {
        ServerEvent::ForegroundDocumentReady { task, .. } => {
            ServerEvent::ForegroundDocumentSkipped {
                task,
                reason: "test rejection".to_string(),
                elapsed_ms: 0,
            }
        }
        _ => panic!("expected foreground result"),
    };
    server.handle_internal_event(skipped).unwrap();
    server.writer.clear();
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "reforger/activeScopeDelimiters",
                "params": {
                    "textDocument": { "uri": uri },
                    "version": 1,
                    "positions": [{ "line": 0, "character": 15 }]
                }
            }),
            None,
            0,
            0,
        )
        .unwrap();

    let output = String::from_utf8(server.writer).unwrap();
    let mut reader = BufReader::new(output.as_bytes());
    let response = loop {
        let response = read_message(&mut reader)
            .unwrap()
            .expect("rejected active scope delimiter response");
        if response["id"] == 1 {
            break response;
        }
    };
    assert_eq!(response["result"]["version"], 1);
    assert_eq!(response["result"]["pending"], false);
    assert_eq!(response["result"]["pairs"], json!([]));
}

#[test]
fn pending_document_symbol_request_returns_current_lexical_outline() {
    let (sender, receiver) = mpsc::channel();
    let scheduler = RuntimeWorkExecutor::start(sender);
    let mut server = LspServer::new_with_runtime_senders(
        Vec::new(),
        LspServerOptions::default(),
        Some(scheduler),
        None,
    );
    let uri = "file:///Scripts/PendingOutline.c";

    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": { "textDocument": {
                    "uri": uri,
                    "languageId": "enforce",
                    "version": 1,
                    "text": "class Initial {}"
                }}
            }),
            None,
            0,
            0,
        )
        .unwrap();
    install_next_foreground(&mut server, &receiver);
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": { "uri": uri, "version": 2 },
                    "contentChanges": [{ "text": "class Current {}" }]
                }
            }),
            None,
            0,
            0,
        )
        .unwrap();
    install_next_foreground(&mut server, &receiver);
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "textDocument/documentSymbol",
                "params": { "textDocument": { "uri": uri } }
            }),
            None,
            0,
            0,
        )
        .unwrap();
    let pending_output = String::from_utf8_lossy(&server.writer);
    assert!(pending_output.contains("\"id\":1"));
    assert!(pending_output.contains("\"name\":\"Current\""));
    assert!(!pending_output.contains("\"name\":\"Initial\""));

    server
        .handle_internal_event(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("document analysis result"),
        )
        .unwrap();

    let output = String::from_utf8(server.writer).unwrap();
    assert_eq!(output.matches("\"id\":1").count(), 1);
    assert!(output.contains("\"name\":\"Current\""));
    assert!(!output.contains("\"name\":\"Initial\""));
}

#[test]
fn runtime_debug_hover_rejects_a_capture_superseded_before_publication() {
    let (sender, receiver) = mpsc::channel();
    let scheduler = RuntimeWorkExecutor::start(sender);
    let mut server = LspServer::new_with_runtime_senders(
        Vec::new(),
        LspServerOptions::default(),
        Some(scheduler),
        None,
    );
    let uri = "file:///Scripts/StaleAsyncDebug.c";
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": { "textDocument": {
                    "uri": uri,
                    "languageId": "enforce",
                    "version": 1,
                    "text": "class StaleAsyncDebug { void Run() {} }"
                }}
            }),
            None,
            0,
            0,
        )
        .unwrap();
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "id": 8,
                "method": "reforger/debugHover",
                "params": {
                    "textDocument": { "uri": uri },
                    "position": { "line": 0, "character": 24 }
                }
            }),
            None,
            0,
            0,
        )
        .unwrap();
    let event = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("debug result");

    server
        .handle_message(
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": { "uri": uri, "version": 2 },
                    "contentChanges": [{ "text": "class Current {}" }]
                }
            }),
            None,
            0,
            0,
        )
        .unwrap();
    server.handle_internal_event(event).unwrap();

    let output = String::from_utf8(server.writer).unwrap();
    assert!(output.contains("\"id\":8"));
    assert!(output.contains("\"code\":-32801"));
    assert!(output.contains("Content modified"));
}

#[test]
fn pending_debug_request_receives_content_modified_when_a_new_edit_supersedes_it() {
    let (sender, _receiver) = mpsc::channel();
    let scheduler = RuntimeWorkExecutor::start(sender);
    let mut server = LspServer::new_with_runtime_senders(
        Vec::new(),
        LspServerOptions::default(),
        Some(scheduler),
        None,
    );
    let uri = "file:///Scripts/SupersededRequest.c";

    for (method, params) in [
        (
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": uri, "languageId": "enforce", "version": 1, "text": "class Initial {}"
            }}),
        ),
        (
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": 2 },
                "contentChanges": [{ "text": "class Pending {}" }]
            }),
        ),
    ] {
        server
            .handle_message(
                json!({ "jsonrpc": "2.0", "method": method, "params": params }),
                None,
                0,
                0,
            )
            .unwrap();
    }
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "reforger/debugHover",
                "params": {
                    "textDocument": { "uri": uri },
                    "position": { "line": 0, "character": 6 }
                }
            }),
            None,
            0,
            0,
        )
        .unwrap();
    server
        .handle_message(
            json!({
                "jsonrpc": "2.0", "method": "textDocument/didChange",
                "params": {
                    "textDocument": { "uri": uri, "version": 3 },
                    "contentChanges": [{ "text": "class Current {}" }]
                }
            }),
            None,
            0,
            0,
        )
        .unwrap();

    let output = String::from_utf8(server.writer).unwrap();
    assert!(output.contains("\"id\":1"));
    assert!(output.contains("\"code\":-32801"));
}
