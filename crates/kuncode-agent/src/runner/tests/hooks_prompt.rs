use super::support::{
    AgentError, AgentRunner, AgentSession, Arc, AssistantContent, CollectingObserver,
    CompletionRequest, EventKind, FakeModel, Message, PermissionPolicy, RuleOrigin,
    ScriptedApprover, ScriptedHook, ToolRegistry, UserContent, bash, parse_rule, response,
    tool_result_text, user_text,
};

#[tokio::test]
async fn empty_transcript_error_carries_no_iteration() {
    let observer = Arc::new(CollectingObserver::default());
    let runner =
        AgentRunner::new(FakeModel::default(), ToolRegistry::new()).with_observer(observer.clone());
    let mut session = AgentSession::new();

    let err = runner
        .continue_session(&mut session)
        .await
        .expect_err("an empty transcript is invalid");
    assert!(matches!(err, AgentError::EmptyTranscript));

    let events = observer.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].iteration, None);
    assert!(matches!(
        &events[0].kind,
        EventKind::Error { kind, .. } if kind == "empty_transcript"
    ));
}

/// Whether any message in a request's history is a user text equal to `text`.
fn request_has_user_text(request: &CompletionRequest, text: &str) -> bool {
    request.chat_history.iter().any(|message| {
        matches!(
            message,
            Message::User { content }
                if matches!(content.first(), UserContent::Text(t) if t.text_ref() == text)
        )
    })
}

#[tokio::test]
async fn pre_tool_use_deny_short_circuits_the_gate() {
    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_1",
            "bash",
            serde_json::json!({ "cmd": "printf hi" }),
        )),
        response(AssistantContent::text("understood")),
    ]);
    let mut registry = ToolRegistry::new();
    registry.register(bash().await);
    let observer = Arc::new(CollectingObserver::default());
    // A scripted approver with no outcomes panics if consulted, proving the
    // hook deny short-circuits before the gate reaches the approver.
    let runner = AgentRunner::new(model, registry)
        .with_approver(Arc::new(ScriptedApprover::new([])))
        .with_observer(observer.clone())
        .with_hook(Arc::new(ScriptedHook::default().deny_tool("bash")));
    let mut session = AgentSession::new();

    runner
        .run_turn(&mut session, "run it")
        .await
        .expect("a hook denial is model-recoverable");

    let result = tool_result_text(&session, 2);
    assert!(result.contains("blocked_by_hook"), "got {result}");

    // Same event shape as a permission denial: ToolStart, then a failed ToolEnd.
    let events = observer.events();
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::ToolStart { .. }))
    );
    let tool_ends: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolEnd { error, .. } => Some(error.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_ends.len(), 1);
    assert!(matches!(&tool_ends[0], Some(f) if f.kind.as_str() == "blocked_by_hook"));
}

#[tokio::test]
async fn hook_proceed_does_not_bypass_a_gate_deny() {
    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_1",
            "bash",
            serde_json::json!({ "cmd": "curl http://evil.test" }),
        )),
        response(AssistantContent::text("ok")),
    ]);
    let mut registry = ToolRegistry::new();
    registry.register(bash().await);
    let mut policy = PermissionPolicy::new();
    policy
        .deny
        .extend(parse_rule("Bash(curl*)", RuleOrigin::ProjectSettings).unwrap());
    // A hook that only proceeds must not turn a gate deny into an allow.
    let runner = AgentRunner::new(model, registry)
        .with_policy(policy)
        .with_hook(Arc::new(ScriptedHook::default()));
    let mut session = AgentSession::new();

    runner
        .run_turn(&mut session, "fetch")
        .await
        .expect("a denial is model-recoverable");
    let result = tool_result_text(&session, 2);
    assert!(result.contains("permission_denied"), "got {result}");
}

#[tokio::test]
async fn user_prompt_submit_add_context_reaches_the_model() {
    let model = FakeModel::new([response(AssistantContent::text("done"))]);
    let runner = AgentRunner::new(model.clone(), ToolRegistry::new())
        .with_hook(Arc::new(ScriptedHook::default().add_context("INJECTED")));
    let mut session = AgentSession::new();

    runner.run_turn(&mut session, "hi").await.expect("runs");

    // Prompt then injected context, in order; and the model saw the context.
    assert_eq!(user_text(&session, 0).as_deref(), Some("hi"));
    assert_eq!(user_text(&session, 1).as_deref(), Some("INJECTED"));
    assert!(request_has_user_text(&model.requests()[0], "INJECTED"));
}

#[tokio::test]
async fn user_prompt_submit_block_leaves_no_trace() {
    let model = FakeModel::new([response(AssistantContent::text("unreached"))]);
    let observer = Arc::new(CollectingObserver::default());
    let runner = AgentRunner::new(model.clone(), ToolRegistry::new())
        .with_observer(observer.clone())
        .with_hook(Arc::new(ScriptedHook::default().block("not allowed")));
    let mut session = AgentSession::new();

    let err = runner
        .run_turn(&mut session, "secret")
        .await
        .expect_err("the prompt is blocked");
    assert!(matches!(err, AgentError::PromptBlocked { reason } if reason == "not allowed"));

    // Nothing entered the transcript and the model was never called.
    assert!(session.messages().is_empty());
    assert!(model.requests().is_empty());

    // The block is still visible to the observer as the turn-terminal error.
    let events = observer.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].iteration, None);
    assert!(matches!(
        &events[0].kind,
        EventKind::Error { kind, .. } if kind == "prompt_blocked"
    ));
}
