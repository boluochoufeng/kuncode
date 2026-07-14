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
#[path = "summarizer/tests.rs"]
mod tests;
