//! Error types for agent orchestration.

use kuncode_core::completion::{CompletionError, Message, Usage};
use thiserror::Error;

use crate::tool::ToolError;

/// Failures that stop the agent loop itself.
#[derive(Debug, Error)]
pub enum AgentError {
    /// The completion provider rejected, failed, or could not decode a request.
    #[error("completion failed: {0}")]
    Completion(#[from] CompletionError),

    /// A tool failed at the harness boundary rather than returning a
    /// model-recoverable [`crate::tool::ToolOutput`].
    #[error("tool `{name}` failed at the harness boundary: {source}")]
    Tool {
        /// Tool name requested by the model.
        name: String,
        /// Harness-level tool failure.
        #[source]
        source: ToolError,
    },

    /// Runner was asked to continue an empty transcript.
    #[error("agent transcript is empty")]
    EmptyTranscript,

    /// The turn was cancelled — a user interrupt, or an `Abort` at an approval
    /// prompt. Distinct from a tool failure so callers (e.g. the CLI) can tell a
    /// deliberate Ctrl-C apart from a real error.
    #[error("agent turn was cancelled")]
    Cancelled,

    /// The model kept requesting tools until the loop budget was exhausted.
    #[error("agent exceeded max iterations ({max_iterations}) before producing a final answer")]
    MaxIterations {
        /// Maximum number of model calls allowed for one run.
        max_iterations: usize,
        /// Transcript accumulated before the budget was hit, so callers can
        /// inspect or resume the partial run.
        messages: Vec<Message>,
        /// Provider usage aggregated across the model calls that were made.
        usage: Usage,
    },
}
