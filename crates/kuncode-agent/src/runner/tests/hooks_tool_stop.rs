#[tokio::test]
async fn post_tool_use_feedback_lands_after_the_batch() {
    let model = FakeModel::new([
        response_many(vec![
            AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "printf one" }),
            ),
            AssistantContent::tool_call(
                "call_2",
                "bash",
                serde_json::json!({ "cmd": "printf two" }),
            ),
        ]),
        response(AssistantContent::text("done")),
    ]);
    let mut registry = ToolRegistry::new();
    registry.register(bash().await);
    let runner = AgentRunner::new(model, registry)
        .with_hook(Arc::new(ScriptedHook::default().add_feedback("FB")));
    let mut session = AgentSession::new();

    runner.run_turn(&mut session, "do two").await.expect("runs");

    // The two tool_results stay contiguous; feedback follows the batch.
    assert!(is_tool_result(&session, 2));
    assert!(is_tool_result(&session, 3));
    assert!(!is_tool_result(&session, 4));
    assert_eq!(tool_result_id(&session, 2), "call_1");
    assert_eq!(tool_result_id(&session, 3), "call_2");
    assert_eq!(user_text(&session, 4).as_deref(), Some("FB"));
}

#[tokio::test]
async fn post_tool_use_does_not_fire_on_a_harness_error() {
    let model = FakeModel::new([response(AssistantContent::tool_call(
        "call_1",
        "broken",
        serde_json::json!({}),
    ))]);
    let mut registry = ToolRegistry::new();
    registry.register(BrokenTool::new());
    let count = Arc::new(AtomicUsize::new(0));
    let runner =
        AgentRunner::new(model, registry).with_hook(Arc::new(CountingPostHook(count.clone())));
    let mut session = AgentSession::new();

    let err = runner
        .run_turn(&mut session, "break")
        .await
        .expect_err("a harness tool error unwinds the turn");
    assert!(matches!(err, AgentError::Tool { .. }));
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "PostToolUse must not fire on a harness-boundary AgentError::Tool",
    );
}

#[tokio::test]
async fn post_tool_use_cancellation_keeps_results_paired() {
    let model = FakeModel::new([response_many(vec![
        AssistantContent::tool_call("call_1", "bash", serde_json::json!({ "cmd": "printf one" })),
        AssistantContent::tool_call("call_2", "bash", serde_json::json!({ "cmd": "printf two" })),
    ])]);
    let mut registry = ToolRegistry::new();
    registry.register(bash().await);
    let cancel = CancellationToken::new();
    let runner =
        AgentRunner::new(model, registry).with_hook(Arc::new(CancelInPostHook(cancel.clone())));
    let mut session = AgentSession::new();

    let err = runner
        .run_turn_with(&mut session, "do two", cancel)
        .await
        .expect_err("the hook cancels the turn");
    assert!(matches!(err, AgentError::Cancelled));

    // call_1 ran and was recorded exactly once (real output); call_2 is paired
    // as interrupted — the invariant holds with no duplicate for call_1.
    assert_eq!(session.messages().len(), 4);
    assert_eq!(tool_result_id(&session, 2), "call_1");
    assert!(tool_result_text(&session, 2).contains("\"stdout\":\"one\""));
    assert_eq!(tool_result_id(&session, 3), "call_2");
    assert!(tool_result_text(&session, 3).contains("cancelled"));
}

#[tokio::test]
async fn stop_continue_forces_more_iterations_then_allows() {
    let model = FakeModel::new([
        response(AssistantContent::text("a")),
        response(AssistantContent::text("b")),
        response(AssistantContent::text("done")),
    ]);
    let runner = AgentRunner::new(model, ToolRegistry::new()).with_hook(Arc::new(
        ScriptedHook::default().stop_continue(2, "keep going"),
    ));
    let mut session = AgentSession::new();

    let turn = runner
        .run_turn(&mut session, "go")
        .await
        .expect("completes after the scripted continuations");
    assert_eq!(turn.iterations, 3);
    assert_eq!(turn.final_text(&session), "done");
}

#[tokio::test]
async fn stop_continue_is_ignored_without_iteration_budget() {
    // The last allowed model call returns a final answer; a hook wants to
    // continue but there is no budget, so the answer stands rather than
    // turning into a MaxIterations error.
    let model = FakeModel::new([response(AssistantContent::text("final"))]);
    let runner = AgentRunner::with_config(
        model,
        ToolRegistry::new(),
        AgentConfig {
            max_iterations: 1,
            ..AgentConfig::default()
        },
    )
    .with_hook(Arc::new(ScriptedHook::default().stop_continue(5, "more")));
    let mut session = AgentSession::new();

    let turn = runner
        .run_turn(&mut session, "go")
        .await
        .expect("the final answer is kept, not turned into MaxIterations");
    assert_eq!(turn.iterations, 1);
    assert_eq!(turn.final_text(&session), "final");
}

#[tokio::test]
async fn stop_continuation_cap_resets_each_turn() {
    // An always-continue hook: the runner's cap (a run_loop local), not the
    // hook, stops it — so each turn gets a fresh budget of continuations. If
    // the cap lived on the session, turn B would stop after one iteration.
    let model = FakeModel::new([
        response(AssistantContent::text("a0")),
        response(AssistantContent::text("a1")),
        response(AssistantContent::text("a2")),
        response(AssistantContent::text("a3")),
        response(AssistantContent::text("b0")),
        response(AssistantContent::text("b1")),
        response(AssistantContent::text("b2")),
        response(AssistantContent::text("b3")),
    ]);
    let runner =
        AgentRunner::new(model, ToolRegistry::new()).with_hook(Arc::new(AlwaysContinueHook));
    let mut session = AgentSession::new();

    let turn_a = runner
        .run_turn(&mut session, "first")
        .await
        .expect("turn A");
    assert_eq!(turn_a.iterations, 4);

    let turn_b = runner
        .run_turn(&mut session, "second")
        .await
        .expect("turn B");
    assert_eq!(turn_b.iterations, 4);
}

#[tokio::test]
async fn hook_cancellation_is_not_a_model_visible_deny() {
    let model = FakeModel::new([response(AssistantContent::tool_call(
        "call_1",
        "bash",
        serde_json::json!({ "cmd": "printf hi" }),
    ))]);
    let mut registry = ToolRegistry::new();
    registry.register(bash().await);
    let cancel = CancellationToken::new();
    let runner =
        AgentRunner::new(model, registry).with_hook(Arc::new(CancelInPreHook(cancel.clone())));
    let mut session = AgentSession::new();

    let err = runner
        .run_turn_with(&mut session, "run", cancel)
        .await
        .expect_err("the hook cancels the turn");
    assert!(matches!(err, AgentError::Cancelled));

    // The call is paired as interrupted ("cancelled"), never a blocked_by_hook
    // deny — a user cancel must not be mistaken for a hook decision.
    let result = tool_result_text(&session, 2);
    assert!(result.contains("cancelled"), "got {result}");
    assert!(
        !result.contains("blocked_by_hook"),
        "cancellation was mis-mapped to a deny: {result}"
    );
}
