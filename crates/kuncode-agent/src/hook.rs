//! Extensible intervention points around the agent loop.
//!
//! A [`Hook`] is the control-plane counterpart of the read-only
//! [`AgentObserver`](crate::observer::AgentObserver). Authorization Hooks can
//! contribute policy or replace input only within capabilities fixed by their
//! trusted registration; policy still resolves Deny over Ask over Allow.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use async_trait::async_trait;
use kuncode_core::completion::Message;
use serde_json::Value;

use crate::permission::{
    ApprovalChallenge, ApprovalResolution, AuthorizationRequest, PolicyScopeSet, SafeExplanation,
};
use crate::tool::ToolOutput;

static NEXT_HOOK_REGISTRY_REVISION: AtomicU64 = AtomicU64::new(1);
const MAX_HOOK_NAME_CHARS: usize = 256;

/// Monotonic identity of the append-only Hook registry snapshot.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HookRegistryRevision(u64);

impl HookRegistryRevision {
    /// Returns the snapshot revision value.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Trusted capabilities assigned when a Hook is registered.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HookCapabilities {
    /// Allows an `Allow` contribution for the current generation.
    pub may_allow: bool,
    /// Allows replacing the complete tool input.
    pub may_rewrite_input: bool,
    /// Allows resolving an existing approval challenge.
    pub may_answer_approval: bool,
    /// Limits persistence scopes selectable by an approval Hook.
    pub policy_mutation_scopes: PolicyScopeSet,
}

/// Call-level policy effect returned by `PreToolUse`.
#[derive(Clone, Debug)]
pub enum HookEffect {
    /// Contributes ordinary Allow to every check in this generation.
    Allow,
    /// Contributes RequireApproval to every check in this generation.
    Ask,
    /// Contributes unapprovable Deny to every check in this generation.
    Deny {
        /// Bounded reason safe for ordinary diagnostics.
        reason: SafeExplanation,
    },
}

/// One Hook's effect and optional full-input replacement.
#[derive(Clone, Debug, Default)]
pub struct PreToolOutcome {
    /// `None` abstains from policy contribution.
    pub effect: Option<HookEffect>,
    /// Complete replacement input, never a sequential JSON patch.
    pub replacement_input: Option<Value>,
}

/// Bounded failure returned by an authorization Hook.
#[derive(Clone, Debug)]
pub struct AuthorizationHookFailure {
    reason: SafeExplanation,
}

impl AuthorizationHookFailure {
    /// Creates a fail-closed Hook diagnostic.
    pub fn new(reason: impl AsRef<str>) -> Self {
        Self {
            reason: SafeExplanation::new(reason),
        }
    }

    /// Returns the safe diagnostic text.
    pub fn reason(&self) -> &SafeExplanation {
        &self.reason
    }
}

/// Stable structured snapshot seen by every Hook in one generation.
pub struct PreToolCx<'a> {
    /// Profile-validated request for the current generation.
    pub request: &'a AuthorizationRequest,
    /// Transcript before this model tool call.
    pub messages: &'a [Message],
    /// Zero-based model-call index within the turn.
    pub iteration: usize,
}

impl PreToolCx<'_> {
    /// Owned serializable snapshot for an out-of-process Hook.
    pub fn payload(&self) -> Value {
        serde_json::json!({
            "event": "PreToolUse",
            "call_id": self.request.call_id(),
            "generation": self.request.generation(),
            "tool": self.request.tool(),
            "canonical_input": self.request.canonical_input(),
            "checks": self.request.checks(),
            "display": self.request.display(),
            "iteration": self.iteration,
        })
    }
}

/// The four loop seams a hook can act on.
///
/// Every method defaults to a no-op, so an implementor overrides only the points
/// it cares about. Methods are `async` because a hook may shell out; they run on
/// the loop task and the runner races each call against cancellation.
#[async_trait]
pub trait Hook: Send + Sync {
    /// A freshly submitted user prompt, **before it enters the transcript**
    /// (pre-commit) — so a `Block` leaves nothing behind to leak next turn.
    async fn user_prompt_submit(&self, cx: &PromptCx<'_>) -> PromptOutcome {
        let _ = cx;
        PromptOutcome::Proceed
    }

