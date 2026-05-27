//! Provider-agnostic chat completion abstractions.
//!
//! [`message`] models the conversation turns exchanged with an LLM, while
//! [`request`] describes how a completion is invoked and what comes back.

use thiserror::Error;

pub mod message;
pub mod request;
pub mod streaming;

pub use message::{
    AssistantContent, Message, Reasoning, ReasoningContent, Text, ToolCall, ToolChoice,
    ToolFunction, ToolResult, ToolResultContent, UserContent,
};

pub use request::{
    CompletionModel, CompletionRequest, CompletionRequestBuilder, CompletionResponse,
    ProviderToolDescriptor, ReasoningEffort, ToolDescriptor, Usage,
};

pub use streaming::{CompletionStream, FinishReason, StreamEvent};

/// Errors that can occur while building, dispatching, or decoding a
/// completion call.
#[derive(Debug, Error)]
pub enum CompletionError {
    /// A payload could not be (de)serialized as JSON.
    #[error("JsonError: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("ResponseError: {0}")]
    ResponseError(String),

    #[error("RequestError: {0}")]
    RequestError(String),

    #[error("HttpError: {0}")]
    HttpError(#[from] reqwest::Error),

    #[error("ApiError({status}): {message}")]
    ApiError { status: u16, message: String },
}
