//! Versioned semantic continuity summaries and their untrusted-input prompt.

mod projection;
mod prompt;
mod summarizer;
mod types;
mod validation;

pub(crate) use projection::{
    COMPACTED_CONTEXT_SYSTEM_INSTRUCTION, is_compacted_context_message, project_summary_message,
};
pub use prompt::{SummaryRequest, build_summary_prompt};
pub use summarizer::{ContextSummarizer, GeneratedSummary, LlmContextSummarizer, SummarizerError};
pub use types::{
    CONTINUITY_SUMMARY_VERSION, CommandSummary, ContinuitySummary, SummaryTodo, SummaryTodoStatus,
    WorkspaceSummary,
};
pub use validation::SummaryError;

/// Returns the structured-output schema for [`ContinuitySummary`].
///
/// # Errors
/// Returns [`SummaryError::SchemaEncoding`] if the generated schema cannot be
/// represented as JSON.
pub fn continuity_summary_schema() -> Result<serde_json::Value, SummaryError> {
    serde_json::to_value(schemars::schema_for!(ContinuitySummary))
        .map_err(|error| SummaryError::SchemaEncoding(error.to_string()))
}

#[cfg(test)]
mod tests;