    /// Contributes policy or replaces input for a prepared authorization request.
    async fn pre_tool_use(
        &self,
        cx: &PreToolCx<'_>,
    ) -> Result<PreToolOutcome, AuthorizationHookFailure> {
        let _ = cx;
        Ok(PreToolOutcome::default())
    }

    /// Optionally resolves an already-created approval challenge.
    async fn approval_request(
        &self,
        challenge: &ApprovalChallenge,
    ) -> Result<ApprovalResolution, AuthorizationHookFailure> {
        let _ = challenge;
        Ok(ApprovalResolution::Abstain)
    }

    /// A tool that produced a deliverable [`ToolOutput`], after its result is
    /// written to the transcript. Does not fire for denied, cancelled, or
    /// harness-failed calls.
    async fn post_tool_use(&self, cx: &PostToolCx<'_>) -> PostToolOutcome {
        let _ = cx;
        PostToolOutcome::Proceed
    }

    /// The model returned a final answer (no tool calls) and the turn is about
    /// to end. A `Continue` forces another iteration.
    async fn stop(&self, cx: &StopCx<'_>) -> StopOutcome {
        let _ = cx;
        StopOutcome::Allow
    }
}

/// What a `UserPromptSubmit` hook decided.
#[derive(Clone, Debug)]
pub enum PromptOutcome {
    /// No objection; let the prompt through.
    Proceed,
    /// Append extra context (git status, conventions, …) the model will see.
    AddContext(String),
    /// Reject the prompt; it never enters the transcript or reaches the model.
    Block {
        /// Reason surfaced to the user.
        reason: String,
    },
}

/// Capability-validated output from one registered authorization Hook.
#[derive(Clone, Debug)]
pub(crate) struct AuthorizationHookResult {
    name: String,
    outcome: PreToolOutcome,
}

impl AuthorizationHookResult {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn outcome(&self) -> &PreToolOutcome {
        &self.outcome
    }
}

/// Invalid trusted Hook registration metadata.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum HookRegistrationError {
    /// Audit and cause identities require a concrete name.
    #[error("hook registration name must not be blank")]
    BlankName,
    /// Stable names must identify at most one Hook.
    #[error("hook registration name `{0}` is already in use")]
    DuplicateName(String),
    /// Stable names remain bounded for logs and cause identities.
    #[error("hook registration name exceeds the maximum of 256 characters")]
    NameTooLong,
}

/// What a `PostToolUse` hook decided. No veto — the tool already ran.
#[derive(Clone, Debug)]
pub enum PostToolOutcome {
    /// Nothing to add.
    Proceed,
    /// Append a feedback message after the tool's result.
    AddFeedback(String),
}

/// What a `Stop` hook decided.
#[derive(Clone, Debug)]
pub enum StopOutcome {
    /// Let the turn end.
    Allow,
    /// Force another iteration by injecting a user message.
    Continue {
        /// Injected user message driving the next model call.
        message: String,
    },
}

/// Borrowed view for [`Hook::user_prompt_submit`].
pub struct PromptCx<'a> {
    /// The prompt about to be committed; not yet in the transcript.
    pub prompt: &'a str,
    /// Transcript so far (excludes `prompt`).
    pub messages: &'a [Message],
}

impl PromptCx<'_> {
    /// Owned, serializable snapshot for an out-of-process hook.
    pub fn payload(&self) -> Value {
        serde_json::json!({ "event": "UserPromptSubmit", "prompt": self.prompt })
    }
}

/// Borrowed view for [`Hook::post_tool_use`].
pub struct PostToolCx<'a> {
    /// Tool name that produced `output`.
    pub tool: &'a str,
    /// The tool's delivered output (already written to the transcript).
    pub output: &'a ToolOutput,
    /// Transcript so far, including this tool's result.
    pub messages: &'a [Message],
    /// Zero-based model-call index within the turn.
    pub iteration: usize,
}

