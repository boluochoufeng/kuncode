//! Chat Completions Server-Sent Events (SSE) streaming: wire chunk DTOs, an incremental
//! SSE frame decoder, and an assembler that folds chunks into [`StreamEvent`]s.
//!
//! Split into pure pieces — [`SseDecoder`] (bytes → `data:` payloads) and
//! [`StreamAssembler`] (chunk → events + accumulated state) — so both are unit
//! tested without a network round trip. [`stream_events`] is the thin async glue
//! that drives a live [`reqwest::Response`] body through them.

use async_stream::try_stream;
use serde::{Deserialize, de::DeserializeOwned};

use crate::completion::{
    AssistantContent, CompletionError, CompletionStream, FinishReason, StreamEvent, Usage,
};
use crate::non_empty_vec::NonEmptyVec;

/// One `chat.completion.chunk` frame. The terminal usage-only frame carries an
/// empty `choices`, so it defaults rather than failing to deserialize.
#[derive(Debug, Deserialize)]
struct StreamChunk<U> {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    /// Present only on the final frame when `stream_options.include_usage` is set.
    usage: Option<U>,
    /// Set when the endpoint reports a failure *mid-stream* as a data frame
    /// instead of via HTTP status; see [`StreamErrorBody`].
    error: Option<StreamErrorBody>,
}

/// An OpenAI-compatible error object some endpoints emit as a `data: {"error":
/// {...}}` frame after generation has begun (e.g. rate-limit / overload), rather
/// than failing the HTTP status. Captured so it surfaces as a stream error: left
/// in `StreamChunk` only, it would parse as an empty chunk and be dropped,
/// letting [`StreamAssembler::finish`] report the partial answer as a clean stop.
#[derive(Debug, Deserialize)]
struct StreamErrorBody {
    message: String,
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChunkChoice {
    /// A role-only opening frame sends `delta: {}`, hence the default.
    #[serde(default)]
    delta: ChunkDelta,
    finish_reason: Option<String>,
}

/// All fields optional: any one of text, reasoning, or tool-call fragments may be
/// absent from a given frame.
#[derive(Debug, Default, Deserialize)]
struct ChunkDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
    refusal: Option<String>,
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    /// Position among parallel tool calls; the key under which fragments accrue.
    index: usize,
    /// Sent once, in the call's first fragment.
    id: Option<String>,
    function: Option<FnDelta>,
}

#[derive(Debug, Deserialize)]
struct FnDelta {
    /// Sent once, in the call's first fragment.
    name: Option<String>,
    /// Streamed across fragments; concatenated into the full stringified-JSON
    /// argument blob.
    arguments: Option<String>,
}

/// One decoded SSE line worth acting on; comments, blank lines, and non-`data:`
/// fields are dropped by the decoder and never surface here.
#[derive(Debug, PartialEq, Eq)]
enum SseEvent {
    /// A `data:` payload (the JSON of a [`StreamChunk`]).
    Data(String),
    /// The `data: [DONE]` sentinel terminating the stream.
    Done,
}

/// Incremental SSE frame decoder.
///
/// Buffers bytes across network chunks and emits one [`SseEvent`] per complete
/// `data:` line. A line break (`\n`, 0x0A) can never fall inside a multi-byte
/// UTF-8 sequence, so splitting on it never severs a character — each complete
/// line is valid UTF-8.
#[derive(Default)]
struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    fn new() -> Self {
        Self::default()
    }

    /// Appends `bytes` and returns every newly-complete `data:` line.
    fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            let Some(payload) = line.strip_prefix("data:") else {
                continue; // event:/id:/comment/blank — not our concern
            };
            let payload = payload.trim();
            if payload == "[DONE]" {
                out.push(SseEvent::Done);
            } else if !payload.is_empty() {
                out.push(SseEvent::Data(payload.to_string()));
            }
        }
        out
    }
}

/// A tool call accreting across streaming fragments.
struct PartialToolCall {
    index: usize,
    id: String,
    name: String,
    arguments: String,
    /// Whether [`StreamEvent::ToolCallStart`] has already fired for this call.
    announced: bool,
}

