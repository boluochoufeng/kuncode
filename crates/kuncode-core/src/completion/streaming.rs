//! Streaming completion: incremental events emitted while the model generates.
//!
//! A one-shot [`completion`](super::request::CompletionModel::completion)
//! returns the whole answer at once. A streaming call instead yields events as
//! tokens arrive, which a coding agent needs in order to:
//!
//! 1. render the visible answer live ([`StreamEvent::TextDelta`]);
//! 2. render the model's reasoning live in a separate channel
//!    ([`StreamEvent::ReasoningDelta`]);
//! 3. announce a tool call the moment it starts forming
//!    ([`StreamEvent::ToolCallStart`]);
//! 4. act on the fully-assembled tool calls and read token usage / stop reason
//!    once the turn ends ([`StreamEvent::Completed`]).

use std::pin::Pin;

use futures_core::Stream;

use crate::{
    completion::{AssistantContent, CompletionError, Usage},
    non_empty_vec::NonEmptyVec,
};

/// Why the model stopped generating.
///
/// The agent loop branches on this: `ToolCalls` executes the calls and
/// continues the turn; `Stop` ends the turn; `Length` means the output was
/// truncated at the token limit (the caller may want to continue or warn).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinishReason {
    /// Natural end of turn, or a stop sequence was hit.
    Stop,
    /// The token limit was reached; output is truncated.
    Length,
    /// Generation paused to hand tool calls back for execution.
    ToolCalls,
    /// The provider filtered the content.
    ContentFilter,
    /// A provider-specific reason not covered above.
    Other(String),
}

/// One incremental event from a streaming completion.
///
/// The deltas (`TextDelta` / `ReasoningDelta`) are for live rendering only and
/// carry just the newly-produced text. [`StreamEvent::Completed`] is always the
/// final event of a successful stream and carries the **fully-assembled**
/// message, identical in shape to what the non-streaming path returns, so the
/// caller never has to stitch together partial tool-call argument fragments
/// itself.
#[derive(Clone, Debug)]
pub enum StreamEvent {
    /// A chunk of the visible answer text.
    TextDelta(String),
    /// A chunk of reasoning/thinking text, kept separate from the answer
    /// (e.g. DeepSeek's `reasoning_content`). Render in a distinct channel.
    ReasoningDelta(String),
    /// A chunk of a safety refusal, kept distinct from ordinary answer text.
    RefusalDelta(String),
    /// A tool call has started: its `id` and `name` are known before the
    /// arguments finish streaming. Useful for an immediate "calling X" hint;
    /// the complete call (with assembled arguments) arrives in
    /// [`StreamEvent::Completed`]. `index` disambiguates parallel tool calls.
    ToolCallStart {
        /// Position among parallel tool calls in this assistant turn.
        index: usize,
        /// Provider tool-call identifier.
        id: String,
        /// Function name selected by the model.
        name: String,
    },
    /// Terminal event: the stream finished successfully. Carries the assembled
    /// assistant content (text + complete tool calls + reasoning), token usage,
    /// and the stop reason.
    Completed {
        /// Fully assembled assistant content for the turn.
        content: NonEmptyVec<AssistantContent>,
        /// Token accounting accumulated across the stream.
        usage: Usage,
        /// Provider stop reason normalized for agent-loop control flow.
        finish_reason: FinishReason,
    },
}

/// A stream of [`StreamEvent`]s. Boxed so each provider can return whatever
/// concrete stream its HTTP/SSE layer produces without leaking that type into
/// the public API.
///
/// Cancellation is simply dropping the stream: that closes the underlying HTTP
/// response and halts generation, so no explicit cancel call is needed.
pub type CompletionStream =
    Pin<Box<dyn Stream<Item = Result<StreamEvent, CompletionError>> + Send>>;