impl PostToolCx<'_> {
    /// Owned, serializable snapshot for an out-of-process hook.
    pub fn payload(&self) -> Value {
        serde_json::json!({
            "event": "PostToolUse",
            "tool": self.tool,
            "output": serde_json::to_value(self.output).unwrap_or_default(),
            "iteration": self.iteration,
        })
    }
}

/// Borrowed view for [`Hook::stop`].
pub struct StopCx<'a> {
    /// Full transcript, including the model's final answer.
    pub messages: &'a [Message],
    /// Zero-based index of the model call that produced the final answer.
    pub iteration: usize,
}

impl StopCx<'_> {
    /// Owned, serializable snapshot for an out-of-process hook.
    pub fn payload(&self) -> Value {
        serde_json::json!({ "event": "Stop", "iteration": self.iteration })
    }
}

/// Ordered collection of hooks with the folding rules documented on each method
/// (veto-first short-circuit, additive accumulation, any-`Continue` continues).
///
/// Empty by default; the runner checks [`is_empty`](Self::is_empty) and skips
/// the whole machinery (and its cancellation race) when there are no hooks.
#[derive(Clone)]
struct HookRegistration {
    name: String,
    hook: Arc<dyn Hook>,
    capabilities: HookCapabilities,
}

/// Ordered Hook registry with capabilities fixed at registration time.
#[derive(Clone)]
pub struct Hooks {
    registrations: Vec<HookRegistration>,
    revision: HookRegistryRevision,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            registrations: Vec::new(),
            revision: next_hook_registry_revision(),
        }
    }
}

impl Hooks {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a hook; later hooks fold after earlier ones (registration order).
    pub fn push(&mut self, hook: Arc<dyn Hook>) {
        let mut suffix = self.registrations.len();
        let name = loop {
            let candidate = format!("hook-{suffix}");
            if !self
                .registrations
                .iter()
                .any(|registration| registration.name == candidate)
            {
                break candidate;
            }
            suffix = suffix.saturating_add(1);
        };
        let hook_name = name.clone();
        self.registrations.push(HookRegistration {
            name,
            hook,
            capabilities: HookCapabilities::default(),
        });
        self.revision = next_hook_registry_revision();
        tracing::info!(
            target: "kuncode::hook",
            hook_name,
            may_allow = false,
            may_rewrite_input = false,
            may_answer_approval = false,
            policy_mutation_scopes = ?PolicyScopeSet::NONE,
            hook_revision = self.revision.get(),
            "authorization hook registered",
        );
    }

