//! Agent loop entry point.

mod cancellation;
mod compaction;
mod iteration;
mod loop_control;
mod request;
mod setup;
mod tool_execution;
mod tool_gate;
mod turn;

use std::sync::Arc;

use kuncode_core::completion::{ReasoningEffort, ToolChoice, Usage};

use crate::{
    compaction::{
        GroupTokenEstimator,
        budget::{CompactionConfig, TokenEstimator},
    },
    hook::Hooks,
    observer::AgentObserver,
    permission::{Approver, PermissionPolicy},
    registry::ToolRegistry,
    session::AgentSession,
    session_store::SessionStore,
    system_prompt::SystemPrompt,
    tool::{ToolOutput, ToolResultRetention},
};

use self::request::final_text_at;

const DEFAULT_MAX_ITERATIONS: usize = 50;

/// Bounds how often a Stop hook may force continuation within one turn.
const STOP_CONTINUATION_LIMIT: usize = 3;

/// Re-surfaces an idle task plan as harness-owned context.
const TODO_REMINDER: &str = "<reminder>Keep the task plan current: call todo_write to mark finished steps completed and set the next one in_progress.</reminder>";

/// Runtime knobs for one agent loop.
#[derive(Clone, Debug)]
pub struct AgentConfig {
    /// Maximum number of model calls before the loop aborts.
    pub max_iterations: usize,
    /// Output token cap passed to each completion request.
    pub max_tokens: Option<u64>,
    /// Reasoning effort passed through to the provider.
    pub reasoning: Option<ReasoningEffort>,
    /// Tool-call policy passed through to the provider.
    pub tool_choice: Option<ToolChoice>,
    /// Consecutive model calls allowed before reminding the model about the plan.
    pub todo_reminder_interval: Option<usize>,
    /// Harness-owned automatic compaction settings, absent to disable all work.
    pub compaction: Option<AgentCompactionConfig>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: DEFAULT_MAX_ITERATIONS,
            max_tokens: Some(32768),
            reasoning: None,
            tool_choice: None,
            // Injecting messages by default would surprise library embedders.
            todo_reminder_interval: None,
            compaction: None,
        }
    }
}

/// Runtime metadata required to commit semantic compaction checkpoints.
#[derive(Clone, Debug)]
pub struct AgentCompactionConfig {
    /// Controls rollout mode and derives all input-budget thresholds.
    policy: CompactionConfig,
    /// Recorded with summary checkpoints so their model provenance is auditable.
    model_id: String,
    /// Independent output cap for the semantic-summary call.
    summary_max_tokens: u64,
}

impl AgentCompactionConfig {
    /// Binds rollout policy to an explicit provider model and output budget.
    ///
    /// # Errors
    /// Returns an error for a blank model id or a budget outside `1..=u32::MAX`.
    pub fn new(
        policy: CompactionConfig,
        model_id: impl Into<String>,
        summary_max_tokens: u64,
    ) -> Result<Self, AgentCompactionConfigError> {
        let model_id = model_id.into();
        if model_id.trim().is_empty() {
            return Err(AgentCompactionConfigError::BlankModelId);
        }
        if summary_max_tokens == 0 || summary_max_tokens > u64::from(u32::MAX) {
            return Err(AgentCompactionConfigError::InvalidSummaryBudget {
                actual: summary_max_tokens,
            });
        }
        Ok(Self {
            policy,
            model_id,
            summary_max_tokens,
        })
    }
}

/// Invalid automatic-compaction runtime metadata.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum AgentCompactionConfigError {
    /// Checkpoint audit records require a concrete model identifier.
    #[error("compaction model id must not be blank")]
    BlankModelId,
    /// Summary requests use a portable non-zero unsigned 32-bit output limit.
    #[error(
        "compaction summary output budget {actual} must be within 1..={}",
        u32::MAX
    )]
    InvalidSummaryBudget {
        /// Rejected caller-supplied budget.
        actual: u64,
    },
}

/// Summary for one completed user turn appended to an existing transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentTurn {
    /// Index of the final assistant message inside the caller-owned transcript.
    pub final_message_index: usize,
    /// Provider usage aggregated across this turn's model calls.
    pub usage: Usage,
    /// Number of model calls performed for this turn.
    pub iterations: usize,
}

impl AgentTurn {
    /// Concatenates visible text blocks from the final assistant message.
    pub fn final_text(&self, session: &AgentSession) -> String {
        final_text_at(session.messages(), self.final_message_index)
    }
}

/// Minimal agent loop for model/tool/model interaction.
#[derive(Clone)]
pub struct AgentRunner<M> {
    model: M,
    // Summary calls may use a separately configured model, but share the same
    // request-boundary cancellation and durable commit rules.
    summary_model: M,
    registry: ToolRegistry,
    config: AgentConfig,
    system_prompt: Arc<SystemPrompt>,
    policy: Arc<PermissionPolicy>,
    approver: Arc<dyn Approver>,
    observer: Option<Arc<dyn AgentObserver>>,
    hooks: Arc<Hooks>,
    // Lossy compaction requires both enabled policy and durable storage; shadow
    // observation remains read-only and does not need persistence.
    session_store: Option<Arc<dyn SessionStore>>,
    // Both estimators must report provider-visible units so trigger, pass, and
    // final-request accounting remain comparable.
    token_estimator: Arc<dyn TokenEstimator>,
    group_estimator: Arc<dyn GroupTokenEstimator>,
}

#[derive(Debug)]
struct IterationResult {
    assistant_message_index: usize,
    usage: Usage,
    tool_calls: Vec<PendingToolCall>,
}

#[derive(Debug)]
struct PendingToolCall {
    id: String,
    call_id: Option<String>,
    name: String,
    arguments: serde_json::Value,
}

// Keeps execution provenance beside retention authorization until the result is
// appended. Rejected or synthetic results must remain verbatim.
struct CallOutcome {
    output: ToolOutput,
    executed: bool,
    // Only an executed tool can grant a non-verbatim retention policy from its
    // authoritative arguments and output; payload shape alone is insufficient.
    retention: ToolResultRetention,
}

#[cfg(test)]
mod tests;
