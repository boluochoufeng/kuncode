use super::support::{
    AgentRunner, AgentSession, Arc, AssistantContent, CollectingObserver, EventKind, FakeModel,
    Message, NewSession, Seq, SessionId, SessionStore, TestDir, TodoWrite, ToolRegistry,
    TursoSessionStore, bash, event_label, response,
};

#[tokio::test]
async fn run_turn_persists_messages_to_session_store() {
    let root = TestDir::new();
    let store = Arc::new(
        TursoSessionStore::open(root.path().join("sessions.db"))
            .await
            .expect("store should open"),
    );
    let session_id = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let model = FakeModel::new([response(AssistantContent::text("done"))]);
    let runner = AgentRunner::new(model, ToolRegistry::new()).with_session_store(store.clone());
    let mut session = AgentSession::new();
    session
        .attach_session_id(session_id.clone())
        .expect("fresh session should attach");

    runner
        .run_turn(&mut session, "hi")
        .await
        .expect("turn should complete");

    let entries = store
        .replay_after(&session_id, Seq::ZERO)
        .await
        .expect("journal should replay");
    let messages: Vec<Message> = entries
        .into_iter()
        .map(|entry| entry.into_message().expect("message payload"))
        .collect();
    assert_eq!(messages, session.messages());
    assert_eq!(session.durable_seq(), Some(Seq::new(2)));
}

#[tokio::test]
async fn append_failure_keeps_message_in_memory_without_advancing_frontier() {
    let root = TestDir::new();
    let store = Arc::new(
        TursoSessionStore::open(root.path().join("sessions.db"))
            .await
            .expect("store should open"),
    );
    let model = FakeModel::new([response(AssistantContent::text("done"))]);
    let runner = AgentRunner::new(model, ToolRegistry::new()).with_session_store(store);
    let mut session = AgentSession::new();
    session
        .attach_session_id(SessionId::new("missing-session"))
        .expect("fresh session should attach");

    runner
        .run_turn(&mut session, "kept in memory")
        .await
        .expect("degraded persistence should not abort the turn");

    assert_eq!(session.durable_seq(), Some(Seq::ZERO));
    assert!(!session.is_durable());
    assert_eq!(session.messages().len(), 2);
    assert_eq!(session.messages()[0], Message::user("kept in memory"));
    assert!(session.take_persistence_error().is_some());
}

#[tokio::test]
async fn emits_full_event_sequence_on_success() {
    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_1",
            "bash",
            serde_json::json!({ "cmd": "printf s01" }),
        )),
        response(AssistantContent::text("done")),
    ]);
    let mut registry = ToolRegistry::new();
    registry.register(bash().await);
    let observer = Arc::new(CollectingObserver::default());
    let runner = AgentRunner::new(model, registry).with_observer(observer.clone());
    let mut session = AgentSession::new();

    runner
        .run_turn(&mut session, "inspect the workspace")
        .await
        .expect("agent run should complete");

    let events = observer.events();
    let kinds: Vec<_> = events.iter().map(|e| event_label(&e.kind)).collect();
    assert_eq!(
        kinds,
        vec![
            "model_start",
            "assistant",
            "tool_start",
            "tool_end",
            "model_start",
            "assistant",
        ],
    );

    // First assistant carries the tool call; the final one carries none.
    assert!(matches!(
        &events[1].kind,
        EventKind::Assistant { tool_calls, .. } if tool_calls == &["call_1"]
    ));
    assert!(matches!(
        &events[5].kind,
        EventKind::Assistant { tool_calls, .. } if tool_calls.is_empty()
    ));
    assert!(matches!(
        &events[3].kind,
        EventKind::ToolEnd {
            ok: true,
            error: None,
            ..
        }
    ));
    // Happy path: no terminal Error, every event owns a model call, and seq
    // is strictly monotonic from 0.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.kind, EventKind::Error { .. }))
    );
    assert!(events.iter().all(|e| e.iteration.is_some()));
    assert!(events.iter().enumerate().all(|(i, e)| e.seq == i as u64));
}

#[tokio::test]
async fn todo_write_emits_a_todo_update_after_tool_end() {
    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_1",
            "todo_write",
            serde_json::json!({
                "todos": [
                    { "content": "Plan it", "active_form": "Planning it", "status": "in_progress" }
                ]
            }),
        )),
        response(AssistantContent::text("done")),
    ]);
    let mut registry = ToolRegistry::new();
    registry.register(TodoWrite::new());
    let observer = Arc::new(CollectingObserver::default());
    let runner = AgentRunner::new(model, registry).with_observer(observer.clone());
    let mut session = AgentSession::new();

    runner
        .run_turn(&mut session, "make a plan")
        .await
        .expect("agent run should complete");

    let events = observer.events();
    let kinds: Vec<_> = events.iter().map(|e| event_label(&e.kind)).collect();
    // `Meta` is allow-by-default, so the call runs unprompted and the plan
    // update lands right after the tool's terminal event.
    assert_eq!(
        kinds,
        vec![
            "model_start",
            "assistant",
            "tool_start",
            "tool_end",
            "todo_update",
            "model_start",
            "assistant",
        ],
    );
    let todos = events.iter().find_map(|e| match &e.kind {
        EventKind::TodoUpdate { todos } => Some(todos.clone()),
        _ => None,
    });
    let todos = todos.expect("a todo_update was emitted");
    assert_eq!(todos.len(), 1);
    assert_eq!(todos[0].content, "Plan it");
    // The session store holds the same plan the event carried.
    assert_eq!(session.todos_snapshot(), todos);
}

#[tokio::test]
async fn rejected_todo_write_emits_no_todo_update() {
    // Two in_progress items fail validation: the call still produces a
    // ToolEnd(ok:false), but the plan generation never advances, so no
    // TodoUpdate is emitted.
    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_1",
            "todo_write",
            serde_json::json!({
                "todos": [
                    { "content": "a", "active_form": "a…", "status": "in_progress" },
                    { "content": "b", "active_form": "b…", "status": "in_progress" }
                ]
            }),
        )),
        response(AssistantContent::text("understood")),
    ]);
    let mut registry = ToolRegistry::new();
    registry.register(TodoWrite::new());
    let observer = Arc::new(CollectingObserver::default());
    let runner = AgentRunner::new(model, registry).with_observer(observer.clone());
    let mut session = AgentSession::new();

    runner
        .run_turn(&mut session, "make a bad plan")
        .await
        .expect("a validation failure is model-recoverable");

    let labels: Vec<_> = observer
        .events()
        .iter()
        .map(|e| event_label(&e.kind))
        .collect();
    assert!(!labels.contains(&"todo_update"), "got {labels:?}");
    // The plan was left empty by the rejected write.
    assert!(session.todos_snapshot().is_empty());
}