    /// Appends a named Hook with trusted capabilities.
    ///
    /// # Errors
    /// Returns an error for a blank or duplicate registration name.
    pub fn push_with_capabilities(
        &mut self,
        name: impl Into<String>,
        hook: Arc<dyn Hook>,
        capabilities: HookCapabilities,
    ) -> Result<(), HookRegistrationError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(HookRegistrationError::BlankName);
        }
        if name
            .chars()
            .take(MAX_HOOK_NAME_CHARS.saturating_add(1))
            .count()
            > MAX_HOOK_NAME_CHARS
        {
            return Err(HookRegistrationError::NameTooLong);
        }
        if self
            .registrations
            .iter()
            .any(|registration| registration.name == name)
        {
            return Err(HookRegistrationError::DuplicateName(name));
        }
        let hook_name = name.clone();
        self.registrations.push(HookRegistration {
            name,
            hook,
            capabilities,
        });
        self.revision = next_hook_registry_revision();
        tracing::info!(
            target: "kuncode::hook",
            hook_name,
            may_allow = capabilities.may_allow,
            may_rewrite_input = capabilities.may_rewrite_input,
            may_answer_approval = capabilities.may_answer_approval,
            policy_mutation_scopes = ?capabilities.policy_mutation_scopes,
            hook_revision = self.revision.get(),
            "authorization hook registered",
        );
        Ok(())
    }

    /// Whether there are no hooks — the runner's fast-path guard.
    pub fn is_empty(&self) -> bool {
        self.registrations.is_empty()
    }

    /// Returns the append-only registration revision.
    pub fn revision(&self) -> HookRegistryRevision {
        self.revision
    }

    /// Veto-first: the first `Block` wins and discards any context accumulated
    /// so far (the turn is rejected anyway); otherwise every `AddContext` is
    /// concatenated in registration order.
    pub async fn user_prompt_submit(&self, cx: &PromptCx<'_>) -> PromptOutcome {
        let mut contexts = Vec::new();
        for (hook_index, registration) in self.registrations.iter().enumerate() {
            let started = Instant::now();
            match registration.hook.user_prompt_submit(cx).await {
                PromptOutcome::Proceed => {
                    log_hook("user_prompt_submit", hook_index, "proceed", 0, started)
                }
                PromptOutcome::AddContext(text) => {
                    log_hook(
                        "user_prompt_submit",
                        hook_index,
                        "add_context",
                        text.chars().count(),
                        started,
                    );
                    contexts.push(text);
                }
                PromptOutcome::Block { reason } => {
                    log_hook(
                        "user_prompt_submit",
                        hook_index,
                        "block",
                        reason.chars().count(),
                        started,
                    );
                    return PromptOutcome::Block { reason };
                }
            }
        }
        fold_additive(contexts).map_or(PromptOutcome::Proceed, PromptOutcome::AddContext)
    }

    /// Runs every Hook against one immutable generation and validates outputs
    /// against registration-time capabilities.
    pub(crate) async fn pre_tool_use(&self, cx: &PreToolCx<'_>) -> Vec<AuthorizationHookResult> {
        let mut results = Vec::with_capacity(self.registrations.len());
        for (hook_index, registration) in self.registrations.iter().enumerate() {
            let started = Instant::now();
            let outcome = match registration.hook.pre_tool_use(cx).await {
                Ok(outcome) => validate_authorization_outcome(outcome, registration.capabilities),
                Err(error) => fail_closed_hook_outcome(error.reason().clone()),
            };
            let outcome_name = match &outcome.effect {
                Some(HookEffect::Allow) => "allow",
                Some(HookEffect::Ask) => "ask",
                Some(HookEffect::Deny { .. }) => "deny",
                None if outcome.replacement_input.is_some() => "rewrite",
                None => "abstain",
            };
            log_hook_with_tool(
                "pre_tool_use",
                hook_index,
                outcome_name,
                cx.request.tool().as_str(),
                cx.iteration,
                usize::from(outcome.replacement_input.is_some()),
                started,
            );
            results.push(AuthorizationHookResult {
                name: registration.name.clone(),
                outcome,
            });
        }
        results
    }

    /// Runs capable approval Hooks as the first resolver stage.
    pub(crate) async fn approval_request(
        &self,
        challenge: &ApprovalChallenge,
    ) -> ApprovalResolution {
        for (hook_index, registration) in self.registrations.iter().enumerate() {
            let started = Instant::now();
            let resolution = match registration.hook.approval_request(challenge).await {
                Ok(resolution) => resolution,
                Err(_) => {
                    log_approval_hook(
                        hook_index,
                        &registration.name,
                        "failure_deny",
                        challenge,
                        started,
                    );
                    return ApprovalResolution::Deny { persistence: None };
                }
            };
            if matches!(resolution, ApprovalResolution::Abstain) {
                log_approval_hook(
                    hook_index,
                    &registration.name,
                    "abstain",
                    challenge,
                    started,
                );
                continue;
            }
            if !registration.capabilities.may_answer_approval {
                log_approval_hook(
                    hook_index,
                    &registration.name,
                    "unauthorized_answer_deny",
                    challenge,
                    started,
                );
                return ApprovalResolution::Deny { persistence: None };
            }
            if matches!(resolution, ApprovalResolution::ReplaceInput(_))
                && !registration.capabilities.may_rewrite_input
            {
                log_approval_hook(
                    hook_index,
                    &registration.name,
                    "unauthorized_rewrite_deny",
                    challenge,
                    started,
                );
                return ApprovalResolution::Deny { persistence: None };
            }
            if !approval_persistence_is_authorized(
                &resolution,
                challenge,
                registration.capabilities,
            ) {
                log_approval_hook(
                    hook_index,
                    &registration.name,
                    "unauthorized_persistence_deny",
                    challenge,
                    started,
                );
                return ApprovalResolution::Deny { persistence: None };
            }
            log_approval_hook(
                hook_index,
                &registration.name,
                approval_resolution_name(&resolution),
                challenge,
                started,
            );
            return resolution;
        }
        ApprovalResolution::Abstain
    }

    /// Additive: every hook runs; all feedback is concatenated in order.
    pub async fn post_tool_use(&self, cx: &PostToolCx<'_>) -> PostToolOutcome {
        let mut feedback = Vec::new();
        for (hook_index, registration) in self.registrations.iter().enumerate() {
            let started = Instant::now();
            match registration.hook.post_tool_use(cx).await {
                PostToolOutcome::Proceed => log_hook_with_tool(
                    "post_tool_use",
                    hook_index,
                    "proceed",
                    cx.tool,
                    cx.iteration,
                    0,
                    started,
                ),
                PostToolOutcome::AddFeedback(text) => {
                    log_hook_with_tool(
                        "post_tool_use",
                        hook_index,
                        "add_feedback",
                        cx.tool,
                        cx.iteration,
                        text.chars().count(),
                        started,
                    );
                    feedback.push(text);
                }
            }
        }
        fold_additive(feedback).map_or(PostToolOutcome::Proceed, PostToolOutcome::AddFeedback)
    }

    /// "Not yet" semantics: every hook runs, and **any** `Continue` continues
    /// (all continuation messages joined into one injection).
    pub async fn stop(&self, cx: &StopCx<'_>) -> StopOutcome {
        let mut messages = Vec::new();
        for (hook_index, registration) in self.registrations.iter().enumerate() {
            let started = Instant::now();
            match registration.hook.stop(cx).await {
                StopOutcome::Allow => {
                    log_hook_with_iteration("stop", hook_index, "allow", cx.iteration, 0, started)
                }
                StopOutcome::Continue { message } => {
                    log_hook_with_iteration(
                        "stop",
                        hook_index,
                        "continue",
                        cx.iteration,
                        message.chars().count(),
                        started,
                    );
                    messages.push(message);
                }
            }
        }
        fold_additive(messages).map_or(StopOutcome::Allow, |message| StopOutcome::Continue {
            message,
        })
    }
}

