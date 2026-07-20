use super::support::{
    AgentConfig, AgentError, AgentRunner, AgentSession, ApproveAll, Arc, AssistantContent,
    CanonicalToolInput, ExecutedInvocation, FakeModel, IdentitySection, Message, NonEmptyVec,
    PermissionCheckSpec, PermissionTarget, PolicyEffect, PolicyOrigin, PreparationContext,
    PreparedInvocation, RememberExactOnce, SystemPrompt, Tool, ToolContext, ToolDefinition,
    ToolDisplay, ToolError, ToolOutput, ToolPreparation, ToolRegistry, Value, async_trait,
    empty_policy, register_bash, response, tool_result_text,
};

struct MisregisteredTool {
    definition: ToolDefinition,
}

impl MisregisteredTool {
    fn new() -> Self {
        Self {
            definition: ToolDefinition {
                name: "misregistered".to_string(),
                description: "test profile invariant".to_string(),
                parameters: serde_json::json!({ "type": "object" }),
            },
        }
    }
}

#[async_trait]
impl Tool for MisregisteredTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare(
        self: Arc<Self>,
        args: Value,
        _ctx: &PreparationContext,
    ) -> Result<ToolPreparation, ToolOutput> {
        Ok(ToolPreparation::new(
            CanonicalToolInput::new(args),
            Box::new(UnreachableInvocation),
            NonEmptyVec::new(PermissionCheckSpec::new(PermissionTarget::TodoWrite)),
            ToolDisplay::new("Run misregistered tool"),
        ))
    }
}

struct UnreachableInvocation;

#[async_trait]
impl PreparedInvocation for UnreachableInvocation {
    async fn execute(self: Box<Self>, _ctx: &ToolContext) -> Result<ExecutedInvocation, ToolError> {
        Ok(ExecutedInvocation::new(
            ToolOutput::success(serde_json::json!({})),
            crate::tool::ToolResultRetention::Verbatim,
        ))
    }
}

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
    register_bash(&mut registry).await;
    let runner = AgentRunner::with_config(model.clone(), registry, AgentConfig::default())
        .with_approval_resolver(Arc::new(ApproveAll))
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
    register_bash(&mut registry).await;
    let runner = AgentRunner::with_config(
        model,
        registry,
        AgentConfig {
            max_iterations: 1,
            ..AgentConfig::default()
        },
    )
    .with_approval_resolver(Arc::new(ApproveAll));
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
    register_bash(&mut registry).await;
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
    register_bash(&mut registry).await;
    let mut policy = empty_policy();
    policy
        .compile_and_push("Bash(curl*)", PolicyEffect::Deny, PolicyOrigin::Project)
        .expect("valid deny rule");
    let runner = AgentRunner::new(model, registry)
        .with_policy(policy)
        .expect("policy root matches registry");
    let mut session = AgentSession::new();

    runner
        .run_turn(&mut session, "fetch the script")
        .await
        .expect("a denial is model-recoverable, so the turn still completes");

    // The tool never ran; the model got a clear permission_denied result.
    let result = tool_result_text(&session, 2);
    assert!(result.contains("permission_denied"), "got {result}");
}

#[tokio::test]
async fn exact_session_grant_skips_only_the_same_second_prompt() {
    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_1",
            "bash",
            serde_json::json!({ "cmd": "printf one" }),
        )),
        response(AssistantContent::tool_call(
            "call_2",
            "bash",
            serde_json::json!({ "cmd": "printf one" }),
        )),
        response(AssistantContent::text("done")),
    ]);
    let mut registry = ToolRegistry::new();
    register_bash(&mut registry).await;
    let resolver = Arc::new(RememberExactOnce::default());
    let runner = AgentRunner::new(model, registry).with_approval_resolver(resolver.clone());
    let mut session = AgentSession::new();

    let turn = runner
        .run_turn(&mut session, "print twice")
        .await
        .expect("both calls run, the second via the grant");

    assert_eq!(turn.final_text(&session), "done");
    assert!(tool_result_text(&session, 2).contains("\"stdout\":\"one\""));
    assert!(tool_result_text(&session, 4).contains("\"stdout\":\"one\""));
    assert_eq!(resolver.calls(), 1);
    assert_eq!(session.permissions().rules().len(), 1);
}

#[tokio::test]
async fn profile_violation_aborts_with_an_honest_tool_registration_result() {
    let model = FakeModel::new([response(AssistantContent::tool_call(
        "call_1",
        "misregistered",
        serde_json::json!({}),
    ))]);
    let mut registry = ToolRegistry::new();
    registry
        .register(MisregisteredTool::new())
        .expect("fallback profile registers");
    let runner = AgentRunner::new(model, registry);
    let mut session = AgentSession::new();

    let error = runner
        .run_turn(&mut session, "run the invalid adapter")
        .await
        .expect_err("profile violation stops the runner");

    assert!(matches!(error, AgentError::ToolRegistration { .. }));
    let result = tool_result_text(&session, 2);
    assert!(result.contains("tool_registration"), "got {result}");
    assert!(!result.contains("cancelled"), "got {result}");
}
