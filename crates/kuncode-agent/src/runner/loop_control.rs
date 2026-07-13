//! Iteration-budget control, plan reminders, and stop-hook continuation.

use kuncode_core::completion::{CompletionModel, Usage};
use tokio_util::sync::CancellationToken;

use crate::{
    error::AgentError,
    hook::{StopCx, StopOutcome},
    session::AgentSession,
};

use super::{
    AgentRunner, AgentTurn, STOP_CONTINUATION_LIMIT, TODO_REMINDER, cancellation::cancellable,
};

impl<M> AgentRunner<M>
where
    M: CompletionModel,
{
    /// The model/tool loop. Returns the failing iteration alongside the error so
    /// [`continue_session_with`](Self::continue_session_with) can emit a single
    /// turn-terminal [`Error`](EventKind::Error) with the right `iteration`.
    pub(super) async fn run_loop(
        &self,
        session: &mut AgentSession,
        cancel: &CancellationToken,
    ) -> Result<AgentTurn, (Option<usize>, AgentError)> {
        if session.is_empty() {
            return Err((None, AgentError::EmptyTranscript));
        }

        let mut usage = Usage::default();
        // Local to this run, so the cap resets every turn; on the session it
        // would accumulate and permanently disable later turns' `Stop` hooks.
        let mut stop_continuations = 0usize;

        // Plan-nag bookkeeping. `idle_iterations` counts model calls since the
        // plan's generation last advanced; when it reaches the configured
        // interval we re-surface the plan. Tracked by generation (not tool name)
        // so the runner stays agnostic to which tool maintains the plan.
        let reminder_interval = self.config.todo_reminder_interval;
        let mut last_todo_generation = session.todo_generation();
        let mut idle_iterations = 0usize;

        for iteration in 0..self.config.max_iterations {
            // A `todo_write` in the previous iteration's tool batch shows up as an
            // advanced generation: reset the idle counter. Otherwise nudge once
            // the plan has sat untouched for `interval` calls.
            let generation_now = session.todo_generation();
            if generation_now != last_todo_generation {
                last_todo_generation = generation_now;
                idle_iterations = 0;
            }
            if reminder_interval.is_some_and(|interval| idle_iterations >= interval) {
                self.push_user_message(session, TODO_REMINDER.to_string())
                    .await;
                idle_iterations = 0;
            }
            idle_iterations += 1;

            let iteration_result = self
                .run_iteration(session, iteration, cancel)
                .await
                .map_err(|error| (Some(iteration), error))?;
            usage += iteration_result.usage;

            if iteration_result.tool_calls.is_empty() {
                // A `Stop` hook may force another iteration — but only while
                // there is budget for another model call and the continuation
                // cap is not yet hit. Out of either, the final answer the model
                // just gave is returned as-is, never turned into a
                // `MaxIterations` error.
                let may_continue = !self.hooks.is_empty()
                    && stop_continuations < STOP_CONTINUATION_LIMIT
                    && iteration + 1 < self.config.max_iterations;
                if may_continue {
                    let outcome = {
                        let cx = StopCx {
                            messages: session.messages(),
                            iteration,
                        };
                        cancellable(cancel, self.hooks.stop(&cx)).await
                    };
                    match outcome {
                        None => return Err((Some(iteration), AgentError::Cancelled)),
                        Some(StopOutcome::Allow) => {}
                        Some(StopOutcome::Continue { message }) => {
                            stop_continuations += 1;
                            self.push_user_message(session, message).await;
                            continue;
                        }
                    }
                }
                return Ok(AgentTurn {
                    final_message_index: iteration_result.assistant_message_index,
                    usage,
                    iterations: iteration + 1,
                });
            }

            self.execute_tool_calls(session, iteration_result.tool_calls, iteration, cancel)
                .await
                .map_err(|error| (Some(iteration), error))?;
        }

        Err((
            // The last model call we made, or `None` when the budget was 0.
            self.config.max_iterations.checked_sub(1),
            AgentError::MaxIterations {
                max_iterations: self.config.max_iterations,
                messages: session.messages().to_vec(),
                usage,
            },
        ))
    }
}