fn next_hook_registry_revision() -> HookRegistryRevision {
    HookRegistryRevision(NEXT_HOOK_REGISTRY_REVISION.fetch_add(1, Ordering::Relaxed))
}

fn validate_authorization_outcome(
    outcome: PreToolOutcome,
    capabilities: HookCapabilities,
) -> PreToolOutcome {
    if matches!(outcome.effect.as_ref(), Some(HookEffect::Allow)) && !capabilities.may_allow {
        return fail_closed_hook_outcome(SafeExplanation::new(
            "hook returned Allow without may_allow capability",
        ));
    }
    if outcome.replacement_input.is_some() && !capabilities.may_rewrite_input {
        return fail_closed_hook_outcome(SafeExplanation::new(
            "hook replaced input without may_rewrite_input capability",
        ));
    }
    outcome
}

fn fail_closed_hook_outcome(reason: SafeExplanation) -> PreToolOutcome {
    PreToolOutcome {
        effect: Some(HookEffect::Deny { reason }),
        replacement_input: None,
    }
}

fn approval_persistence_is_authorized(
    resolution: &ApprovalResolution,
    challenge: &ApprovalChallenge,
    capabilities: HookCapabilities,
) -> bool {
    let (id, expected_effect) = match resolution {
        ApprovalResolution::Approve {
            persistence: Some(id),
        } => (id, crate::permission::PolicyEffect::Allow),
        ApprovalResolution::Deny {
            persistence: Some(id),
        } => (id, crate::permission::PolicyEffect::Deny),
        _ => return true,
    };
    challenge.mutation(id).is_some_and(|template| {
        template.effect() == expected_effect
            && capabilities
                .policy_mutation_scopes
                .contains(template.scope())
    })
}

