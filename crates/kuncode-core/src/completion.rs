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
    ProviderToolDefinition, ReasoningEffort, ToolDefinition, Usage,
};

pub use streaming::{CompletionStream, FinishReason, StreamEvent};

/// Errors that can occur while building, dispatching, or decoding a completion call.
#[derive(Debug, Error)]
pub enum CompletionError {
    /// A payload could not be (de)serialized as JSON.
    #[error("JsonError: {0}")]
    JsonError(#[from] serde_json::Error),

    /// Provider response was syntactically valid but could not be projected
    /// into the domain model.
    #[error("ResponseError: {0}")]
    ResponseError(String),

    /// Caller supplied a request that cannot be represented by the target
    /// provider.
    #[error("RequestError: {0}")]
    RequestError(String),

    /// Transport or response-body read failure from the HTTP client.
    #[error("HttpError: {0}")]
    HttpError(#[from] reqwest::Error),

    /// Non-2xx provider response; `message` keeps the raw response body for
    /// debugging provider-side validation failures.
    #[error("ApiError({status}): {message}")]
    ApiError {
        /// HTTP status code returned by the provider.
        status: u16,
        /// Raw provider response body.
        message: String,
    },
}
