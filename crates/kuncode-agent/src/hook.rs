//! Extensible intervention points around the agent loop.
//!
//! A [`Hook`] is the control-plane dual of the read-only
//! [`AgentObserver`](crate::observer::AgentObserver): same "agent defines the
//! trait, the frontend implements it" shape as
//! [`Approver`](crate::permission::Approver), but on the control path — a hook
//! can veto, inject, or force a continuation. Hooks may only *tighten*: there is
//! deliberately no variant that lets one grant a tool the
//! [`PermissionGate`](crate::permission::PermissionGate) did not — `PreToolOutcome`
//! has no `Allow`, so "a hook cannot bypass the gate" holds at the type level.

use std::sync::Arc;

use async_trait::async_trait;
use kuncode_core::completion::Message;
use serde_json::Value;

use crate::permission::{PermissionAction, PermissionRequest};
use crate::tool::ToolOutput;

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

    /// A tool call once its [`PermissionRequest`] is computed, before the gate
    /// decides. Tighten-only: it can `Deny`, never grant.
    async fn pre_tool_use(&self, cx: &PreToolCx<'_>) -> PreToolOutcome {
        let _ = cx;
        PreToolOutcome::Proceed
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

/// What a `PreToolUse` hook decided.
///
/// No `Allow` variant by design: a hook can only tighten, never override the
/// gate (see the module docs).
#[derive(Clone, Debug)]
pub enum PreToolOutcome {
    /// No objection; the gate still decides.
    Proceed,
    /// Block the call; fed back to the model as a recoverable
    /// `blocked_by_hook` failure.
    Deny {
        /// Message reported to the model.
        message: String,
    },
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

/// Borrowed view for [`Hook::pre_tool_use`].
pub struct PreToolCx<'a> {
    /// The structured request the gate will rule on (reused from s03).
    pub request: &'a PermissionRequest,
    /// Raw arguments (read-only; argument rewrite is deferred).
    pub args: &'a Value,
    /// Transcript so far.
    pub messages: &'a [Message],
    /// Zero-based model-call index within the turn.
    pub iteration: usize,
}

impl PreToolCx<'_> {
    /// Owned, serializable snapshot for an out-of-process hook.
    pub fn payload(&self) -> Value {
        serde_json::json!({
            "event": "PreToolUse",
            "tool": self.request.tool,
            "action": action_str(self.request.action),
            "resource": self.request.resource,
            "summary": self.request.summary,
            "args": self.args,
            "iteration": self.iteration,
        })
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

/// Wire name for a [`PermissionAction`] in a hook payload.
fn action_str(action: PermissionAction) -> &'static str {
    match action {
        PermissionAction::Read => "read",
        PermissionAction::Write => "write",
        PermissionAction::Execute => "execute",
        PermissionAction::Meta => "meta",
    }
}

/// Ordered collection of hooks with the folding rules documented on each method
/// (veto-first short-circuit, additive accumulation, any-`Continue` continues).
///
/// Empty by default; the runner checks [`is_empty`](Self::is_empty) and skips
/// the whole machinery (and its cancellation race) when there are no hooks.
#[derive(Default, Clone)]
pub struct Hooks(Vec<Arc<dyn Hook>>);

impl Hooks {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a hook; later hooks fold after earlier ones (registration order).
    pub fn push(&mut self, hook: Arc<dyn Hook>) {
        self.0.push(hook);
    }

    /// Whether there are no hooks — the runner's fast-path guard.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Veto-first: the first `Block` wins and discards any context accumulated
    /// so far (the turn is rejected anyway); otherwise every `AddContext` is
    /// concatenated in registration order.
    pub async fn user_prompt_submit(&self, cx: &PromptCx<'_>) -> PromptOutcome {
        let mut contexts = Vec::new();
        for hook in &self.0 {
            match hook.user_prompt_submit(cx).await {
                PromptOutcome::Proceed => {}
                PromptOutcome::AddContext(text) => contexts.push(text),
                block @ PromptOutcome::Block { .. } => return block,
            }
        }
        fold_additive(contexts).map_or(PromptOutcome::Proceed, PromptOutcome::AddContext)
    }

    /// Veto-first: the first `Deny` short-circuits the rest.
    pub async fn pre_tool_use(&self, cx: &PreToolCx<'_>) -> PreToolOutcome {
        for hook in &self.0 {
            if let deny @ PreToolOutcome::Deny { .. } = hook.pre_tool_use(cx).await {
                return deny;
            }
        }
        PreToolOutcome::Proceed
    }

    /// Additive: every hook runs; all feedback is concatenated in order.
    pub async fn post_tool_use(&self, cx: &PostToolCx<'_>) -> PostToolOutcome {
        let mut feedback = Vec::new();
        for hook in &self.0 {
            if let PostToolOutcome::AddFeedback(text) = hook.post_tool_use(cx).await {
                feedback.push(text);
            }
        }
        fold_additive(feedback).map_or(PostToolOutcome::Proceed, PostToolOutcome::AddFeedback)
    }

    /// "Not yet" semantics: every hook runs, and **any** `Continue` continues
    /// (all continuation messages joined into one injection).
    pub async fn stop(&self, cx: &StopCx<'_>) -> StopOutcome {
        let mut messages = Vec::new();
        for hook in &self.0 {
            if let StopOutcome::Continue { message } = hook.stop(cx).await {
                messages.push(message);
            }
        }
        fold_additive(messages).map_or(StopOutcome::Allow, |message| StopOutcome::Continue {
            message,
        })
    }
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
/// `expect`-free, but gated to `#[cfg(test)]` anyway to keep the test scaffold
/// out of the shipped library (same stance as
/// [`ScriptedApprover`](crate::permission::ScriptedApprover)).
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

    async fn pre_tool_use(&self, cx: &PreToolCx<'_>) -> PreToolOutcome {
        if self.deny_tool.as_deref() == Some(cx.request.tool.as_str()) {
            return PreToolOutcome::Deny {
                message: format!("blocked {} in test", cx.request.tool),
            };
        }
        PreToolOutcome::Proceed
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
