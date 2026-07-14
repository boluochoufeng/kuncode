//! Executes one isolated provider call and validates its untrusted response.

use kuncode_core::{
    completion::{
        AssistantContent, CompletionError, CompletionModel, CompletionRequestBuilder, Message,
        ReasoningEffort, ToolChoice, Usage,
    },
    non_empty_vec::NonEmptyVec,
};

use super::GeneratedSummary;
use crate::compaction::summary::{SummaryError, SummaryRequest};

pub(super) enum AttemptError {
    Completion(CompletionError),
    InvalidResponseShape(Usage),
    InvalidSummary { source: SummaryError, usage: Usage },
}

pub(super) fn aggregate_usage(left: Usage, right: Usage) -> Usage {
    Usage {
        input_tokens: left.input_tokens.saturating_add(right.input_tokens),
        output_tokens: left.output_tokens.saturating_add(right.output_tokens),
        total_tokens: left.total_tokens.saturating_add(right.total_tokens),
        cached_input_tokens: left
            .cached_input_tokens
            .saturating_add(right.cached_input_tokens),
        cache_creation_input_tokens: left
            .cache_creation_input_tokens
            .saturating_add(right.cache_creation_input_tokens),
        reasoning_tokens: left.reasoning_tokens.saturating_add(right.reasoning_tokens),
    }
}

pub(super) async fn run_attempt<M>(
    model: &M,
    request: &SummaryRequest,
    prompt: NonEmptyVec<Message>,
    schema: serde_json::Value,
    max_output_tokens: u64,
) -> Result<GeneratedSummary, AttemptError>
where
    M: CompletionModel,
{
    let completion = CompletionRequestBuilder::from_messages(prompt)
        .temperature(Some(0.0))
        .max_tokens(Some(max_output_tokens))
        .reasoning(Some(ReasoningEffort::Off))
        .tool_choice(Some(ToolChoice::None))
        .output_schema(Some(schema))
        .build();
    let response = model
        .completion(completion)
        .await
        .map_err(AttemptError::Completion)?;
    if response.choice.len() != 1 {
        return Err(AttemptError::InvalidResponseShape(response.usage));
    }
    let AssistantContent::Text(text) = response.choice.first() else {
        return Err(AttemptError::InvalidResponseShape(response.usage));
    };
    let summary = request
        .parse_and_validate(text.text_ref())
        .map_err(|source| AttemptError::InvalidSummary {
            source,
            usage: response.usage,
        })?;
    Ok(GeneratedSummary {
        summary,
        usage: response.usage,
    })
}