/// Folds successive [`StreamChunk`]s into render deltas, accumulating the full
/// assistant content for the terminal [`StreamEvent::Completed`].
#[derive(Default)]
struct StreamAssembler {
    text: String,
    reasoning: String,
    refusal: String,
    tool_calls: Vec<PartialToolCall>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

impl StreamAssembler {
    /// Folds one chunk in, returning the render deltas it produced (text /
    /// reasoning / tool-call-start). The terminal [`StreamEvent::Completed`] is
    /// produced separately by [`finish`](Self::finish).
    fn ingest<U>(&mut self, chunk: StreamChunk<U>) -> Vec<StreamEvent>
    where
        U: Into<Usage>,
    {
        if let Some(usage) = chunk.usage {
            self.usage = Some(usage.into());
        }
        let mut events = Vec::new();
        for choice in chunk.choices {
            if let Some(text) = choice.delta.content.filter(|s| !s.is_empty()) {
                self.text.push_str(&text);
                events.push(StreamEvent::TextDelta(text));
            }
            if let Some(reasoning) = choice.delta.reasoning_content.filter(|s| !s.is_empty()) {
                self.reasoning.push_str(&reasoning);
                events.push(StreamEvent::ReasoningDelta(reasoning));
            }
            if let Some(refusal) = choice.delta.refusal.filter(|s| !s.is_empty()) {
                self.refusal.push_str(&refusal);
                events.push(StreamEvent::RefusalDelta(refusal));
            }
            for delta in choice.delta.tool_calls.into_iter().flatten() {
                self.ingest_tool_call(delta, &mut events);
            }
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(reason);
            }
        }
        events
    }

    fn ingest_tool_call(&mut self, delta: ToolCallDelta, events: &mut Vec<StreamEvent>) {
        // Resolve to an index first so the new-call branch needs no fallible
        // re-borrow (the just-pushed element is at `len - 1`).
        let pos = match self.tool_calls.iter().position(|c| c.index == delta.index) {
            Some(pos) => pos,
            None => {
                self.tool_calls.push(PartialToolCall {
                    index: delta.index,
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                    announced: false,
                });
                self.tool_calls.len() - 1
            }
        };
        let call = &mut self.tool_calls[pos];
        if let Some(id) = delta.id {
            call.id = id;
        }
        if let Some(function) = delta.function {
            if let Some(name) = function.name {
                call.name = name;
            }
            if let Some(arguments) = function.arguments {
                call.arguments.push_str(&arguments);
            }
        }
        // Announce once id and name are both known — they arrive together in the
        // first fragment, before the arguments finish streaming.
        if !call.announced && !call.id.is_empty() && !call.name.is_empty() {
            call.announced = true;
            events.push(StreamEvent::ToolCallStart {
                index: call.index,
                id: call.id.clone(),
                name: call.name.clone(),
            });
        }
    }

    /// Builds the terminal [`StreamEvent::Completed`] from accumulated state.
    ///
    /// Content order mirrors the non-streaming projection: text, then tool calls,
    /// then reasoning.
    ///
    /// # Errors
    ///
    /// [`CompletionError::ResponseError`] if no content accumulated (empty
    /// [`NonEmptyVec`]) or a tool call's assembled arguments are not valid JSON.
    fn finish(self) -> Result<StreamEvent, CompletionError> {
        let mut content = Vec::new();
        if !self.text.trim().is_empty() {
            content.push(AssistantContent::text(self.text));
        }
        let mut tool_calls = self.tool_calls;
        tool_calls.sort_by_key(|c| c.index);
        for call in tool_calls {
            let arguments = if call.arguments.trim().is_empty() {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(&call.arguments).map_err(|err| {
                    CompletionError::ResponseError(format!(
                        "tool_call `{}` arguments are not valid JSON: {err}",
                        call.name
                    ))
                })?
            };
            content.push(AssistantContent::tool_call(call.id, call.name, arguments));
        }
        if !self.reasoning.is_empty() {
            content.push(AssistantContent::reasoning(self.reasoning));
        }
        if !self.refusal.is_empty() {
            content.push(AssistantContent::refusal(self.refusal));
        }

        let content = NonEmptyVec::try_from(content).map_err(|err| {
            CompletionError::ResponseError(format!("stream produced no assistant content: {err}"))
        })?;

        Ok(StreamEvent::Completed {
            content,
            usage: self.usage.unwrap_or_default(),
            finish_reason: map_finish_reason(self.finish_reason.as_deref()),
        })
    }
}

/// Wraps a mid-stream provider [`StreamErrorBody`] as a [`CompletionError`].
fn stream_error(error: StreamErrorBody) -> CompletionError {
    let detail = match error.kind {
        Some(kind) => format!("{} ({kind})", error.message),
        None => error.message,
    };
    CompletionError::ResponseError(format!("provider reported a mid-stream error: {detail}"))
}