fn log_hook(
    hook: &str,
    hook_index: usize,
    outcome: &str,
    contribution_chars: usize,
    started: Instant,
) {
    tracing::info!(
        target: "kuncode::hook",
        hook,
        hook_index,
        outcome,
        contribution_chars,
        latency_ms = elapsed_ms(started),
        "hook completed",
    );
}

fn log_hook_with_tool(
    hook: &str,
    hook_index: usize,
    outcome: &str,
    tool: &str,
    iteration: usize,
    contribution_chars: usize,
    started: Instant,
) {
    tracing::info!(
        target: "kuncode::hook",
        hook,
        hook_index,
        outcome,
        tool,
        iteration,
        contribution_chars,
        latency_ms = elapsed_ms(started),
        "hook completed",
    );
}

fn log_hook_with_iteration(
    hook: &str,
    hook_index: usize,
    outcome: &str,
    iteration: usize,
    contribution_chars: usize,
    started: Instant,
) {
    tracing::info!(
        target: "kuncode::hook",
        hook,
        hook_index,
        outcome,
        iteration,
        contribution_chars,
        latency_ms = elapsed_ms(started),
        "hook completed",
    );
}

fn log_approval_hook(
    hook_index: usize,
    name: &str,
    outcome: &str,
    challenge: &ApprovalChallenge,
    started: Instant,
) {
    tracing::info!(
        target: "kuncode::hook",
        hook = "approval_request",
        hook_index,
        hook_name = name,
        outcome,
        challenge_id = challenge.id().as_str(),
        latency_ms = elapsed_ms(started),
        "hook completed",
    );
}

