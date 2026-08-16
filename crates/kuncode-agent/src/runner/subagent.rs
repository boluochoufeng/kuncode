//! Runner-side subagent execution behind the `task` tool.
//!
//! The driver is built per tool call from the live runner, so a delegation
//! always uses the current model, hooks, policy, and approval channel. The
//! subagent session is fresh — only the delegated prompt enters it — but it
//! inherits the parent session's permission overlay (mode + session grants), so
//! delegation can never widen what the user has allowed, and never re-asks for
//! what they already granted this session.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kuncode_core::completion::{CompletionModel, Usage};
use tokio_util::sync::CancellationToken;

use crate::{
    error::AgentError,
    permission::SessionPolicyOverlay,
    session::AgentSession,
    tool::{
        ToolErrorKind,
        task::{SubagentDriver, SubagentFailure, SubagentOutcome, TASK_TOOL_NAME},
    },
};

use super::AgentRunner;

/// Batch-scoped accumulator for provider usage consumed inside subagent runs.
///
/// Subagent model calls happen inside a tool call, outside the loop's own
/// iteration accounting; this meter is how they still end up in the turn's
/// [`Usage`]. Shared by every driver of one batch and drained once at its end.
pub(super) type SubagentUsageMeter = Arc<Mutex<Usage>>;

/// Reads the accumulated subagent usage for one finished batch.
pub(super) fn accrued_usage(meter: &SubagentUsageMeter) -> Usage {
    *meter.lock().expect("subagent usage meter")
}

/// One turn's delegation seam: a snapshot of the runner plus the parent
/// session's permission overlay, taken right before the tool call it serves.
struct TurnSubagents<M> {
    runner: AgentRunner<M>,
    overlay: SessionPolicyOverlay,
    meter: SubagentUsageMeter,
}

impl<M> AgentRunner<M>
where
    M: CompletionModel + 'static,
{
    /// Builds the driver injected into a tool call's context, or `None` when
    /// no registered tool can delegate (which keeps non-delegating loops free
    /// of the snapshot cost).
    pub(super) fn turn_subagent_driver(
        &self,
        session: &AgentSession,
        meter: &SubagentUsageMeter,
    ) -> Option<Arc<dyn SubagentDriver>> {
        self.registry.registered(TASK_TOOL_NAME)?;
        Some(Arc::new(TurnSubagents {
            runner: self.clone(),
            overlay: session.permissions().clone(),
            meter: meter.clone(),
        }))
    }

    /// Derives the runner a subagent turn executes on.
    ///
    /// Same model, system prompt, policy, approvals, and hooks as the parent —
    /// the permission boundary must not depend on which loop makes a call.
    /// What differs is deliberate: the registry loses `task` (no infinite
    /// delegation), and persistence, compaction, plan nagging, and the
    /// observer are dropped because the throwaway session has no durable
    /// journal to compact into and the frontend cannot yet render nested
    /// events (tracing still records the run).
    fn subagent_runner(&self) -> AgentRunner<M> {
        let mut sub = self.clone();
        sub.registry = self.registry.without_tool(TASK_TOOL_NAME);
        sub.config.compaction = None;
        sub.config.todo_reminder_interval = None;
        sub.session_store = None;
        sub.observer = None;
        sub
    }
}

impl<M> TurnSubagents<M> {
    fn add_usage(&self, usage: Usage) {
        *self.meter.lock().expect("subagent usage meter") += usage;
    }
}

#[async_trait]
impl<M> SubagentDriver for TurnSubagents<M>
where
    M: CompletionModel + 'static,
{
    async fn run(
        &self,
        description: &str,
        prompt: &str,
        cancel: &CancellationToken,
    ) -> Result<SubagentOutcome, SubagentFailure> {
        let runner = self.runner.subagent_runner();
        let mut session = AgentSession::new();
        *session.permissions_mut() = self.overlay.clone();
        tracing::info!(
            target: "kuncode::subagent",
            description,
            prompt_chars = prompt.chars().count(),
            "subagent turn started",
        );
        match runner
            .run_turn_with(&mut session, prompt, cancel.clone())
            .await
        {
            Ok(turn) => {
                self.add_usage(turn.usage);
                tracing::info!(
                    target: "kuncode::subagent",
                    description,
                    iterations = turn.iterations,
                    total_tokens = turn.usage.total_tokens,
                    "subagent turn completed",
                );
                Ok(SubagentOutcome {
                    report: turn.final_text(&session),
                    usage: turn.usage,
                    iterations: turn.iterations,
                })
            }
            Err(error) => {
                tracing::warn!(
                    target: "kuncode::subagent",
                    description,
                    error = %error,
                    "subagent turn failed",
                );
                Err(classify_failure(error, |usage| self.add_usage(usage)))
            }
        }
    }
}

/// Maps a subagent's turn failure onto the model-recoverable tool vocabulary.
///
/// `account` receives usage the failed run still consumed, where the error
/// carries it — tokens were spent whether or not a report came back.
fn classify_failure(error: AgentError, account: impl FnOnce(Usage)) -> SubagentFailure {
    match error {
        AgentError::Cancelled => SubagentFailure::new(
            ToolErrorKind::Cancelled,
            "subagent turn was interrupted before it produced a report",
        ),
        AgentError::MaxIterations {
            max_iterations,
            usage,
            ..
        } => {
            account(usage);
            SubagentFailure::new(
                "subagent_max_iterations",
                format!(
                    "subagent used its whole budget of {max_iterations} model calls \
                     without producing a final report; retry with a narrower prompt"
                ),
            )
        }
        other => SubagentFailure::new("subagent_failed", other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuncode_core::completion::Message;

    #[test]
    fn max_iterations_failures_still_account_their_usage() {
        let spent = Usage {
            input_tokens: 5,
            output_tokens: 7,
            total_tokens: 12,
            ..Usage::default()
        };
        let mut accounted = Usage::default();

        let failure = classify_failure(
            AgentError::MaxIterations {
                max_iterations: 3,
                messages: vec![Message::user("hi")],
                usage: spent,
            },
            |usage| accounted += usage,
        );

        assert_eq!(accounted, spent);
        assert_eq!(failure.kind.as_str(), "subagent_max_iterations");
    }

    #[test]
    fn cancellation_maps_to_the_cancelled_kind() {
        let failure = classify_failure(AgentError::Cancelled, |_| {
            panic!("cancellation carries no usage")
        });

        assert_eq!(failure.kind, ToolErrorKind::Cancelled);
    }
}
