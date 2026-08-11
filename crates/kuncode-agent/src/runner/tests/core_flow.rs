use super::support::{
    AgentConfig, AgentRunner, AgentSession, ApproveAll, Arc, AssistantContent, CollectingObserver,
    CompactionMode, CompletionError, CompletionRequest, CompletionResponse, CompletionStream,
    EventKind, FakeModel, FinishReason, Message, NonEmptyVec, StreamEvent, ToolRegistry,
    ToolResultContent, Usage, UserContent, Value, configured_runner, event_label, register_bash,
    response,
};

/// A model that streams reasoning + text deltas before the final answer, for
/// asserting the runner forwards render deltas and still finalizes with
/// `Assistant`.
#[derive(Clone)]
struct DeltaModel;

impl kuncode_core::completion::CompletionModel for DeltaModel {
    type Response = Value;
    type Client = ();

    fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
        Self
    }

    async fn completion(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        unimplemented!("delta model only streams")
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionStream, CompletionError> {
        let events = vec![
            Ok(StreamEvent::ReasoningDelta("hmm".to_string())),
            Ok(StreamEvent::TextDelta("Hel".to_string())),
            Ok(StreamEvent::TextDelta("lo".to_string())),
            Ok(StreamEvent::Completed {
                content: NonEmptyVec::new(AssistantContent::text("Hello")),
                usage: Usage::default(),
                finish_reason: FinishReason::Stop,
                model: None,
            }),
        ];
        Ok(Box::pin(futures_util::stream::iter(events)))
    }
}

#[tokio::test]
async fn streaming_forwards_deltas_then_finalizes_with_assistant() {
    let observer = Arc::new(CollectingObserver::default());
    let runner = AgentRunner::new(DeltaModel, ToolRegistry::new()).with_observer(observer.clone());
    let mut session = AgentSession::new();

    runner
        .run_turn(&mut session, "hi")
        .await
        .expect("agent run should complete");

    let events = observer.events();
    let kinds: Vec<_> = events.iter().map(|e| event_label(&e.kind)).collect();
    assert_eq!(
        kinds,
        vec![
            "model_start",
            "reasoning_delta",
            "text_delta",
            "text_delta",
            "assistant",
        ],
    );

    // Deltas carry the streamed fragments; the final Assistant carries the
    // authoritative assembled text.
    let text_deltas: Vec<&str> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text_deltas, ["Hel", "lo"]);
    assert!(matches!(
        &events[4].kind,
        EventKind::Assistant { text, tool_calls } if text == "Hello" && tool_calls.is_empty()
    ));
}

#[tokio::test]
async fn runs_tool_call_then_final_answer() {
    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_1",
            "bash",
            serde_json::json!({ "cmd": "printf s01" }),
        )),
        response(AssistantContent::text("done")),
    ]);
    let mut registry = ToolRegistry::new();
    register_bash(&mut registry).await;
    let runner =
        AgentRunner::new(model.clone(), registry).with_approval_resolver(Arc::new(ApproveAll));
    let mut session = AgentSession::new();

    let turn = runner
        .run_turn(&mut session, "inspect the workspace")
        .await
        .expect("agent run should complete");

    assert_eq!(turn.final_text(&session), "done");
    assert_eq!(turn.iterations, 2);
    assert_eq!(turn.usage.total_tokens, 6);
    assert_eq!(session.messages().len(), 4);

    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].tools[0].name, "bash");
    assert_eq!(requests[1].tools[0].name, "bash");
    assert_eq!(requests[1].chat_history.len(), 3);

    match &session.messages()[2] {
        Message::User { content } => {
            let UserContent::ToolResult(result) = content.first() else {
                panic!("expected tool result content");
            };
            let ToolResultContent::Text(text) = result.content.first();
            assert!(text.text_ref().contains("\"stdout\":\"s01\""));
        }
        other => panic!("expected tool result user message, got {other:?}"),
    }
}

#[tokio::test]
async fn with_model_swaps_the_model_used_for_turns() {
    let original = FakeModel::new([]);
    let replacement = FakeModel::new([response(AssistantContent::text("done"))]);
    let runner =
        AgentRunner::new(original.clone(), ToolRegistry::new()).with_model(replacement.clone());
    let mut session = AgentSession::new();

    runner
        .run_turn(&mut session, "hi")
        .await
        .expect("agent run should complete");

    assert!(
        original.requests().is_empty(),
        "the replaced model must not be called"
    );
    assert_eq!(replacement.requests().len(), 1);
}

#[tokio::test]
async fn with_agent_config_swaps_max_tokens_and_compaction() {
    let model = FakeModel::new([response(AssistantContent::text("done"))]);
    let runner =
        configured_runner(model.clone(), CompactionMode::Enabled).with_agent_config(AgentConfig {
            max_tokens: Some(1234),
            ..AgentConfig::default()
        });
    let mut session = AgentSession::new();

    runner
        .run_turn(&mut session, "hi")
        .await
        .expect("agent run should complete");

    assert_eq!(model.requests()[0].max_tokens, Some(1234));
    // Wholesale replacement: the configured runner's compaction is gone.
    assert!(runner.config.compaction.is_none());
}
