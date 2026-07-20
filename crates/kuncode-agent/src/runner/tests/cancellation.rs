use super::support::{
    AgentError, AgentRunner, AgentSession, ApprovalResolution, ApproveAll, Arc, AssistantContent,
    CancellationToken, FakeModel, HangModel, HangTool, ScriptedApprovalResolver, ToolRegistry,
    cancellable, register_bash, response, response_many, tool_result_id, tool_result_text,
};

#[tokio::test]
async fn cancellable_yields_none_when_cancelled_while_pending() {
    let cancel = CancellationToken::new();
    // Cancel from inside the racing future, then never return: the cancel
    // branch is the only one that can resolve, so the race ends in `None`.
    let fut = {
        let cancel = cancel.clone();
        async move {
            cancel.cancel();
            std::future::pending::<i32>().await
        }
    };
    assert_eq!(cancellable(&cancel, fut).await, None);
}

#[tokio::test]
async fn abort_pairs_every_tool_call_with_a_result() {
    // One assistant turn emits TWO tool calls; the user aborts at the first
    // approval prompt. Both tool_calls must still get a tool_result, or the
    // assistant message dangles and the next turn's request is rejected.
    let model = FakeModel::new([response_many(vec![
        AssistantContent::tool_call("call_1", "bash", serde_json::json!({ "cmd": "printf one" })),
        AssistantContent::tool_call("call_2", "bash", serde_json::json!({ "cmd": "printf two" })),
    ])]);
    let mut registry = ToolRegistry::new();
    register_bash(&mut registry).await;
    let runner = AgentRunner::new(model, registry).with_approval_resolver(Arc::new(
        ScriptedApprovalResolver::new([ApprovalResolution::Cancel]),
    ));
    let mut session = AgentSession::new();

    let err = runner
        .run_turn(&mut session, "do two things")
        .await
        .expect_err("abort unwinds the whole turn");
    assert!(matches!(err, AgentError::Cancelled));

    // Transcript: user, assistant(2 tool_calls), tool_result(call_1),
    // tool_result(call_2) — every tool_call paired, so it is re-sendable.
    assert_eq!(session.messages().len(), 4);
    assert_eq!(tool_result_id(&session, 2), "call_1");
    assert_eq!(tool_result_id(&session, 3), "call_2");
    assert!(tool_result_text(&session, 2).contains("cancelled"));
    assert!(tool_result_text(&session, 3).contains("cancelled"));
}

#[tokio::test]
async fn cancellation_token_interrupts_a_running_tool() {
    let model = FakeModel::new([response(AssistantContent::tool_call(
        "call_1",
        "hang",
        serde_json::json!({}),
    ))]);
    let mut registry = ToolRegistry::new();
    registry
        .register(HangTool::new())
        .expect("hang tool registration");
    let runner = AgentRunner::new(model, registry).with_approval_resolver(Arc::new(ApproveAll));
    let mut session = AgentSession::new();

    // A fresh (un-cancelled) token: the model stage runs normally and the
    // `HangTool` cancels mid-run, so the interrupt lands specifically on the
    // tool-execution `select!`.
    let cancel = CancellationToken::new();

    let err = runner
        .run_turn_with(&mut session, "hang please", cancel)
        .await
        .expect_err("a tool that cancels mid-run interrupts the call");

    assert!(matches!(err, AgentError::Cancelled));
    // The cancelled tool_call is still paired with a synthetic result, so
    // the transcript stays re-sendable: user, assistant(1 call), tool_result.
    assert_eq!(session.messages().len(), 3);
    assert!(tool_result_text(&session, 2).contains("cancelled"));
}

#[tokio::test]
async fn cancellation_token_interrupts_a_model_request() {
    let runner = AgentRunner::new(HangModel, ToolRegistry::new());
    let mut session = AgentSession::new();

    // Pre-cancelled token: the never-returning model loses the race to the
    // cancellation branch deterministically, proving the gate now wraps the
    // model call — not only tool approval/execution.
    let cancel = CancellationToken::new();
    cancel.cancel();

    let err = runner
        .run_turn_with(&mut session, "think forever", cancel)
        .await
        .expect_err("a cancelled token interrupts the model request");

    assert!(matches!(err, AgentError::Cancelled));
    // The turn aborted before any assistant message was appended: only the
    // user prompt is in the transcript.
    assert_eq!(session.messages().len(), 1);
}

#[tokio::test]
async fn cancellable_yields_some_when_the_future_wins() {
    let cancel = CancellationToken::new();
    // An un-cancelled token never fires, so the ready future wins the race.
    assert_eq!(cancellable(&cancel, async { 42 }).await, Some(42));
}

#[tokio::test]
async fn cancellable_is_biased_toward_an_already_cancelled_token() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    // Even against an immediately-ready future, a pre-cancelled token wins:
    // `biased` polls the cancel branch first. This is the determinism the six
    // call sites rely on; a non-biased `select!` could pick either branch.
    assert_eq!(cancellable(&cancel, async { 42 }).await, None);
}
