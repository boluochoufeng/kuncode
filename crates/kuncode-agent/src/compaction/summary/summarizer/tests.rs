use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use kuncode_core::{
    completion::{
        AssistantContent, CompletionError, CompletionModel, CompletionRequest, CompletionResponse,
        CompletionStream, Message, ReasoningEffort, ToolChoice, Usage,
    },
    non_empty_vec::NonEmptyVec,
};
use serde_json::Value;

use super::{ContextSummarizer, LlmContextSummarizer, SummarizerError};
use crate::{
    compaction::summary::{
        CONTINUITY_SUMMARY_VERSION, SummaryRequest, validation::SummaryValidationContext,
    },
    session::AgentSession,
    session_store::Seq,
};

type ScriptedResponse = Result<CompletionResponse<Value>, CompletionError>;

#[derive(Clone, Default)]
struct ScriptedModel {
    responses: Arc<Mutex<VecDeque<ScriptedResponse>>>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl ScriptedModel {
    fn new(response: ScriptedResponse) -> Self {
        Self {
            responses: Arc::new(Mutex::new(VecDeque::from([response]))),
            requests: Arc::default(),
        }
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().expect("requests lock").clone()
    }
}

impl CompletionModel for ScriptedModel {
    type Response = Value;
    type Client = ();

    fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
        Self::default()
    }

    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        self.requests.lock().expect("requests lock").push(request);
        self.responses
            .lock()
            .expect("responses lock")
            .pop_front()
            .expect("scripted response")
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionStream, CompletionError> {
        panic!("summarizer must not use streaming")
    }
}

#[tokio::test]
async fn valid_output_uses_one_non_streaming_tool_free_request() {
    let model = ScriptedModel::new(Ok(response(AssistantContent::text(valid_summary_json()))));
    let summarizer = LlmContextSummarizer::new(model.clone(), 2_048)
        .expect("summary output budget should be valid");
    let session = AgentSession::from_messages(vec![Message::user("durable source")]);
    let original_messages = session.messages().to_vec();

    let generated = summarizer
        .summarize(request_with_messages(original_messages.clone()))
        .await
        .expect("valid summary should be accepted");

    assert_eq!(generated.summary.version, CONTINUITY_SUMMARY_VERSION);
    assert_eq!(generated.usage.total_tokens, 8);
    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    let sent = &requests[0];
    assert!(sent.model.is_none());
    assert_eq!(sent.chat_history.len(), 2);
    assert!(sent.tools.is_empty());
    assert_eq!(sent.tool_choice, Some(ToolChoice::None));
    assert_eq!(sent.temperature, Some(0.0));
    assert_eq!(sent.max_tokens, Some(2_048));
    assert_eq!(sent.reasoning, Some(ReasoningEffort::Off));
    assert!(sent.output_schema.is_some());
    assert_eq!(session.messages(), original_messages);
}

#[tokio::test]
async fn rejects_non_json_wrong_version_and_unknown_artifacts() {
    for raw in [
        "```json\n{}\n```".to_string(),
        summary_json_with(serde_json::json!(2), serde_json::json!([])),
        summary_json_with(
            serde_json::json!(1),
            serde_json::json!([
                "tool-result-sha256-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            ]),
        ),
    ] {
        let model = ScriptedModel::new(Ok(response(AssistantContent::text(raw))));
        let error = LlmContextSummarizer::new(model, 2_048)
            .expect("summary output budget should be valid")
            .summarize(request())
            .await
            .expect_err("untrusted output should be rejected");
        assert!(matches!(error, SummarizerError::InvalidSummary { .. }));
        assert_eq!(error.usage(), Some(usage()));
    }
}

#[tokio::test]
async fn rejects_extra_blocks_and_propagates_provider_failure() {
    let response = CompletionResponse {
        choice: NonEmptyVec::from_first_rest(
            AssistantContent::text(valid_summary_json()),
            vec![AssistantContent::tool_call(
                "call-1",
                "compact",
                serde_json::json!({}),
            )],
        ),
        usage: usage(),
        raw_response: serde_json::json!({}),
        message_id: None,
    };
    let shape_error = LlmContextSummarizer::new(ScriptedModel::new(Ok(response)), 2_048)
        .expect("summary output budget should be valid")
        .summarize(request())
        .await
        .expect_err("extra content should be rejected");
    assert!(matches!(
        shape_error,
        SummarizerError::InvalidResponseShape { .. }
    ));
    assert_eq!(shape_error.usage(), Some(usage()));

    let provider_error = LlmContextSummarizer::new(
        ScriptedModel::new(Err(CompletionError::ResponseError("offline".to_string()))),
        2_048,
    )
    .expect("summary output budget should be valid")
    .summarize(request())
    .await
    .expect_err("provider failure should propagate");
    assert!(matches!(
        provider_error,
        SummarizerError::Completion(CompletionError::ResponseError(message)) if message == "offline"
    ));
}

#[test]
fn rejects_zero_output_budget() {
    for invalid in [0, u64::from(u32::MAX) + 1] {
        assert!(matches!(
            LlmContextSummarizer::new(ScriptedModel::default(), invalid),
            Err(SummarizerError::InvalidOutputBudget { actual, .. }) if actual == invalid
        ));
    }
}

fn request() -> SummaryRequest {
    request_with_messages(vec![Message::user("durable source")])
}

fn request_with_messages(source_messages: Vec<Message>) -> SummaryRequest {
    let context = SummaryValidationContext::new(
        Seq::new(2),
        Seq::new(8),
        Seq::new(8),
        std::iter::empty::<&str>(),
    )
    .expect("validation source should be valid");
    SummaryRequest::new(None, source_messages, context).expect("summary request should be valid")
}

fn response(content: AssistantContent) -> CompletionResponse<Value> {
    CompletionResponse {
        choice: NonEmptyVec::new(content),
        usage: usage(),
        raw_response: serde_json::json!({}),
        message_id: None,
    }
}

fn usage() -> Usage {
    Usage {
        input_tokens: 5,
        output_tokens: 3,
        total_tokens: 8,
        ..Usage::default()
    }
}

fn valid_summary_json() -> String {
    summary_json_with(serde_json::json!(1), serde_json::json!([]))
}

fn summary_json_with(version: Value, artifact_refs: Value) -> String {
    serde_json::json!({
        "schema_version": version,
        "source_seq_start": 2,
        "source_seq_end": 8,
        "current_goal": "Continue implementation",
        "constraints": [],
        "decisions": [],
        "completed_work": [],
        "workspace": {
            "working_directory": "/workspace",
            "files": [],
            "symbols": []
        },
        "commands_and_tests": [],
        "unresolved_errors": [],
        "todos": [],
        "next_actions": [],
        "artifact_refs": artifact_refs
    })
    .to_string()
}
