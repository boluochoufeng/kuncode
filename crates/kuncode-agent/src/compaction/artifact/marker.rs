use kuncode_core::{
    completion::{ToolResult, ToolResultContent},
    non_empty_vec::NonEmptyVec,
};
use serde::Serialize;

use super::{
    preview::adaptive_preview,
    types::{ArtifactSpillFailure, ArtifactTokenCounter},
};
use crate::tool::{ToolErrorKind, ToolErrorPayload, ToolOutput};

const MARKER_LIMIT_TOKENS: u64 = 2_048;
const INITIAL_PREVIEW_BYTES: usize = 8_192;
const TOOL_NAME_BYTES: usize = 256;
const TOOL_CALL_ID_BYTES: usize = 256;
const ERROR_KIND_BYTES: usize = 128;
const ERROR_MESSAGE_BYTES: usize = 512;

pub(super) struct MarkerSource<'a> {
    tool_name: String,
    tool_call_id: String,
    ok: bool,
    error: Option<ToolErrorPayload>,
    truncated: bool,
    artifact_id: String,
    content_hash: &'a str,
    original_bytes: u64,
    original_tokens: u64,
    payload: &'a str,
    result: &'a ToolResult,
}

pub(super) struct MarkerPayload<'a> {
    pub(super) output: &'a ToolOutput,
    pub(super) content_hash: &'a str,
    pub(super) text: &'a str,
    pub(super) tokens: u64,
}

impl<'a> MarkerSource<'a> {
    pub(super) fn new(
        tool_name: &str,
        result: &'a ToolResult,
        payload: MarkerPayload<'a>,
    ) -> Result<Self, ArtifactSpillFailure> {
        Ok(Self {
            tool_name: bounded_field(tool_name, TOOL_NAME_BYTES),
            tool_call_id: bounded_field(&result.id, TOOL_CALL_ID_BYTES),
            ok: payload.output.ok,
            error: payload.output.error.as_ref().map(bounded_error),
            truncated: payload.output.truncated,
            artifact_id: format!("tool-result-{}", payload.content_hash),
            content_hash: payload.content_hash,
            original_bytes: u64::try_from(payload.text.len())
                .map_err(|_| ArtifactSpillFailure::HashLength)?,
            original_tokens: payload.tokens,
            payload: payload.text,
            result,
        })
    }
}

#[derive(Serialize)]
pub(super) struct ToolArtifactMarker {
    schema_version: u8,
    tool_name: String,
    tool_call_id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ToolErrorPayload>,
    truncated: bool,
    artifact_id: String,
    content_hash: String,
    original_bytes: u64,
    original_tokens: u64,
    pub(super) preview: String,
}

pub(super) struct PreparedMarker {
    pub(super) result: ToolResult,
}

pub(super) async fn build_marker_result(
    source: &MarkerSource<'_>,
    counter: &dyn ArtifactTokenCounter,
) -> Result<PreparedMarker, ArtifactSpillFailure> {
    let mut preview_bytes = INITIAL_PREVIEW_BYTES.min(source.payload.len());
    loop {
        let marker = marker_with_preview(source, adaptive_preview(source.payload, preview_bytes));
        let candidate = marker_result(&marker, source.result)?;
        let tokens = counter
            .count(&candidate)
            .await
            .map_err(|error| ArtifactSpillFailure::Count(error.to_string()))?;
        if tokens <= MARKER_LIMIT_TOKENS {
            return Ok(PreparedMarker { result: candidate });
        }
        if preview_bytes == 0 {
            return Err(ArtifactSpillFailure::MarkerTooLarge);
        }
        preview_bytes /= 2;
    }
}

fn marker_with_preview(source: &MarkerSource<'_>, preview: String) -> ToolArtifactMarker {
    ToolArtifactMarker {
        schema_version: 1,
        tool_name: source.tool_name.clone(),
        tool_call_id: source.tool_call_id.clone(),
        ok: source.ok,
        error: source.error.clone(),
        truncated: source.truncated,
        artifact_id: source.artifact_id.clone(),
        content_hash: source.content_hash.to_string(),
        original_bytes: source.original_bytes,
        original_tokens: source.original_tokens,
        preview,
    }
}

fn marker_result(
    marker: &ToolArtifactMarker,
    source: &ToolResult,
) -> Result<ToolResult, ArtifactSpillFailure> {
    let text = serde_json::to_string(marker)
        .map_err(|error| ArtifactSpillFailure::Marker(error.to_string()))?;
    Ok(ToolResult {
        id: source.id.clone(),
        call_id: source.call_id.clone(),
        content: NonEmptyVec::new(ToolResultContent::text(text)),
    })
}

fn bounded_error(error: &ToolErrorPayload) -> ToolErrorPayload {
    ToolErrorPayload {
        kind: ToolErrorKind::from(bounded_field(error.kind.as_str(), ERROR_KIND_BYTES)),
        message: bounded_field(&error.message, ERROR_MESSAGE_BYTES),
    }
}

fn bounded_field(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    adaptive_preview(value, max_bytes)
}
