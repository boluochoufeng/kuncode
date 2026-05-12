//! `ToolResult`: the structured success value returned by a tool.
//!
//! Constraints (plan §9.4):
//!
//! 1. `summary` must not exceed 200 characters.
//! 2. `inline_content` is bounded — large payloads belong in `content_ref`.
//! 3. If output is persisted to an artifact, `content_ref` must be set.
//! 4. `metadata` is an object or `null`, never a raw blob of text.

use kuncode_core::ArtifactId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Maximum length of `summary`, enforced by [`ToolResult::try_new`] and
/// re-checked by `ToolRuntime` before emitting `tool.completed`.
pub const SUMMARY_MAX_CHARS: usize = 200;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// One-line, human-readable outcome. Lands verbatim in
    /// `tool.completed.summary` and is shown back to the model. Must be
    /// ≤ `SUMMARY_MAX_CHARS` characters; the runtime re-checks before emitting.
    pub summary: String,

    /// Short inline excerpt of the tool's output, bounded by
    /// `ToolLimits.max_inline_output_bytes`. Long outputs must be persisted to
    /// an artifact and referenced via `content_ref`; do not dump unbounded
    /// stdout here.
    pub inline_content: Option<String>,

    /// Pointer to a persisted artifact when output exceeds the inline budget.
    /// `None` means everything fit in `inline_content`. The artifact's
    /// `source_event_id` is always the corresponding `tool.started` event.
    pub content_ref: Option<ArtifactId>,

    /// Structured side-information (paths touched, exit code, byte counts,
    /// truncation flag, etc.). Must be a JSON object or `null` — never a raw
    /// text blob; large bodies belong in `content_ref`.
    pub metadata: Value,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ToolResultError {
    #[error("summary exceeds {SUMMARY_MAX_CHARS} characters (got {len})")]
    SummaryTooLong { len: usize },

    #[error("metadata must be a JSON object or null")]
    MetadataNotObject,
}

impl ToolResult {
    /// Build a `ToolResult`, enforcing the constraints in plan §9.4. The
    /// `summary` budget is measured in characters, not bytes.
    pub fn try_new(
        summary: String,
        inline_content: Option<String>,
        content_ref: Option<ArtifactId>,
        metadata: Value,
    ) -> Result<Self, ToolResultError> {
        let summary_len = summary.chars().count();
        if summary_len > SUMMARY_MAX_CHARS {
            return Err(ToolResultError::SummaryTooLong { len: summary_len });
        }
        if !metadata.is_object() && !metadata.is_null() {
            return Err(ToolResultError::MetadataNotObject);
        }

        Ok(Self { summary, inline_content, content_ref, metadata })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn try_new_accepts_minimal_result() {
        let result =
            ToolResult::try_new("read src/lib.rs (1024 bytes)".to_owned(), Some("hello".to_owned()), None, json!({}))
                .expect("construct");
        assert_eq!(result.summary, "read src/lib.rs (1024 bytes)");
        assert_eq!(result.metadata, json!({}));
    }

    #[test]
    fn try_new_accepts_null_metadata() {
        ToolResult::try_new("ok".to_owned(), None, None, Value::Null).expect("null metadata ok");
    }

    #[test]
    fn try_new_rejects_oversized_summary() {
        let big = "a".repeat(SUMMARY_MAX_CHARS + 1);
        let err = ToolResult::try_new(big, None, None, json!({})).expect_err("must reject");
        assert!(matches!(err, ToolResultError::SummaryTooLong { len } if len == SUMMARY_MAX_CHARS + 1));
    }

    #[test]
    fn try_new_accepts_boundary_summary() {
        let on_limit = "a".repeat(SUMMARY_MAX_CHARS);
        ToolResult::try_new(on_limit, None, None, json!({})).expect("boundary ok");
    }

    #[test]
    fn try_new_rejects_array_metadata() {
        let err = ToolResult::try_new("ok".to_owned(), None, None, json!([1, 2, 3])).expect_err("must reject");
        assert_eq!(err, ToolResultError::MetadataNotObject);
    }

    #[test]
    fn try_new_rejects_scalar_metadata() {
        let err = ToolResult::try_new("ok".to_owned(), None, None, json!("string")).expect_err("must reject");
        assert_eq!(err, ToolResultError::MetadataNotObject);
    }

    #[test]
    fn try_new_counts_summary_chars_not_bytes() {
        let summary = "工".repeat(SUMMARY_MAX_CHARS);
        ToolResult::try_new(summary, None, None, json!({})).expect("200 chars ok");
    }

    #[test]
    fn try_new_rejects_multibyte_summary_over_char_limit() {
        let summary = "工".repeat(SUMMARY_MAX_CHARS + 1);
        let err = ToolResult::try_new(summary, None, None, json!({})).expect_err("must reject");
        assert!(matches!(err, ToolResultError::SummaryTooLong { len } if len == SUMMARY_MAX_CHARS + 1));
    }
}
