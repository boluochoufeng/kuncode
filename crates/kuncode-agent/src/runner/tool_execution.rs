//! Tool-batch execution, result pairing, and transcript recording.

use std::time::Instant;

use kuncode_core::{
    completion::{
        AssistantContent, CompletionModel, Message, ToolResult, ToolResultContent, UserContent,
    },
    non_empty_vec::NonEmptyVec,
};
use tokio_util::sync::CancellationToken;

use crate::{
    error::AgentError,
    hook::{PostToolCx, PostToolOutcome},
    observer::{EventKind, ToolFailure},
    session::AgentSession,
    tool::{ToolContext, ToolErrorKind, ToolOutput, ToolResultRetention},
};

use super::{AgentRunner, CallOutcome, PendingToolCall, cancellation::cancellable};

impl<M> AgentRunner<M>
where
    M: CompletionModel,
{
    pub(super) async fn execute_tool_calls(
        &self,
        session: &mut AgentSession,
        tool_calls: Vec<PendingToolCall>,
        iteration: usize,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        // `PostToolUse` feedback is buffered and flushed only after the whole
        // batch's tool_results are written: a tool_call's results must stay
        // contiguous in the next user content, so a feedback message must not be
        // interleaved between two tool_results (providers reject that).
        let mut feedback = Vec::new();

        for index in 0..tool_calls.len() {
            let ctx = ToolContext::with_cancel(cancel.clone()).with_todos(session.todo_handle());
            let id = tool_calls[index].id.clone();
            let call_id = tool_calls[index].call_id.clone();
            let name = tool_calls[index].name.clone();
            let arguments = tool_calls[index].arguments.clone();
            // Snapshot the plan generation so a successful `todo_write` is
            // detected by the counter advancing — generic, no tool-name check.
            let todo_generation = session.todo_generation();
            let started = Instant::now();

            match self
                .authorized_call(session, iteration, &id, &name, arguments, &ctx)
                .await
            {
                Ok(outcome) => {
                    tracing::debug!(
                        target: "kuncode::tool",
                        iteration,
                        tool_call_id = %id,
                        tool = %name,
                        executed = outcome.executed,
                        ok = outcome.output.ok,
                        error_kind = outcome
                            .output
                            .error
                            .as_ref()
                            .map_or("-", |error| error.kind.as_str()),
                        pipeline_latency_ms = elapsed_ms(started),
                        "tool call pipeline finished",
                    );
                    // Snapshot the output for PostToolUse before record_result
                    // consumes it — only when a hook could actually use it.
                    let post_output = (outcome.executed && !self.hooks.is_empty())
                        .then(|| outcome.output.clone());
                    self.record_result(session, iteration, id, call_id, &name, outcome)
                        .await;

                    // The plan changed iff the generation advanced (a validation
                    // failure does not bump it, so a rejected `todo_write` emits
                    // no update). Fire after `ToolEnd`, before `PostToolUse`, so a
                    // hook that cancels still leaves the plan render correct.
                    if session.todo_generation() != todo_generation {
                        let todos = session.todos_snapshot();
                        self.emit(session, Some(iteration), EventKind::TodoUpdate { todos });
                    }

                    if let Some(output) = post_output {
                        let outcome = {
                            let cx = PostToolCx {
                                tool: &name,
                                output: &output,
                                messages: session.messages(),
                                iteration,
                            };
                            cancellable(cancel, self.hooks.post_tool_use(&cx)).await
                        };
                        match outcome {
                            // Cancelled mid-batch: this tool's result is already
                            // written, so only the *following* calls are unpaired
                            // (re-pairing `index` would double-write it). Buffered
                            // feedback is dropped — the turn is unwinding.
                            None => {
                                tracing::info!(
                                    target: "kuncode::hook",
                                    hook = "post_tool_use",
                                    iteration,
                                    tool = %name,
                                    "hook cancelled",
                                );
                                self.pair_unpaired(session, iteration, &tool_calls[index + 1..])
                                    .await;
                                return Err(AgentError::Cancelled);
                            }
                            Some(PostToolOutcome::Proceed) => {}
                            Some(PostToolOutcome::AddFeedback(text)) => feedback.push(text),
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        target: "kuncode::tool",
                        iteration,
                        tool_call_id = %id,
                        tool = %name,
                        error_kind = agent_error_kind(&error),
                        pipeline_latency_ms = elapsed_ms(started),
                        "tool call pipeline aborted the turn",
                    );
                    // The turn is unwinding with this tool_call — and any that
                    // follow it — still unpaired. Pair `index` *honestly by why*:
                    // a harness tool error did run and fail (don't relabel it
                    // "cancelled"); a cancel/abort did not run.
                    let failed = match &error {
                        AgentError::Tool { source, .. } => {
                            ToolOutput::failure(ToolErrorKind::ToolError, source.to_string())
                        }
                        AgentError::ToolRegistration { .. } => ToolOutput::failure(
                            "tool_registration",
                            "Tool call not executed: its permission profile rejected the prepared operation.",
                        ),
                        AgentError::Authorization(_) => ToolOutput::failure(
                            "authorization_error",
                            "Tool call not executed: authorization failed closed.",
                        ),
                        _ => interrupted_tool_output(),
                    };
                    self.record_result(
                        session,
                        iteration,
                        id,
                        call_id,
                        &name,
                        CallOutcome {
                            output: failed,
                            executed: false,
                            retention: ToolResultRetention::Verbatim,
                        },
                    )
                    .await;
                    self.pair_unpaired(session, iteration, &tool_calls[index + 1..])
                        .await;
                    // Buffered PostToolUse feedback is dropped here, same as the
                    // cancel branch above: the turn is unwinding, the end-of-loop
                    // flush is never reached, and a failed turn should not leave a
                    // trailing feedback message behind. The harness-error path
                    // (`AgentError::Tool`) is rare; feedback is advisory and the
                    // real file state survives for the next turn to re-read.
                    return Err(error);
                }
            }
        }

        // Flush buffered feedback after the batch's tool_results (see above).
        for text in feedback {
            self.push_user_message(session, text).await;
        }

        Ok(())
    }

    /// Pairs every still-unexecuted tool_call with a synthetic interrupted
    /// result, so an unwinding turn never leaves an assistant tool_call
    /// dangling — most providers reject a request whose tool_call has no
    /// matching tool_result before the next user message.
    async fn pair_unpaired(
        &self,
        session: &mut AgentSession,
        iteration: usize,
        calls: &[PendingToolCall],
    ) {
        for call in calls {
            self.record_result(
                session,
                iteration,
                call.id.clone(),
                call.call_id.clone(),
                &call.name,
                CallOutcome {
                    output: interrupted_tool_output(),
                    executed: false,
                    retention: ToolResultRetention::Verbatim,
                },
            )
            .await;
        }
    }

    /// Records one tool result: emits the [`ToolEnd`](EventKind::ToolEnd) that
    /// mirrors `output` — same `ok`/`error`/`truncated` — then appends the
    /// transcript `tool_result` carrying the same `output`.
    ///
    /// The sole producer of either half, so the invariant "exactly one
    /// `ToolEnd` per transcript `tool_result`, both describing the same outcome"
    /// is structural: a new result path calls this and cannot emit the event
    /// without the transcript write, or let the two describe different outcomes.
    /// Event before transcript so a UI row closes no later than the result lands.
    async fn record_result(
        &self,
        session: &mut AgentSession,
        iteration: usize,
        id: String,
        call_id: Option<String>,
        tool: &str,
        outcome: CallOutcome,
    ) {
        let CallOutcome {
            output, retention, ..
        } = outcome;
        self.emit(
            session,
            Some(iteration),
            EventKind::ToolEnd {
                tool_call_id: id.clone(),
                tool: tool.to_string(),
                ok: output.ok,
                truncated: output.truncated,
                error: output.error.as_ref().map(ToolFailure::from),
            },
        );
        self.push_tool_result_message(
            session,
            tool_result_message(id, call_id, output.to_model_content()),
            retention,
        )
        .await;
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn agent_error_kind(error: &AgentError) -> &'static str {
    match error {
        AgentError::Completion(_) => "completion",
        AgentError::Tool { .. } => "tool",
        AgentError::Authorization(_) => "authorization",
        AgentError::ToolRegistration { .. } => "tool_registration",
        AgentError::EmptyTranscript => "empty_transcript",
        AgentError::RequestEncoding(_) => "request_encoding",
        AgentError::Compaction { .. } => "compaction",
        AgentError::Cancelled => "cancelled",
        AgentError::PromptBlocked { .. } => "prompt_blocked",
        AgentError::MaxIterations { .. } => "max_iterations",
    }
}

pub(super) fn pending_tool_calls(content: &NonEmptyVec<AssistantContent>) -> Vec<PendingToolCall> {
    content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::ToolCall(tool_call) => Some(PendingToolCall {
                id: tool_call.id.clone(),
                call_id: tool_call.call_id.clone(),
                name: tool_call.function.name.clone(),
                arguments: tool_call.function.arguments.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn tool_result_message(id: String, call_id: Option<String>, content: String) -> Message {
    Message::User {
        content: NonEmptyVec::new(UserContent::ToolResult(ToolResult {
            id,
            call_id,
            content: NonEmptyVec::new(ToolResultContent::text(content)),
        })),
    }
}

fn interrupted_tool_output() -> ToolOutput {
    ToolOutput::failure(
        ToolErrorKind::Cancelled,
        "Tool call not executed: the turn was interrupted before this tool returned.",
    )
}
