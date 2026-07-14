//! Persists one complete artifact before authorizing its candidate marker.

use kuncode_core::completion::{ToolResult, ToolResultContent};

use super::{SpillRuntime, failed};
use crate::{
    compaction::artifact::{
        ArtifactResultLocation, ArtifactSpillError, ArtifactSpillFailure, ArtifactSpillOutcome,
        ArtifactSpillResult,
        hash::sha256_hex,
        marker::{MarkerPayload, MarkerSource, build_marker_result},
        preview::canonical_artifact_preview,
    },
    session_store::NewToolArtifact,
    tool::ToolOutput,
};

pub(super) async fn spill_large_result(
    runtime: &SpillRuntime<'_>,
    result: &ToolResult,
    tool_name: &str,
    location: ArtifactResultLocation,
    tokens: u64,
    pass: &mut ArtifactSpillResult,
) -> Result<Option<ToolResult>, ArtifactSpillError> {
    let payload = match payload_text(result) {
        Ok(payload) => payload,
        Err(failure) => return Ok(failed(pass, result, location, failure)),
    };
    let output = match serde_json::from_str::<ToolOutput>(payload) {
        Ok(output) => output,
        Err(error) => {
            return Ok(failed(
                pass,
                result,
                location,
                ArtifactSpillFailure::Parse(error.to_string()),
            ));
        }
    };
    let content_hash = format!("sha256-{}", sha256_hex(payload.as_bytes()));
    let source = match MarkerSource::new(
        tool_name,
        result,
        MarkerPayload {
            output: &output,
            content_hash: &content_hash,
            text: payload,
            tokens,
        },
    ) {
        Ok(source) => source,
        Err(failure) => return Ok(failed(pass, result, location, failure)),
    };
    let marker = match build_marker_result(&source, runtime.counter).await {
        Ok(marker) => marker,
        Err(failure) => return Ok(failed(pass, result, location, failure)),
    };
    let artifact = match NewToolArtifact::inline(
        &content_hash,
        canonical_artifact_preview(payload),
        payload,
    ) {
        Ok(artifact) => artifact,
        Err(error) => {
            return Ok(failed(
                pass,
                result,
                location,
                ArtifactSpillFailure::Store(error.to_string()),
            ));
        }
    };
    // The complete payload must become durable before the provider-visible
    // candidate can contain a marker that no longer carries that payload.
    let expected_artifact = artifact.clone();
    let receipt = match runtime
        .store
        .put(runtime.session, pass.frontier, artifact)
        .await
    {
        Ok(receipt) => receipt,
        Err(crate::session_store::SessionStoreError::JournalHeadConflict { expected, actual }) => {
            return Err(ArtifactSpillError::JournalHeadConflict { expected, actual });
        }
        Err(crate::session_store::SessionStoreError::CommitOutcomeUnknown {
            operation,
            message,
        }) => {
            return Err(ArtifactSpillError::PersistenceOutcomeUnknown { operation, message });
        }
        Err(error) if error.invalidates_artifact_authority() => {
            return Err(ArtifactSpillError::ArtifactIntegrity(error.to_string()));
        }
        Err(error) => {
            return Ok(failed(
                pass,
                result,
                location,
                ArtifactSpillFailure::Store(error.to_string()),
            ));
        }
    };
    // A successful return alone is insufficient: the receipt must bind the
    // same session, artifact identity, content hash, size, and preview.
    if !receipt.proves(runtime.session, &expected_artifact) {
        return Err(ArtifactSpillError::ReceiptMismatch);
    }
    // Advance authority before exposing the marker so subsequent writes use
    // the durable receipt's journal coordinate rather than the audit frontier.
    pass.frontier = pass.frontier.max(receipt.journal_seq());
    pass.outcomes.push(ArtifactSpillOutcome::Spilled {
        location,
        tool_call_id: result.id.clone(),
        artifact_id: receipt.reference().artifact_id().to_string(),
        journal_seq: receipt.journal_seq(),
        original_tokens: tokens,
    });
    Ok(Some(marker.result))
}

fn payload_text(result: &ToolResult) -> Result<&str, ArtifactSpillFailure> {
    if result.content.len() != 1 {
        return Err(ArtifactSpillFailure::Parse(
            "tool result must contain exactly one JSON text block".to_string(),
        ));
    }
    match result.content.first() {
        ToolResultContent::Text(text) => Ok(text.text_ref()),
    }
}
