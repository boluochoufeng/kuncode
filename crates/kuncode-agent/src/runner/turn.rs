//! Public turn entry points and turn-level hook handling.

use kuncode_core::completion::CompletionModel;
use tokio_util::sync::CancellationToken;

use crate::{
    error::AgentError,
    hook::{PromptCx, PromptOutcome},
    observer::EventKind,
    session::AgentSession,
};

use super::{AgentRunner, AgentTurn, cancellation::cancellable};

impl<M> AgentRunner<M>
where
    M: CompletionModel,
{
    /// Appends a user prompt, then advances the transcript until a final answer.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when a hook blocks or cancels the prompt, the
    /// provider or a tool fails, or the iteration budget is exhausted.
    pub async fn run_turn(
        &self,
        session: &mut AgentSession,
        prompt: impl Into<String>,
    ) -> Result<AgentTurn, AgentError> {
        self.run_turn_with(session, prompt, CancellationToken::new())
            .await
    }

    /// Like [`run_turn`](Self::run_turn) but with a caller-owned cancellation
    /// token (wire it to Ctrl-C for interruptible turns).
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] under the same conditions as
    /// [`run_turn`](Self::run_turn), or when `cancel` is triggered.
    pub async fn run_turn_with(
        &self,
        session: &mut AgentSession,
        prompt: impl Into<String>,
        cancel: CancellationToken,
    ) -> Result<AgentTurn, AgentError> {
        let prompt = prompt.into();

        // `UserPromptSubmit` is a *pre-commit* hook: it runs before the prompt
        // enters the transcript, so a `Block` rejects the input without leaving
        // it behind to leak to the provider on a later turn. The cx borrows the
        // transcript, so it is scoped to end before any `push_user`.
        if self.hooks.is_empty() {
            self.push_user_message(session, prompt).await;
        } else {
            let outcome = {
                let cx = PromptCx {
                    prompt: &prompt,
                    messages: session.messages(),
                };
                cancellable(&cancel, self.hooks.user_prompt_submit(&cx)).await
            };
            match outcome {
                None => return Err(self.terminal_error(session, None, AgentError::Cancelled)),
                Some(PromptOutcome::Proceed) => self.push_user_message(session, prompt).await,
                Some(PromptOutcome::AddContext(context)) => {
                    self.push_user_message(session, prompt).await;
                    self.push_user_message(session, context).await;
                }
                Some(PromptOutcome::Block { reason }) => {
                    return Err(self.terminal_error(
                        session,
                        None,
                        AgentError::PromptBlocked { reason },
                    ));
                }
            }
        }

        self.continue_session_with(session, cancel).await
    }

    /// Advances an existing transcript in place until the model stops calling tools.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] for an empty transcript, a provider or tool
    /// failure, cancellation, or exhaustion of the iteration budget.
    pub async fn continue_session(
        &self,
        session: &mut AgentSession,
    ) -> Result<AgentTurn, AgentError> {
        self.continue_session_with(session, CancellationToken::new())
            .await
    }

    /// Like [`continue_session`](Self::continue_session) but with a caller-owned
    /// cancellation token.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] under the same conditions as
    /// [`continue_session`](Self::continue_session), or when `cancel` is
    /// triggered.
    pub async fn continue_session_with(
        &self,
        session: &mut AgentSession,
        cancel: CancellationToken,
    ) -> Result<AgentTurn, AgentError> {
        let result = self.run_loop(session, &cancel).await;
        // Drained here — after the loop, not per iteration — so the turn's
        // *final* pushes (the closing assistant message, the last tool batch,
        // pushes on an unwinding error path) are covered too. A one-shot run
        // exits right after this turn; a loop-head check would let a failure
        // in those last writes escape unreported forever.
        self.warn_persistence(session);
        match result {
            Ok(turn) => Ok(turn),
            Err((iteration, error)) => Err(self.terminal_error(session, iteration, error)),
        }
    }

    /// Reports a session-persistence failure as a one-shot
    /// [`Warning`](EventKind::Warning). `iteration` is `None`: the failure
    /// belongs to a past push, not to any model call.
    ///
    /// With no observer attached the error is deliberately **left in the
    /// session** — `take_persistence_error` is take-and-clear, so draining it
    /// into a no-op emit would destroy the only report; leaving it lets a
    /// later observer-bearing runner still surface it.
    fn warn_persistence(&self, session: &mut AgentSession) {
        if self.observer.is_none() {
            return;
        }
        if let Some(message) = session.take_persistence_error() {
            self.emit(session, None, EventKind::Warning { message });
        }
    }
}