/// Maps a Chat Completions stop-reason string onto the neutral [`FinishReason`]. A
/// missing reason (stream ended without one) reads as a natural stop.
fn map_finish_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        None | Some("stop") => FinishReason::Stop,
        Some("length") => FinishReason::Length,
        Some("tool_calls") => FinishReason::ToolCalls,
        Some("content_filter") => FinishReason::ContentFilter,
        Some(other) => FinishReason::Other(other.to_string()),
    }
}

/// Drives a streaming `/chat/completions` response body into a
/// [`CompletionStream`].
///
/// Reads the body chunk by chunk via [`reqwest::Response::chunk`] (so no
/// `StreamExt` import is needed), decodes SSE frames, and folds them through a
/// [`StreamAssembler`], yielding render deltas as they arrive and a final
/// [`StreamEvent::Completed`] once the body ends or `[DONE]` is seen. Dropping
/// the returned stream closes the HTTP response and halts generation.
pub(crate) fn stream_events<U>(mut response: reqwest::Response) -> CompletionStream
where
    U: DeserializeOwned + Into<Usage> + Send + 'static,
{
    Box::pin(try_stream! {
        let mut decoder = SseDecoder::new();
        let mut assembler = StreamAssembler::default();
        'body: while let Some(bytes) = response.chunk().await? {
            for event in decoder.push(&bytes) {
                match event {
                    SseEvent::Done => break 'body,
                    SseEvent::Data(payload) => {
                        let mut chunk: StreamChunk<U> = serde_json::from_str(&payload)?;
                        // A mid-stream error frame ends the stream with an error;
                        // otherwise the partial answer would be assembled and
                        // reported as a clean completion.
                        if let Some(error) = chunk.error.take() {
                            Err::<(), CompletionError>(stream_error(error))?;
                        }
                        for delta in assembler.ingest(chunk) {
                            yield delta;
                        }
                    }
                }
            }
        }
        yield assembler.finish()?;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::deepseek::protocol::Usage as TestUsage;

    /// Runs `sse` through a fresh decoder + assembler, splitting the input into
    /// `chunk_size`-byte network chunks to exercise cross-chunk buffering.
    fn run(sse: &str, chunk_size: usize) -> Vec<StreamEvent> {
        let mut decoder = SseDecoder::new();
        let mut assembler = StreamAssembler::default();
        let mut events = Vec::new();
        let mut done = false;
        for piece in sse.as_bytes().chunks(chunk_size) {
            for ev in decoder.push(piece) {
                match ev {
                    SseEvent::Done => done = true,
                    SseEvent::Data(payload) => {
                        let chunk: StreamChunk<TestUsage> =
                            serde_json::from_str(&payload).expect("chunk json");
                        events.extend(assembler.ingest(chunk));
                    }
                }
            }
        }
        assert!(done, "test SSE must end with [DONE]");
        events.push(assembler.finish().expect("finish"));
        events
    }

    fn completed(events: Vec<StreamEvent>) -> (NonEmptyVec<AssistantContent>, FinishReason) {
        match events.into_iter().next_back().expect("at least Completed") {
            StreamEvent::Completed {
                content,
                finish_reason,
                ..
            } => (content, finish_reason),
            other => panic!("last event was not Completed: {other:?}"),
        }
    }

    const TEXT_STREAM: &str = "\
data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}

data: [DONE]

";

    #[test]
    fn text_deltas_then_completed() {
        let events = run(TEXT_STREAM, 4096);
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, ["Hel", "lo"]);

        let (content, finish) = completed(events);
        assert_eq!(finish, FinishReason::Stop);
        assert!(matches!(content.first(), AssistantContent::Text(t) if t.text_ref() == "Hello"));
    }

    #[test]
    fn byte_by_byte_chunking_matches_whole() {
        // Feeding one byte at a time must reassemble identically: the decoder's
        // cross-chunk buffering is the thing under test.
        let one = run(TEXT_STREAM, 1);
        let texts: String = one
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, "Hello");
    }

    #[test]
    fn reasoning_streams_on_its_own_channel() {
        let sse = "\
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think \"}}]}

data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hard\"}}]}

data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}

data: [DONE]

