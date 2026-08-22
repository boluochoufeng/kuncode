//! Runner-side subagent execution behind the `task` tool.
//!
//! The driver is built per tool call from the live runner, so a delegation
//! always uses the current model, hooks, policy, and approval channel. The
//! requested agent type decides the starting transcript — fresh for most
//! types, a copy of the parent conversation for `fork` — and may narrow the
//! tool set or append its own instructions. Every shape inherits the parent
//! session's permission overlay (mode + session grants), so delegation can
//! never widen what the user has allowed, and never re-asks for what they
//! already granted this session. The run's own events flow back to the parent
//! observer wrapped in [`EventKind::Subagent`] envelopes, so frontends can
//! show nested progress under the delegating `task` row.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kuncode_core::completion::{AssistantContent, CompletionModel, Message, Usage};
use tokio_util::sync::CancellationToken;

use crate::{
    agent_type::{AgentType, SubagentContext},
    error::AgentError,
    observer::{AgentEvent, AgentObserver, EventKind},
    permission::SessionPolicyOverlay,
    session::AgentSession,
    system_prompt::{IdentitySection, SystemPrompt},
    tool::{
        ToolErrorKind,
        task::{SubagentDriver, SubagentFailure, SubagentOutcome, SubagentRequest, TASK_TOOL_NAME},
    },
};

use super::{AgentRunner, PendingToolCall};

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
    /// Id of the tool call this driver serves — the attribution key nested
    /// events are enveloped with.
    tool_call_id: String,
    /// Parent transcript copy serving a possible `fork` delegation. Captured
    /// only when the call being served is `task` itself, and `take`n by the
    /// single run this driver serves.
    fork_messages: Mutex<Option<Vec<Message>>>,
    meter: SubagentUsageMeter,
}

/// Re-emits a subagent's events through the parent observer, each wrapped in
/// a [`EventKind::Subagent`] envelope naming the delegating tool call — so a
/// frontend can render nested progress without mistaking the inner stream for
/// parent-session events.
struct SubagentEventRelay {
    parent: Arc<dyn AgentObserver>,
    parent_tool_call_id: String,
}

impl AgentObserver for SubagentEventRelay {
    fn on_event(&self, event: &AgentEvent) {
        let envelope = AgentEvent {
            // Mirrors the inner event's ordering fields: the parent session's
            // counter is unreachable here (the session is mutably borrowed by
            // the batch executing this delegation), and per-run ordering is
            // what a nested renderer needs. See the variant docs.
            seq: event.seq,
            iteration: event.iteration,
            kind: EventKind::Subagent {
                parent_tool_call_id: self.parent_tool_call_id.clone(),
                event: Box::new(event.clone()),
            },
        };
        self.parent.on_event(&envelope);
    }
}

