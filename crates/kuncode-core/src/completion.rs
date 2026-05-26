//! Provider-agnostic chat completion abstractions.
//!
//! [`message`] models the conversation turns exchanged with an LLM, while
//! [`request`] describes how a completion is invoked and what comes back.

use thiserror::Error;

pub mod message;
pub mod request;

/// Errors that can occur while building, dispatching, or decoding a
/// completion call.
#[derive(Debug, Error)]
pub enum CompletionError {
    /// A payload could not be (de)serialized as JSON.
    #[error("JsonError: {0}")]
    JsonError(#[from] serde_json::Error),
}