";
        let events = run(sse, 4096);
        let reasoning: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ReasoningDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning, "think hard");

        let (content, _) = completed(events);
        // text first, reasoning last — the non-streaming content order.
        assert!(matches!(content.first(), AssistantContent::Text(t) if t.text_ref() == "answer"));
        assert!(
            content
                .iter()
                .any(|c| matches!(c, AssistantContent::Reasoning(_))),
            "reasoning must be assembled into the final content"
        );
    }

    #[test]
    fn refusal_streams_and_is_preserved_in_completed_content() {
        let sse = "\
data: {\"choices\":[{\"delta\":{\"refusal\":\"Cannot \"}}]}

data: {\"choices\":[{\"delta\":{\"refusal\":\"comply\"},\"finish_reason\":\"stop\"}]}

data: [DONE]

";
        let events = run(sse, 4096);
        let refusal: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::RefusalDelta(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(refusal, "Cannot comply");

        let (content, _) = completed(events);
        assert!(matches!(
            content.first(),
            AssistantContent::Refusal(value) if value.text_ref() == "Cannot comply"
        ));
    }

    #[test]
    fn tool_call_arguments_assemble_across_fragments() {
        let sse = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]}}]}

data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\"}}]}}]}

data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"NYC\\\"}\"}}]}}]}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}

data: [DONE]

";
        let events = run(sse, 4096);

        // ToolCallStart fires once, as soon as id+name are known.
        let starts: Vec<(&str, &str)> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCallStart { id, name, .. } => Some((id.as_str(), name.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(starts, [("call_1", "get_weather")]);

        let (content, finish) = completed(events);
        assert_eq!(finish, FinishReason::ToolCalls);
        let tool_call = content
            .iter()
            .find_map(|c| match c {
                AssistantContent::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .expect("a tool call");
        assert_eq!(tool_call.function.name, "get_weather");
        assert_eq!(
            tool_call.function.arguments,
            serde_json::json!({ "city": "NYC" })
        );
    }

    #[test]
    fn malformed_chunk_json_is_an_error() {
        let mut decoder = SseDecoder::new();
        let lines = decoder.push(b"data: {not json}\n\n");
        let payload = match lines.first() {
            Some(SseEvent::Data(p)) => p,
            other => panic!("expected a data line, got {other:?}"),
        };
        assert!(serde_json::from_str::<StreamChunk<TestUsage>>(payload).is_err());
    }

    #[test]
    fn empty_stream_finishes_with_a_response_error() {
        // No content at all → NonEmptyVec rejects → ResponseError, never a panic.
        let assembler = StreamAssembler::default();
        assert!(matches!(
            assembler.finish(),
            Err(CompletionError::ResponseError(_))
        ));
    }

    #[test]
    fn mid_stream_error_frame_surfaces_as_error() {
        // Some content, then a `data: {"error":...}` frame, then a graceful close
        // with no `[DONE]` — mirrors `stream_events`' per-chunk handling.
        let sse = "\
data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}

data: {\"error\":{\"message\":\"rate limited\",\"type\":\"server_error\"}}

";
        let mut decoder = SseDecoder::new();
        let mut assembler = StreamAssembler::default();
        let mut deltas = Vec::new();
        let mut error = None;
        'outer: for piece in sse.as_bytes().chunks(4096) {
            for ev in decoder.push(piece) {
                if let SseEvent::Data(payload) = ev {
                    let mut chunk: StreamChunk<TestUsage> =
                        serde_json::from_str(&payload).expect("chunk json");
                    if let Some(err) = chunk.error.take() {
                        error = Some(stream_error(err));
                        break 'outer;
                    }
                    deltas.extend(assembler.ingest(chunk));
                }
            }
        }
        // The partial content streamed as a delta...
        assert!(matches!(deltas.as_slice(), [StreamEvent::TextDelta(t)] if t == "Hel"));
        // ...but the turn ends in an error, not a silent clean completion.
        assert!(matches!(
            error,
            Some(CompletionError::ResponseError(m)) if m.contains("rate limited")
        ));
    }

    #[test]
    fn usage_frame_missing_a_core_count_is_rejected() {
        // The standard trio is required: a usage object missing one is malformed
        // and must fail the parse, not silently read as zero.
        let bad = r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;
        assert!(serde_json::from_str::<StreamChunk<TestUsage>>(bad).is_err());
        // The DeepSeek cache extensions, by contrast, may be omitted.
        let ok =
            r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#;
        assert!(serde_json::from_str::<StreamChunk<TestUsage>>(ok).is_ok());
    }
}
