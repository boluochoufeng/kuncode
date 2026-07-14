//! Runs semantic compaction as an isolated, tool-free model operation.
//!
//! Provider structured-output support narrows the expected shape but is not a
//! trust boundary. The response is still untrusted until request-bound decoding,
//! provenance checks, semantic checks, and resource bounds all succeed.

mod attempt;

use std::num::NonZeroU32;

use async_trait::async_trait;
use kuncode_core::completion::{CompletionError, CompletionModel, Usage};
use thiserror::Error;

use self::attempt::{AttemptError, aggregate_usage, run_attempt};
use super::{
    ContinuitySummary, SummaryError, SummaryRequest, build_summary_prompt,
    continuity_summary_schema,
    prompt::{SummaryCorrection, build_summary_correction_prompt},
};

/// Validated semantic output and provider accounting from one isolated operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedSummary {
    /// Summary that passed every request-bound deterministic gate.
    pub summary: ContinuitySummary,
    /// Aggregated provider usage kept separate from the normal conversation loop.
    pub usage: Usage,
}

/// Failures produced by the isolated semantic-summary call.
#[derive(Debug, Error)]
pub enum SummarizerError {
    /// The caller cancelled before the isolated model call completed.
    #[error("summary completion was cancelled")]
    Cancelled,
    /// The configured generation budget cannot be represented by providers.
    #[error("summary output token budget {actual} must be within 1..={max}")]
    InvalidOutputBudget {
        /// Largest cross-provider budget represented without narrowing.
        max: u64,
        /// Caller-supplied budget rejected at construction.
        actual: u64,
    },
    /// Prompt construction or schema generation failed before dispatch.
    #[error("invalid summary request: {0}")]
    InvalidRequest(#[source] SummaryError),
    /// The provider rejected or failed the isolated completion request.
    #[error("summary completion failed: {0}")]
    Completion(#[from] CompletionError),
    /// Correction prompt construction failed after the first rejected response.
    #[error("invalid summary correction request: {source}")]
    CorrectionRequest {
        /// Local construction failure for the request-bound correction prompt.
        source: SummaryError,
        /// Usage already incurred by the first rejected response.
        usage: Usage,
    },
    /// The provider failed while dispatching the one allowed correction request.
    #[error("summary correction completion failed: {source}")]
    CorrectionCompletion {
        /// Provider failure from the correction request.
        source: CompletionError,
        /// Usage already incurred by the first rejected response.
        usage: Usage,
    },
    /// Structured output must be the response's only assistant content block.
    #[error("summary completion must return exactly one text content block")]
    InvalidResponseShape {
        /// Usage already incurred by the rejected provider response.
        usage: Usage,
    },
    /// The provider response failed strict request-bound validation.
    #[error("invalid semantic summary: {source}")]
    InvalidSummary {
        /// Deterministic reason the untrusted output was rejected.
        source: SummaryError,
        /// Usage already incurred by the rejected provider response.
        usage: Usage,
    },
}

impl SummarizerError {
    /// Returns provider usage when a response was received before rejection.
    pub const fn usage(&self) -> Option<Usage> {
        match self {
            Self::CorrectionRequest { usage, .. }
            | Self::CorrectionCompletion { usage, .. }
            | Self::InvalidResponseShape { usage }
            | Self::InvalidSummary { usage, .. } => Some(*usage),
            Self::Cancelled
            | Self::InvalidOutputBudget { .. }
            | Self::InvalidRequest(_)
            | Self::Completion(_) => None,
        }
    }
}

/// Produces a validated summary without mutating conversation state.
#[async_trait]
pub trait ContextSummarizer: Send + Sync {
    /// Runs one isolated, non-streaming summary operation.
    ///
    /// # Errors
    /// Returns [`SummarizerError`] when dispatch or any deterministic gate fails.
    async fn summarize(&self, request: SummaryRequest)
    -> Result<GeneratedSummary, SummarizerError>;
}

/// No-tool summarizer backed by the same provider abstraction as the agent.
///
/// Disabling tools and reasoning isolates compression from the normal agent loop;
/// the generated text cannot directly schedule actions or mutate conversation state.
pub struct LlmContextSummarizer<M> {
    model: M,
    max_output_tokens: NonZeroU32,
}

impl<M> LlmContextSummarizer<M> {
    /// Binds a model to an independent summary output budget.
    ///
    /// # Errors
    /// Returns [`SummarizerError::InvalidOutputBudget`] unless the budget fits
    /// every provider's unsigned 32-bit request field.
    pub fn new(model: M, max_output_tokens: u64) -> Result<Self, SummarizerError> {
        let max_output_tokens = u32::try_from(max_output_tokens)
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or(SummarizerError::InvalidOutputBudget {
                max: u64::from(u32::MAX),
                actual: max_output_tokens,
            })?;
        Ok(Self {
            model,
            max_output_tokens,
        })
    }
}

#[async_trait]
impl<M> ContextSummarizer for LlmContextSummarizer<M>
where
    M: CompletionModel,
{
    async fn summarize(
        &self,
        request: SummaryRequest,
    ) -> Result<GeneratedSummary, SummarizerError> {
        let prompt = build_summary_prompt(&request).map_err(SummarizerError::InvalidRequest)?;
        let schema = continuity_summary_schema().map_err(SummarizerError::InvalidRequest)?;
        let max_output_tokens = u64::from(self.max_output_tokens.get());
        let (correction, first_usage) = match run_attempt(
            &self.model,
            &request,
            prompt,
            schema.clone(),
            max_output_tokens,
        )
        .await
        {
            Ok(generated) => return Ok(generated),
            Err(AttemptError::Completion(source)) => {
                return Err(SummarizerError::Completion(source));
            }
            Err(AttemptError::InvalidResponseShape(usage)) => {
                (SummaryCorrection::InvalidResponseShape, usage)
            }
            Err(AttemptError::InvalidSummary { usage, .. }) => {
                (SummaryCorrection::InvalidSummary, usage)
            }
        };
        let correction_prompt =
            build_summary_correction_prompt(&request, correction).map_err(|source| {
                SummarizerError::CorrectionRequest {
                    source,
                    usage: first_usage,
                }
            })?;

        match run_attempt(
            &self.model,
            &request,
            correction_prompt,
            schema,
            max_output_tokens,
        )
        .await
        {
            Ok(mut generated) => {
                generated.usage = aggregate_usage(first_usage, generated.usage);
                Ok(generated)
            }
            Err(AttemptError::Completion(source)) => Err(SummarizerError::CorrectionCompletion {
                source,
                usage: first_usage,
            }),
            Err(AttemptError::InvalidResponseShape(usage)) => {
                Err(SummarizerError::InvalidResponseShape {
                    usage: aggregate_usage(first_usage, usage),
                })
            }
            Err(AttemptError::InvalidSummary { source, usage }) => {
                Err(SummarizerError::InvalidSummary {
                    source,
                    usage: aggregate_usage(first_usage, usage),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use kuncode_core::{
        completion::{
            AssistantContent, CompletionError, CompletionModel, CompletionRequest,
            CompletionResponse, CompletionStream, Message, ReasoningEffort, ToolChoice, Usage,
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
            Self::with_responses([response])
        }

        fn with_responses(responses: impl IntoIterator<Item = ScriptedResponse>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into_iter().collect())),
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
    async fn invalid_summary_gets_one_safe_correction_retry_and_aggregates_usage() {
        let rejected_output = "RAW_INVALID_SUMMARY_SENTINEL";
        let model = ScriptedModel::with_responses([
            Ok(response(AssistantContent::text(rejected_output))),
            Ok(response(AssistantContent::text(valid_summary_json()))),
        ]);
        let summarizer = LlmContextSummarizer::new(model.clone(), 2_048)
            .expect("summary output budget should be valid");

        let generated = summarizer
            .summarize(request())
            .await
            .expect("a corrected summary should be accepted");

        assert_eq!(generated.usage, usage() + usage());
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].chat_history[1], requests[1].chat_history[1]);
        assert_eq!(requests[0].output_schema, requests[1].output_schema);
        let Message::System { content } = requests[1].chat_history.first() else {
            panic!("correction request should keep system authority first");
        };
        assert!(content.contains("invalid_summary"));
        assert!(!content.contains(rejected_output));
        assert!(
            !serde_json::to_string(&requests[1].chat_history)
                .expect("correction prompt should encode")
                .contains(rejected_output)
        );
    }

    #[tokio::test]
    async fn invalid_response_shape_gets_one_correction_retry() {
        let invalid_shape = CompletionResponse {
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
        let model = ScriptedModel::with_responses([
            Ok(invalid_shape),
            Ok(response(AssistantContent::text(valid_summary_json()))),
        ]);

        let generated = LlmContextSummarizer::new(model.clone(), 2_048)
            .expect("summary output budget should be valid")
            .summarize(request())
            .await
            .expect("a corrected response shape should be accepted");

        assert_eq!(generated.usage, usage() + usage());
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        let Message::System { content } = requests[1].chat_history.first() else {
            panic!("correction request should keep system authority first");
        };
        assert!(content.contains("invalid_response_shape"));
    }

    #[tokio::test]
    async fn second_invalid_summary_fails_closed_without_a_third_call() {
        let model = ScriptedModel::with_responses([
            Ok(response(AssistantContent::text("first invalid output"))),
            Ok(response(AssistantContent::text("second invalid output"))),
            Ok(response(AssistantContent::text(valid_summary_json()))),
        ]);

        let error = LlmContextSummarizer::new(model.clone(), 2_048)
            .expect("summary output budget should be valid")
            .summarize(request())
            .await
            .expect_err("the second rejected summary must fail closed");

        assert!(matches!(error, SummarizerError::InvalidSummary { .. }));
        assert_eq!(error.usage(), Some(usage() + usage()));
        assert_eq!(model.requests().len(), 2);
    }

    #[tokio::test]
    async fn correction_provider_failure_preserves_first_response_usage() {
        let model = ScriptedModel::with_responses([
            Ok(response(AssistantContent::text("invalid output"))),
            Err(CompletionError::ResponseError("offline".to_string())),
        ]);

        let error = LlmContextSummarizer::new(model.clone(), 2_048)
            .expect("summary output budget should be valid")
            .summarize(request())
            .await
            .expect_err("correction provider failure should propagate");

        assert_eq!(error.usage(), Some(usage()));
        assert_eq!(model.requests().len(), 2);
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
            let model = ScriptedModel::with_responses([
                Ok(response(AssistantContent::text(raw.clone()))),
                Ok(response(AssistantContent::text(raw))),
            ]);
            let error = LlmContextSummarizer::new(model, 2_048)
                .expect("summary output budget should be valid")
                .summarize(request())
                .await
                .expect_err("untrusted output should be rejected");
            assert!(matches!(error, SummarizerError::InvalidSummary { .. }));
            assert_eq!(error.usage(), Some(usage() + usage()));
        }
    }

    #[tokio::test]
    async fn rejects_extra_blocks_and_propagates_provider_failure() {
        let invalid_shape = || CompletionResponse {
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
        let shape_error = LlmContextSummarizer::new(
            ScriptedModel::with_responses([Ok(invalid_shape()), Ok(invalid_shape())]),
            2_048,
        )
        .expect("summary output budget should be valid")
        .summarize(request())
        .await
        .expect_err("extra content should be rejected");
        assert!(matches!(
            shape_error,
            SummarizerError::InvalidResponseShape { .. }
        ));
        assert_eq!(shape_error.usage(), Some(usage() + usage()));

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
        SummaryRequest::new(None, source_messages, context)
            .expect("summary request should be valid")
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
}
