//! Read-only observation of the agent loop.
//!
//! The runner emits structured [`AgentEvent`]s at key points so a frontend can
//! render live progress. This mirrors the [`Approver`] seam — the agent defines
//! the trait and emits, the frontend implements rendering — so `kuncode-agent`
//! never touches the terminal.
//!
//! [`Approver`]: crate::permission::Approver

use std::{panic::AssertUnwindSafe, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::todo::TodoItem;
use crate::tool::{ToolErrorKind, ToolErrorPayload};

/// One event produced in order by the agent loop.
///
/// Split into an ordering/attribution envelope ([`seq`](Self::seq) /
/// [`iteration`](Self::iteration)) plus the [`kind`](Self::kind) payload, so a
/// renderer can ignore ordering while audit/remote consumers still get it
/// without every variant repeating the fields.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentEvent {
    /// Monotonic per-session sequence number, unique and ordering. Taken from
    /// the session counter at emit time — the only reliable ordering key when a
    /// transport may reorder events, so **every event carries one**.
    pub seq: u64,
    /// Which model call within the current user turn this event belongs to (the
    /// runner's iteration index, 0-based per turn). Deliberately not named
    /// `turn`: it is not a session-level user round (session ordering is
    /// [`seq`](Self::seq)).
    ///
    /// `Option` because not every event belongs to a model call:
    /// [`ModelStart`](EventKind::ModelStart) / [`Assistant`](EventKind::Assistant)
    /// / [`ToolStart`](EventKind::ToolStart) / [`ToolEnd`](EventKind::ToolEnd)
    /// always carry `Some(i)`, but a turn-level [`Error`](EventKind::Error) can
    /// fire before any iteration (empty transcript, or `max_iterations == 0`),
    /// where `None` is honest and `0` would collide with real iteration `0`.
    pub iteration: Option<usize>,
    pub kind: EventKind,
}

/// Payload of an [`AgentEvent`].
///
/// All fields are owned (no borrows) so an event can be cloned, sent across
/// threads/processes, and serialized. Tagged so a remote frontend sees a clean
/// `{ "type": "tool_start", .. }` shape.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// The runner is waiting on the model. There is no matching `ModelEnd`: on
    /// success the next [`Assistant`](Self::Assistant) means it returned; on a
    /// model-stage failure/cancel the turn-level [`Error`](Self::Error) closes
    /// it instead, so a "thinking" indicator always gets cleared.
    ModelStart,
    /// A chunk of the visible answer produced while streaming, for live
    /// rendering. Presentation-only with no transcript counterpart: the
    /// authoritative text arrives in the turn-final [`Assistant`](Self::Assistant)
    /// and the transcript. A renderer accumulates these into the in-progress
    /// answer, then lets `Assistant` finalize it.
    TextDelta { text: String },
    /// A chunk of the model's reasoning/thinking produced while streaming, kept
    /// separate from [`TextDelta`](Self::TextDelta) so a renderer can show it in
    /// a distinct (e.g. dimmed) channel. Also presentation-only.
    ReasoningDelta { text: String },
    /// One assistant message. `tool_calls` empty ⟺ this turn is the final
    /// answer; non-empty ⟺ `text` is intermediate narration alongside calls.
    Assistant {
        text: String,
        /// Ids of the tool calls in this message, in order.
        tool_calls: Vec<String>,
    },
    /// A tool call is about to be gated/executed. Emitted only once a
    /// [`PermissionRequest`] was computed (tool resolved, arguments parsed), so
    /// unknown-tool / bad-arguments produce only a [`ToolEnd`](Self::ToolEnd)
    /// with no preceding `ToolStart`.
    ///
    /// [`PermissionRequest`]: crate::permission::PermissionRequest
    ToolStart {
        tool_call_id: String,
        tool: String,
        summary: String,
    },
    /// The single terminal event for a tool call, derived from the `ToolOutput`
    /// written to the transcript. Success, denial, unknown tool, bad arguments,
    /// harness error, interruption — all flavors, told apart by `error.kind`.
    /// Carries no full result body — that stays in the transcript; the event is
    /// a thin notification, not the payload.
    ToolEnd {
        tool_call_id: String,
        tool: String,
        ok: bool,
        truncated: bool,
        error: Option<ToolFailure>,
    },
    /// The turn unwound before producing a final answer. Emitted once, at the
    /// turn boundary, for every error path: completion failure, harness tool
    /// error, cancel/abort, max-iterations. Closes any open
    /// [`ModelStart`](Self::ModelStart) / [`ToolStart`](Self::ToolStart) UI
    /// state — especially when the model stage itself fails, where no
    /// `Assistant`/`ToolEnd` follows. `kind` mirrors the `AgentError` variant,
    /// e.g. `"completion"` / `"tool"` / `"cancelled"` / `"max_iterations"`.
    Error { kind: String, message: String },
    /// The session task plan changed (the model called `todo_write`). Emitted by
    /// the runner when a tool call advances the session plan's generation, so it
    /// stays generic instead of recognizing `todo_write` by name.
    ///
    /// A *presentation-only* event with no transcript counterpart: the plan's
    /// authoritative copies are the `todo_write` `tool_result` (the model's view)
    /// and the session store (the harness's view), so this does not participate
    /// in the [`ToolEnd`](Self::ToolEnd) ⇄ `tool_result` mirror invariant.
    /// Unlike `ToolEnd` it *carries its full payload* — the renderer needs the
    /// structured plan to draw a checklist and cannot reconstruct it from
    /// `ToolEnd`, and a plan is small and bounded.
    TodoUpdate { todos: Vec<TodoItem> },
    /// A non-fatal harness degradation the user should hear about — e.g.
    /// transcript persistence stopped working (disk full, no home directory).
    /// The turn itself continues unaffected.
    ///
    /// *Presentation-only*, like [`TodoUpdate`](Self::TodoUpdate): no
    /// transcript counterpart. Deliberately generic rather than one variant
    /// per source — every best-effort side channel that degrades shares this
    /// one rendering path, and the *emitter* is responsible for reporting a
    /// given failure only once (see
    /// [`transcript`](crate::transcript)'s take-and-clear contract).
    Warning { message: String },
}

/// Failure summary for [`EventKind::ToolEnd`], shaped like `ToolOutput.error`
/// ([`ToolErrorPayload`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolFailure {
    pub kind: ToolErrorKind,
    pub message: String,
}

impl From<&ToolErrorPayload> for ToolFailure {
    fn from(payload: &ToolErrorPayload) -> Self {
        Self {
            kind: payload.kind.clone(),
            message: payload.message.clone(),
        }
    }
}

/// Receives agent events.
///
/// Implementations must not block the runtime: a terminal renderer's writes are
/// light enough to run synchronously; forwarding to a TUI/remote should do a
/// non-blocking enqueue.
pub trait AgentObserver: Send + Sync {
    fn on_event(&self, event: &AgentEvent);
}

/// Fans one event stream out to several observers (e.g. a UI renderer and an
/// audit sink) that don't know about each other.
///
/// Fanout is synchronous and serial on the runner's task, so observers must not
/// block — a slow one stalls the rest and the loop. A panicking observer is
/// isolated via [`catch_unwind`](std::panic::catch_unwind) so it neither
/// unwinds the turn nor starves the observers after it; this is a defensive
/// backstop, not licence to use panics as control flow.
pub struct CompositeObserver(pub Vec<Arc<dyn AgentObserver>>);

impl AgentObserver for CompositeObserver {
    fn on_event(&self, event: &AgentEvent) {
        for observer in &self.0 {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| observer.on_event(event)));
        }
    }
}