fn approval_resolution_name(resolution: &ApprovalResolution) -> &'static str {
    match resolution {
        ApprovalResolution::Abstain => "abstain",
        ApprovalResolution::Approve { .. } => "approve",
        ApprovalResolution::Deny { .. } => "deny",
        ApprovalResolution::ReplaceInput(_) => "replace_input",
        ApprovalResolution::Cancel => "cancel",
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Joins accumulated additive contributions, or `None` when there were none.
fn fold_additive(parts: Vec<String>) -> Option<String> {
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// A declarative test hook: each builder enables one behavior at one point.
///
/// `expect`-free, but gated to `#[cfg(test)]` to keep the scaffold out of the
/// shipped library.
#[cfg(test)]
#[derive(Default)]
pub struct ScriptedHook {
    deny_tool: Option<String>,
    add_context: Option<String>,
    block_reason: Option<String>,
    add_feedback: Option<String>,
    stop_continue: Option<(usize, String)>,
    stop_calls: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl ScriptedHook {
    /// `Deny` any `PreToolUse` whose tool name equals `tool`.
    pub fn deny_tool(mut self, tool: impl Into<String>) -> Self {
        self.deny_tool = Some(tool.into());
        self
    }

    /// `AddContext(text)` on `UserPromptSubmit`.
    pub fn add_context(mut self, text: impl Into<String>) -> Self {
        self.add_context = Some(text.into());
        self
    }

    /// `Block { reason }` on `UserPromptSubmit`.
    pub fn block(mut self, reason: impl Into<String>) -> Self {
        self.block_reason = Some(reason.into());
        self
    }

    /// `AddFeedback(text)` on `PostToolUse`.
    pub fn add_feedback(mut self, text: impl Into<String>) -> Self {
        self.add_feedback = Some(text.into());
        self
    }

    /// `Continue { message }` the first `times` calls to `stop`, then `Allow`.
    pub fn stop_continue(mut self, times: usize, message: impl Into<String>) -> Self {
        self.stop_continue = Some((times, message.into()));
        self
    }
}

#[cfg(test)]
#[async_trait]
impl Hook for ScriptedHook {
    async fn user_prompt_submit(&self, _cx: &PromptCx<'_>) -> PromptOutcome {
        if let Some(reason) = &self.block_reason {
            return PromptOutcome::Block {
                reason: reason.clone(),
            };
        }
        if let Some(text) = &self.add_context {
            return PromptOutcome::AddContext(text.clone());
        }
        PromptOutcome::Proceed
    }

    async fn pre_tool_use(
        &self,
        cx: &PreToolCx<'_>,
    ) -> Result<PreToolOutcome, AuthorizationHookFailure> {
        if self.deny_tool.as_deref() == Some(cx.request.tool().as_str()) {
            return Ok(PreToolOutcome {
                effect: Some(HookEffect::Deny {
                    reason: SafeExplanation::new(format!(
                        "blocked {} in test",
                        cx.request.tool().as_str()
                    )),
                }),
                replacement_input: None,
            });
        }
        Ok(PreToolOutcome::default())
    }

    async fn post_tool_use(&self, _cx: &PostToolCx<'_>) -> PostToolOutcome {
        if let Some(text) = &self.add_feedback {
            return PostToolOutcome::AddFeedback(text.clone());
        }
        PostToolOutcome::Proceed
    }

    async fn stop(&self, _cx: &StopCx<'_>) -> StopOutcome {
        if let Some((times, message)) = &self.stop_continue {
            let n = self
                .stop_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < *times {
                return StopOutcome::Continue {
                    message: message.clone(),
                };
            }
        }
        StopOutcome::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt_cx<'a>(prompt: &'a str, messages: &'a [Message]) -> PromptCx<'a> {
        PromptCx { prompt, messages }
    }

    #[tokio::test]
    async fn additive_context_concatenates_in_order() {
        let hooks = {
            let mut h = Hooks::new();
            h.push(Arc::new(ScriptedHook::default().add_context("first")));
            h.push(Arc::new(ScriptedHook::default().add_context("second")));
            h
        };
        let messages = Vec::new();
        let outcome = hooks.user_prompt_submit(&prompt_cx("hi", &messages)).await;
        match outcome {
            PromptOutcome::AddContext(text) => assert_eq!(text, "first\n\nsecond"),
            other => panic!("expected AddContext, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn block_wins_and_discards_earlier_context() {
        let hooks = {
            let mut h = Hooks::new();
            h.push(Arc::new(ScriptedHook::default().add_context("dropped")));
            h.push(Arc::new(ScriptedHook::default().block("nope")));
            h
        };
        let messages = Vec::new();
        let outcome = hooks.user_prompt_submit(&prompt_cx("hi", &messages)).await;
        assert!(matches!(outcome, PromptOutcome::Block { reason } if reason == "nope"));
    }

    #[tokio::test]
    async fn empty_hooks_proceed() {
        let hooks = Hooks::new();
        let messages = Vec::new();
        assert!(hooks.is_empty());
        assert!(matches!(
            hooks.user_prompt_submit(&prompt_cx("hi", &messages)).await,
            PromptOutcome::Proceed
        ));
    }

    #[tokio::test]
    async fn stop_continues_until_the_count_is_spent() {
        let hooks = {
            let mut h = Hooks::new();
            h.push(Arc::new(ScriptedHook::default().stop_continue(2, "again")));
            h
        };
        let messages = Vec::new();
        let cx = StopCx {
            messages: &messages,
            iteration: 0,
        };
        assert!(matches!(
            hooks.stop(&cx).await,
            StopOutcome::Continue { .. }
        ));
        assert!(matches!(
            hooks.stop(&cx).await,
            StopOutcome::Continue { .. }
        ));
        assert!(matches!(hooks.stop(&cx).await, StopOutcome::Allow));
    }
}
