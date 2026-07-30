//! Bounded marker construction for explicitly authorized tool results.
//!
//! Marker preparation fails safe to verbatim retention unless the source has
//! durable lineage, represents a successful complete harness output, and the
//! final marker is both capped and strictly cheaper than the original result.

use kuncode_core::{
    completion::{AssistantContent, Message, ToolResult, ToolResultContent},
    non_empty_vec::NonEmptyVec,
};
use serde::Serialize;
use serde_json::Value;

use super::SlimmingRetention;
use crate::{
    compaction::artifact::{ArtifactTokenCounter, adaptive_preview},
    session_store::Seq,
    tool::ToolOutput,
};

const INITIAL_PREVIEW_BYTES: usize = 2_048;
const MARKER_LIMIT_TOKENS: u64 = 2_048;
const METADATA_BYTES: usize = 512;

pub(super) enum PreparedSlimming {
    Marker { result: ToolResult, tokens: u64 },
    Retain(SlimmingRetention),
}

#[derive(Serialize)]
struct SlimmedToolResultMarker {
    schema_version: u8,
    kind: &'static str,
    tool_name: String,
    tool_call_id: String,
    ok: bool,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<Value>,
    original_journal_seq: i64,
    preview: String,
}

pub(super) async fn prepare_slimmed_result(
    assistant: &Message,
    result: &ToolResult,
    original_journal_seq: Seq,
    original_tokens: u64,
    counter: &dyn ArtifactTokenCounter,
) -> PreparedSlimming {
    let Some(payload) = payload_text(result) else {
        return PreparedSlimming::Retain(SlimmingRetention::Parse);
    };
    let Ok(output) = serde_json::from_str::<ToolOutput>(payload) else {
        return PreparedSlimming::Retain(SlimmingRetention::Parse);
    };
    // Error and truncation details may be necessary to diagnose or continue the
    // current task, so a bounded projection cannot safely replace either form.
    if !output.ok {
        return PreparedSlimming::Retain(SlimmingRetention::FailedOutput);
    }
    if output.truncated {
        return PreparedSlimming::Retain(SlimmingRetention::TruncatedOutput);
    }
    let Some(call) = assistant_call(assistant, &result.id) else {
        return PreparedSlimming::Retain(SlimmingRetention::Parse);
    };
    let data = output.data.as_ref().and_then(Value::as_object);
    let arguments = call.function.arguments.as_object();
    // Only preview is reduced; reaching zero distinguishes oversized fixed
    // evidence from a candidate that merely needs a smaller excerpt.
    let mut preview_bytes = INITIAL_PREVIEW_BYTES.min(payload.len());
    loop {
        let marker = SlimmedToolResultMarker {
            schema_version: 1,
            kind: "slimmed_tool_result",
            tool_name: adaptive_preview(&call.function.name, METADATA_BYTES),
            tool_call_id: adaptive_preview(&result.id, METADATA_BYTES),
            ok: output.ok,
            truncated: output.truncated,
            command: bounded_value(
                arguments.and_then(|map| map.get("cmd").or_else(|| map.get("command"))),
            ),
            path: bounded_value(
                arguments
                    .and_then(|map| map.get("path"))
                    .or_else(|| data.and_then(|map| map.get("path"))),
            ),
            exit_code: bounded_value(data.and_then(|map| map.get("exit_code"))),
            original_journal_seq: original_journal_seq.get(),
            preview: adaptive_preview(payload, preview_bytes),
        };
        let Ok(candidate) = marker_result(&marker, result) else {
            return PreparedSlimming::Retain(SlimmingRetention::Parse);
        };
        let marker_tokens = match counter.count(&candidate).await {
            Ok(tokens) => tokens,
            Err(_) => return PreparedSlimming::Retain(SlimmingRetention::Count),
        };
        if marker_tokens <= MARKER_LIMIT_TOKENS && marker_tokens < original_tokens {
            return PreparedSlimming::Marker {
                result: candidate,
                tokens: marker_tokens,
            };
        }
        if preview_bytes == 0 {
            let reason = if marker_tokens > MARKER_LIMIT_TOKENS {
                SlimmingRetention::MarkerTooLarge
            } else {
                SlimmingRetention::NoSavings
            };
            return PreparedSlimming::Retain(reason);
        }
        preview_bytes /= 2;
    }
}

fn payload_text(result: &ToolResult) -> Option<&str> {
    if result.content.len() != 1 {
        return None;
    }
    match result.content.first() {
        ToolResultContent::Text(text) => Some(text.text_ref()),
    }
}

fn assistant_call<'a>(
    assistant: &'a Message,
    result_id: &str,
) -> Option<&'a kuncode_core::completion::ToolCall> {
    let Message::Assistant { content, .. } = assistant else {
        return None;
    };
    content.iter().find_map(|block| match block {
        AssistantContent::ToolCall(call) if call.id == result_id => Some(call),
        AssistantContent::Text(_)
        | AssistantContent::Reasoning(_)
        | AssistantContent::Refusal(_)
        | AssistantContent::ToolCall(_) => None,
    })
}

fn bounded_value(value: Option<&Value>) -> Option<Value> {
    match value? {
        Value::String(text) => Some(Value::String(adaptive_preview(text, METADATA_BYTES))),
        Value::Number(number) => Some(Value::Number(number.clone())),
        Value::Bool(value) => Some(Value::Bool(*value)),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

fn marker_result(
    marker: &SlimmedToolResultMarker,
    source: &ToolResult,
) -> Result<ToolResult, serde_json::Error> {
    Ok(ToolResult {
        id: source.id.clone(),
        call_id: source.call_id.clone(),
        content: NonEmptyVec::new(ToolResultContent::text(serde_json::to_string(marker)?)),
    })
}
