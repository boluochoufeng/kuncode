//! Agent loop entry point.

mod cancellation;
mod events;
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
    hook::Hooks,
    observer::AgentObserver,
    permission::{Approver, PermissionPolicy},
    registry::ToolRegistry,
    session::AgentSession,
    session_store::SessionStore,
    system_prompt::SystemPrompt,
    tool::ToolOutput,
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
        }
    }
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
    registry: ToolRegistry,
    config: AgentConfig,
    system_prompt: Arc<SystemPrompt>,
    policy: Arc<PermissionPolicy>,
    approver: Arc<dyn Approver>,
    observer: Option<Arc<dyn AgentObserver>>,
    hooks: Arc<Hooks>,
    session_store: Option<Arc<dyn SessionStore>>,
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

struct CallOutcome {
    output: ToolOutput,
    executed: bool,
}

#[cfg(test)]
mod tests;
