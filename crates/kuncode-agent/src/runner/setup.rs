//! Runner construction, event emission, and durable transcript writes.

use std::{panic::AssertUnwindSafe, sync::Arc};

use kuncode_core::completion::{CompletionModel, Message};

use crate::{
    error::AgentError,
    hook::{Hook, Hooks},
    observer::{AgentEvent, AgentObserver, EventKind},
    permission::{Approver, AutoApprove, PermissionPolicy},
    registry::ToolRegistry,
    session::AgentSession,
    session_store::{NewJournalEntry, SessionStore},
    system_prompt::SystemPrompt,
};

use super::{AgentConfig, AgentRunner, events::error_kind};

impl<M> AgentRunner<M>
where
    M: CompletionModel,
{
    /// Creates a runner with default loop configuration.
    ///
    /// Defaults to the built-in deny rules and an [`AutoApprove`] approver, so
    /// dangerous commands are still blocked but nothing prompts. Callers that
    /// want a human in the loop set one via [`with_approver`](Self::with_approver).
    pub fn new(model: M, registry: ToolRegistry) -> Self {
        Self::with_config(model, registry, AgentConfig::default())
    }

    /// Creates a runner with explicit loop configuration.
    pub fn with_config(model: M, registry: ToolRegistry, config: AgentConfig) -> Self {
        Self {
            model,
            registry,
            config,
            system_prompt: Arc::new(SystemPrompt::default()),
            policy: Arc::new(PermissionPolicy::builtin()),
            approver: Arc::new(AutoApprove),
            observer: None,
            hooks: Arc::new(Hooks::new()),
            session_store: None,
        }
    }

    /// Replaces the system-prompt assembler (e.g. the CLI's full section set).
    pub fn with_system_prompt(mut self, system_prompt: SystemPrompt) -> Self {
        self.system_prompt = Arc::new(system_prompt);
        self
    }

    /// Replaces the static permission policy.
    pub fn with_policy(mut self, policy: PermissionPolicy) -> Self {
        self.policy = Arc::new(policy);
        self
    }

    /// Replaces the approval layer (e.g. a terminal prompt in the CLI).
    pub fn with_approver(mut self, approver: Arc<dyn Approver>) -> Self {
        self.approver = approver;
        self
    }

    /// Attaches a progress observer (e.g. the CLI's terminal renderer).
    pub fn with_observer(mut self, observer: Arc<dyn AgentObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Appends one loop hook, keeping registration order.
    pub fn with_hook(mut self, hook: Arc<dyn Hook>) -> Self {
        Arc::make_mut(&mut self.hooks).push(hook);
        self
    }

    /// Replaces the whole hook set (e.g. the ones parsed from settings).
    pub fn with_hooks(mut self, hooks: Hooks) -> Self {
        self.hooks = Arc::new(hooks);
        self
    }

    /// Attaches the durable store used for subsequent transcript messages.
    ///
    /// Messages already present in the session are not backfilled.
    pub fn with_session_store(mut self, store: Arc<dyn SessionStore>) -> Self {
        self.session_store = Some(store);
        self
    }

    /// Emits one event, but only when an observer is attached — with none the
    /// `seq` is left untouched and nothing is dispatched. The `seq` is drawn at
    /// emit time, the single source of total ordering.
    ///
    /// A panicking observer is isolated here: the rendering frontend must never
    /// be able to unwind the agent loop. This one chokepoint covers every sink —
    /// a bare observer as well as a
    /// [`CompositeObserver`](crate::observer::CompositeObserver), whose own
    /// per-observer guard additionally keeps siblings rendering when one panics.
    pub(super) fn emit(
        &self,
        session: &mut AgentSession,
        iteration: Option<usize>,
        kind: EventKind,
    ) {
        if let Some(observer) = &self.observer {
            let event = AgentEvent {
                seq: session.next_seq(),
                iteration,
                kind,
            };
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| observer.on_event(&event)));
        }
    }

    pub(super) async fn push_user_message(
        &self,
        session: &mut AgentSession,
        prompt: impl Into<String>,
    ) {
        self.push_message(session, Message::user(prompt)).await;
    }

    pub(super) async fn push_message(&self, session: &mut AgentSession, message: Message) {
        let session_id = session.session_id().cloned();
        let mut journal_seq = None;
        if let (Some(store), Some(session_id)) = (&self.session_store, session_id)
            && session.is_durable()
        {
            match NewJournalEntry::message(&message) {
                Ok(entry) => match store.append(&session_id, entry).await {
                    Ok(seq) => journal_seq = Some(seq),
                    Err(error) => session.mark_persistence_failed(error.to_string()),
                },
                Err(error) => session.mark_persistence_failed(error.to_string()),
            }
        }
        session.push_with_journal_seq(message, journal_seq);
    }

    /// Emits the single turn-terminal [`Error`](EventKind::Error) for a failing
    /// turn, then returns the error unchanged.
    ///
    /// Every unwind path routes through here: `run_loop` failures via
    /// [`continue_session_with`](Self::continue_session_with), and a
    /// `UserPromptSubmit` `Block`/cancel directly from
    /// [`run_turn_with`](Self::run_turn_with) — which returns before `run_loop`
    /// is ever entered, so it would otherwise miss the emit. One helper keeps
    /// "exactly one terminal `Error` per turn" true and closes any open
    /// `ModelStart`/`ToolStart` UI state once. `iteration` is `None` for
    /// failures with no owning model call (empty transcript, blocked prompt,
    /// `max_iterations == 0`).
    pub(super) fn terminal_error(
        &self,
        session: &mut AgentSession,
        iteration: Option<usize>,
        error: AgentError,
    ) -> AgentError {
        self.emit(
            session,
            iteration,
            EventKind::Error {
                kind: error_kind(&error).to_string(),
                message: error.to_string(),
            },
        );
        error
    }
}
