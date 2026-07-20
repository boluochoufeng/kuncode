use super::support::{
    AgentConfig, AgentError, AgentRunner, AgentSession, ApprovalResolution, ApproveAll, Arc,
    AssistantContent, BrokenTool, CollectingObserver, ErrModel, EventKind, FakeModel,
    ScriptedApprovalResolver, ToolRegistry, event_label, register_bash, register_todo,
    reminder_count, response, response_many, tool_result_ids, tool_result_text,
};

#[tokio::test]
async fn plan_nag_fires_after_the_idle_interval() {
    // Two tool-only calls leave the plan untouched; on the third iteration
    // the idle counter hits the interval and a reminder is injected before
    // the model call.
    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "c1",
            "bash",
            serde_json::json!({ "cmd": "printf one" }),
        )),
        response(AssistantContent::tool_call(
            "c2",
            "bash",
            serde_json::json!({ "cmd": "printf two" }),
        )),
        response(AssistantContent::text("done")),
    ]);
    let mut registry = ToolRegistry::new();
    register_bash(&mut registry).await;
    let runner = AgentRunner::with_config(
        model,
        registry,
        AgentConfig {
            todo_reminder_interval: Some(2),
            ..AgentConfig::default()
        },
    );
    let mut session = AgentSession::new();

    runner
        .run_turn(&mut session, "keep working")
        .await
        .expect("agent run should complete");

    // Exactly one nudge, and it is not the opening user message — it was
    // injected mid-loop once the plan sat idle for the interval.
    assert_eq!(reminder_count(&session), 1);
}

#[tokio::test]
async fn a_todo_write_resets_the_plan_nag() {
    // Same iteration count as the firing case, but a `todo_write` up front
    // advances the plan generation and resets the idle counter, so the
    // interval is never reached and no reminder is injected.
    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "c1",
            "todo_write",
            serde_json::json!({
                "todos": [
                    { "content": "Step", "active_form": "Stepping", "status": "in_progress" }
                ]
            }),
        )),
        response(AssistantContent::tool_call(
            "c2",
            "bash",
            serde_json::json!({ "cmd": "printf go" }),
        )),
        response(AssistantContent::text("done")),
    ]);
    let mut registry = ToolRegistry::new();
    register_bash(&mut registry).await;
    register_todo(&mut registry);
    let runner = AgentRunner::with_config(
        model,
        registry,
        AgentConfig {
            todo_reminder_interval: Some(2),
            ..AgentConfig::default()
        },
    );
    let mut session = AgentSession::new();

    runner
        .run_turn(&mut session, "plan then work")
        .await
        .expect("agent run should complete");

    assert_eq!(reminder_count(&session), 0);
    // The plan really was set, which is what reset the counter.
    assert_eq!(session.todos_snapshot().len(), 1);
}

#[tokio::test]
async fn abort_mirrors_tool_results_and_ends_with_cancelled_error() {
    let model = FakeModel::new([response_many(vec![
        AssistantContent::tool_call("call_1", "bash", serde_json::json!({ "cmd": "printf one" })),
        AssistantContent::tool_call("call_2", "bash", serde_json::json!({ "cmd": "printf two" })),
    ])]);
    let mut registry = ToolRegistry::new();
    register_bash(&mut registry).await;
    let observer = Arc::new(CollectingObserver::default());
    let runner = AgentRunner::new(model, registry)
        .with_approval_resolver(Arc::new(ScriptedApprovalResolver::new([
            ApprovalResolution::Cancel,
        ])))
        .with_observer(observer.clone());
    let mut session = AgentSession::new();

    let err = runner
        .run_turn(&mut session, "do two things")
        .await
        .expect_err("abort unwinds the whole turn");
    assert!(matches!(err, AgentError::Cancelled));

    let events = observer.events();
    // Mirror invariant: one ToolEnd per transcript tool_result, same ids.
    let tool_ends: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolEnd { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_ends, vec!["call_1", "call_2"]);
    assert_eq!(tool_ends, tool_result_ids(&session));

    // Exactly one terminal Error, kind "cancelled", and it is last.
    let errors: Vec<_> = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::Error { .. }))
        .collect();
    assert_eq!(errors.len(), 1);
    assert!(matches!(
        &errors[0].kind,
        EventKind::Error { kind, .. } if kind == "cancelled"
    ));
    assert!(matches!(
        events.last().map(|e| &e.kind),
        Some(EventKind::Error { .. })
    ));
}

#[tokio::test]
async fn completion_failure_closes_thinking_with_error() {
    let observer = Arc::new(CollectingObserver::default());
    let runner = AgentRunner::new(ErrModel, ToolRegistry::new()).with_observer(observer.clone());
    let mut session = AgentSession::new();

    let err = runner
        .run_turn(&mut session, "go")
        .await
        .expect_err("the model fails");
    assert!(matches!(err, AgentError::Completion(_)));

    let events = observer.events();
    let kinds: Vec<_> = events.iter().map(|e| event_label(&e.kind)).collect();
    // ModelStart's "thinking" state is closed by the Error, with no
    // intervening Assistant.
    assert_eq!(kinds, vec!["model_start", "error"]);
    assert!(matches!(
        &events[1].kind,
        EventKind::Error { kind, .. } if kind == "completion"
    ));
}

#[tokio::test]
async fn harness_tool_error_is_honestly_ended_then_turn_errors() {
    let model = FakeModel::new([response_many(vec![
        AssistantContent::tool_call("call_1", "broken", serde_json::json!({})),
        AssistantContent::tool_call("call_2", "broken", serde_json::json!({})),
    ])]);
    let mut registry = ToolRegistry::new();
    registry
        .register(BrokenTool::new())
        .expect("broken tool registration");
    let observer = Arc::new(CollectingObserver::default());
    let runner = AgentRunner::new(model, registry)
        .with_approval_resolver(Arc::new(ApproveAll))
        .with_observer(observer.clone());
    let mut session = AgentSession::new();

    let err = runner
        .run_turn(&mut session, "break it")
        .await
        .expect_err("a harness tool error unwinds the turn");
    assert!(matches!(err, AgentError::Tool { .. }));

    let events = observer.events();
    let tool_ends: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolEnd {
                tool_call_id,
                error,
                ..
            } => Some((
                tool_call_id.clone(),
                error.as_ref().map(|f| f.kind.to_string()),
            )),
            _ => None,
        })
        .collect();
    // call_1 actually ran and failed → honest "tool_error"; call_2 never ran
    // → "cancelled".
    assert_eq!(
        tool_ends,
        vec![
            ("call_1".to_string(), Some("tool_error".to_string())),
            ("call_2".to_string(), Some("cancelled".to_string())),
        ],
    );
    // The transcript result mirrors the ToolEnd's honest failure.
    assert!(tool_result_text(&session, 2).contains("tool_error"));

    let errors: Vec<_> = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::Error { .. }))
        .collect();
    assert_eq!(errors.len(), 1);
    assert!(matches!(
        &errors[0].kind,
        EventKind::Error { kind, .. } if kind == "tool"
    ));
}