impl<M> AgentRunner<M>
where
    M: CompletionModel,
{
    /// Builds the driver injected into a tool call's context, or `None` when
    /// no registered tool can delegate (which keeps non-delegating loops free
    /// of the snapshot cost).
    pub(super) fn turn_subagent_driver(
        &self,
        session: &AgentSession,
        meter: &SubagentUsageMeter,
        call: &PendingToolCall,
    ) -> Option<Arc<dyn SubagentDriver>> {
        self.registry.registered(TASK_TOOL_NAME)?;
        // Only a `task` call can request a fork, so the transcript copy is
        // paid per delegation, not per tool call in the batch.
        let fork_messages = (call.name == TASK_TOOL_NAME).then(|| session.messages().to_vec());
        Some(Arc::new(TurnSubagents {
            runner: self.clone(),
            overlay: session.permissions().clone(),
            tool_call_id: call.id.clone(),
            fork_messages: Mutex::new(fork_messages),
            meter: meter.clone(),
        }))
    }

    /// Derives the runner a subagent turn executes on.
    ///
    /// Same policy, approvals, and hooks as the parent — the permission
    /// boundary must not depend on which loop makes a call. What differs is
    /// deliberate: the registry loses `task` (no infinite delegation) and is
    /// narrowed further when the agent type carries a whitelist; a type's
    /// extra instructions render after the parent's whole prompt so the
    /// environment and project-instruction blocks survive; a type with a
    /// model override runs on that model and budget instead of the turn's.
    /// Persistence, compaction, and plan nagging are dropped because the
    /// throwaway session has no durable journal to compact into. The observer
    /// is dropped *here* and re-attached by the driver as an enveloping relay,
    /// so the raw inner stream never reaches the frontend unmarked.
    fn subagent_runner(&self, agent_type: &AgentType) -> AgentRunner<M> {
        let mut sub = self.clone();
        sub.registry = self.registry.without_tool(TASK_TOOL_NAME);
        if let Some(entry) = self.subagent_models.get(agent_type.name()) {
            sub.model = entry.model.clone();
            sub.config.max_tokens = Some(entry.max_tokens);
        }
        if let Some(tools) = agent_type.tools() {
            let missing: Vec<&str> = tools
                .iter()
                .map(String::as_str)
                .filter(|name| sub.registry.registered(name).is_none())
                .collect();
            if !missing.is_empty() {
                tracing::warn!(
                    target: "kuncode::subagent",
                    agent_type = %agent_type.name(),
                    missing = %missing.join(", "),
                    "agent type whitelists tools unavailable to subagents; ignoring those names",
                );
            }
            // Filtering the already task-less registry means a whitelist can
            // never smuggle `task` back in.
            sub.registry = sub.registry.keeping_tools(tools);
        }
        if let Some(instructions) = agent_type.instructions() {
            sub.system_prompt = Arc::new(SystemPrompt::new(vec![
                Box::new(self.system_prompt.clone()),
                Box::new(IdentitySection::new(instructions)),
            ]));
        }
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
    M: CompletionModel,
{
    async fn run(
        &self,
        request: SubagentRequest,
        cancel: &CancellationToken,
    ) -> Result<SubagentOutcome, SubagentFailure> {
        let SubagentRequest {
            description,
            prompt,
            agent_type,
        } = request;
        let mut runner = self.runner.subagent_runner(&agent_type);
        // Nested progress reaches the parent frontend wrapped in `Subagent`
        // envelopes carrying this delegation's tool call id.
        if let Some(parent) = self.runner.observer.clone() {
            runner.observer = Some(Arc::new(SubagentEventRelay {
                parent,
                parent_tool_call_id: self.tool_call_id.clone(),
            }));
        }
        let mut session = match agent_type.context() {
            SubagentContext::Fresh => AgentSession::new(),
            SubagentContext::Fork => {
                let snapshot = self.fork_messages.lock().expect("fork snapshot").take();
                let Some(mut messages) = snapshot else {
                    // Unreachable through the runner (the snapshot is always
                    // captured for a `task` call), but a driver reached any
                    // other way must fail the delegation, not panic.
                    return Err(SubagentFailure::new(
                        "subagent_failed",
                        "no conversation snapshot is available for a fork delegation",
                    ));
                };
                trim_unpaired_tail(&mut messages);
                AgentSession::from_messages(messages)
            }
        };
        *session.permissions_mut() = self.overlay.clone();
        tracing::info!(
            target: "kuncode::subagent",
            description = %description,
            agent_type = %agent_type.name(),
            model = self
                .runner
                .subagent_models
                .get(agent_type.name())
                .map_or("inherit", |entry| entry.model_name.as_str()),
            prompt_chars = prompt.chars().count(),
            starting_messages = session.messages().len(),
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
                    description = %description,
                    agent_type = %agent_type.name(),
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
                    description = %description,
                    agent_type = %agent_type.name(),
                    error = %error,
                    "subagent turn failed",
                );
                Err(classify_failure(error, |usage| self.add_usage(usage)))
            }
        }
    }
}

/// Drops the parent's in-flight tool batch from a fork snapshot.
///
/// The snapshot is taken while the parent is mid-batch: its last assistant
/// message holds tool calls whose results are not all recorded yet — the
/// delegation being served never is. Providers reject a transcript with an
/// unpaired tool call, so the fork starts from the conversation as it stood
/// before that batch.
fn trim_unpaired_tail(messages: &mut Vec<Message>) {
    let Some(last_assistant) = messages
        .iter()
        .rposition(|message| matches!(message, Message::Assistant { .. }))
    else {
        return;
    };
    let has_tool_calls = matches!(
        &messages[last_assistant],
        Message::Assistant { content, .. }
            if content.iter().any(|block| matches!(block, AssistantContent::ToolCall(_)))
    );
    if has_tool_calls {
        messages.truncate(last_assistant);
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
    use kuncode_core::{completion::Message, non_empty_vec::NonEmptyVec};

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

    #[test]
    fn a_fork_snapshot_drops_the_in_flight_tool_batch() {
        let mut messages = vec![
            Message::user("question"),
            Message::assistant("answer"),
            Message::Assistant {
                id: None,
                content: NonEmptyVec::new(AssistantContent::tool_call(
                    "call_task",
                    "task",
                    serde_json::json!({}),
                )),
            },
            // An earlier call of the same batch, already paired; it goes with
            // the batch because its request message does.
            Message::tool_result("call_earlier", "earlier result"),
        ];

        trim_unpaired_tail(&mut messages);

        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[1], Message::Assistant { .. }));
    }

    #[test]
    fn a_fork_snapshot_keeps_a_plain_assistant_tail() {
        let mut messages = vec![Message::user("question"), Message::assistant("answer")];

        trim_unpaired_tail(&mut messages);

        assert_eq!(messages.len(), 2);
    }
}
