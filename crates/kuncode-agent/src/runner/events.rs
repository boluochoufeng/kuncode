//! Stable event error classification.

use crate::error::AgentError;

/// Maps an [`AgentError`] to the stable `kind` string on
/// [`EventKind::Error`]. Kept exhaustive so a new variant forces a decision.
pub(super) fn error_kind(error: &AgentError) -> &'static str {
    match error {
        AgentError::Completion(_) => "completion",
        AgentError::Tool { .. } => "tool",
        AgentError::EmptyTranscript => "empty_transcript",
        AgentError::Compaction { .. } => "compaction",
        AgentError::Cancelled => "cancelled",
        AgentError::PromptBlocked { .. } => "prompt_blocked",
        AgentError::MaxIterations { .. } => "max_iterations",
    }
}
