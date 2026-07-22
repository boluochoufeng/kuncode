//! One model iteration and streaming response consumption.

use std::time::Instant;

use futures_util::StreamExt;
use kuncode_core::{
    completion::{
        AssistantContent, CompletionError, CompletionModel, CompletionRequest, Message,
        StreamEvent, Usage,
    },
    non_empty_vec::NonEmptyVec,
};
use tokio_util::sync::CancellationToken;

use crate::{error::AgentError, observer::EventKind, session::AgentSession};

use super::{
    AgentRunner, IterationResult, cancellation::cancellable, request::assistant_text,
    tool_execution::pending_tool_calls,
};

impl<M> AgentRunner<M>
where
    M: CompletionModel,
{
    pub(super) async fn run_iteration(
        &self,
        session: &mut AgentSession,
        iteration: usize,
        cancel: &CancellationToken,
    ) -> Result<IterationResult, AgentError> {
        let request = self.prepare_request(session, iteration, cancel).await?;
        // Open the "thinking" state only after a successful build (a build
        // failure never started a model call). On completion error/cancel the
        // turn-terminal `Error` closes it; on success the `Assistant` below does.
        self.emit(session, Some(iteration), EventKind::ModelStart);
        let started = Instant::now();
        // Race the whole stream (establish + consume) against cancellation.
        // Waiting on the model is the most common place a user hits Ctrl-C, so
        // the token must cover it — not just the later tool approval/execution.
        // Dropping the future drops the stream, which closes the in-flight HTTP
        // response and halts generation.
        let (choice, usage) = match cancellable(
            cancel,
            self.stream_completion(session, iteration, request, started),
        )
        .await
        {
            Some(Ok(result)) => result,
            Some(Err(error)) => {
                log_model_failure(iteration, &error, started);
                return Err(error);
            }
            None => {
                tracing::info!(
                    target: "kuncode::provider",
                    iteration,
                    latency_ms = elapsed_ms(started),
                    "model request cancelled",
                );
                return Err(AgentError::Cancelled);
            }
        };
        tracing::info!(
            target: "kuncode::provider",
            iteration,
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            total_tokens = usage.total_tokens,
            cached_input_tokens = usage.cached_input_tokens,
            reasoning_tokens = usage.reasoning_tokens,
            latency_ms = elapsed_ms(started),
            "model request completed",
        );

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
        request_started: Instant,
    ) -> Result<(NonEmptyVec<AssistantContent>, Usage), AgentError> {
        let mut stream = self.model.stream(request).await?;
        let mut first_event = true;
        while let Some(event) = stream.next().await {
            let event = event?;
            if first_event {
                tracing::info!(
                    target: "kuncode::provider",
                    iteration,
                    event_kind = stream_event_kind(&event),
                    time_to_first_event_ms = elapsed_ms(request_started),
                    "model stream produced its first event",
                );
                first_event = false;
            }
            match event {
                StreamEvent::TextDelta(text) => {
                    self.emit(session, Some(iteration), EventKind::TextDelta { text });
                }
                StreamEvent::ReasoningDelta(text) => {
                    self.emit(session, Some(iteration), EventKind::ReasoningDelta { text });
                }
                StreamEvent::RefusalDelta(text) => {
                    self.emit(session, Some(iteration), EventKind::TextDelta { text });
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
}

fn log_model_failure(iteration: usize, error: &AgentError, started: Instant) {
    let (error_kind, status, is_timeout, is_connect) = match error {
        AgentError::Completion(CompletionError::HttpError(error)) => {
            ("http", None, error.is_timeout(), error.is_connect())
        }
        AgentError::Completion(CompletionError::ApiError { status, .. }) => {
            ("api", Some(*status), false, false)
        }
        AgentError::Completion(CompletionError::JsonError(_)) => ("json", None, false, false),
        AgentError::Completion(CompletionError::ResponseError(_)) => {
            ("response", None, false, false)
        }
        AgentError::Completion(CompletionError::RequestError(_)) => ("request", None, false, false),
        _ => ("agent", None, false, false),
    };
    tracing::warn!(
        target: "kuncode::provider",
        iteration,
        error_kind,
        status = ?status,
        is_timeout,
        is_connect,
        latency_ms = elapsed_ms(started),
        "model request failed",
    );
}

fn stream_event_kind(event: &StreamEvent) -> &'static str {
    match event {
        StreamEvent::TextDelta(_) => "text_delta",
        StreamEvent::ReasoningDelta(_) => "reasoning_delta",
        StreamEvent::RefusalDelta(_) => "refusal_delta",
        StreamEvent::ToolCallStart { .. } => "tool_call_start",
        StreamEvent::Completed { .. } => "completed",
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
