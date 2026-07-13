//! Durable tool-result artifact inputs and commit receipts.

use super::{Seq, SessionStoreError};

/// Tool-result payload captured by the harness before active-context trimming.
///
/// The artifact is not exposed as a workspace file. Future compaction code uses
/// the returned [`ToolArtifactRef`] as a short marker in the active context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewToolArtifact {
    artifact_id: String,
    content_hash: String,
    bytes: i64,
    preview: String,
    payload_text: Option<String>,
    storage_ref: Option<String>,
}

impl NewToolArtifact {
    /// Builds an inline artifact from a tool-result payload already visible to
    /// the model.
    ///
    /// # Errors
    /// Returns [`SessionStoreError::InvalidToolArtifact`] when `content_hash` is
    /// empty, or [`SessionStoreError::ToolArtifactTooLarge`] if the payload
    /// length cannot fit SQLite's signed integer range.
    pub fn inline(
        content_hash: impl Into<String>,
        preview: impl Into<String>,
        payload: impl Into<String>,
    ) -> Result<Self, SessionStoreError> {
        let content_hash = non_empty_artifact_field("content_hash", content_hash.into())?;
        let payload = payload.into();
        let bytes =
            i64::try_from(payload.len()).map_err(|_| SessionStoreError::ToolArtifactTooLarge {
                bytes: payload.len(),
            })?;
        Ok(Self {
            artifact_id: format!("tool-result-{content_hash}"),
            content_hash,
            bytes,
            preview: preview.into(),
            payload_text: Some(payload),
            storage_ref: None,
        })
    }

    /// Returns the artifact identifier used for idempotent writes and references.
    pub fn artifact_id(&self) -> &str {
        &self.artifact_id
    }

    /// Returns the digest bound to the complete payload.
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    /// Returns the UTF-8 byte length of the complete payload.
    pub fn bytes(&self) -> i64 {
        self.bytes
    }

    /// Returns the short preview safe to retain in the active context.
    pub fn preview(&self) -> &str {
        &self.preview
    }

    /// Returns the inline payload persisted directly by SQLite.
    pub fn payload_text(&self) -> Option<&str> {
        self.payload_text.as_deref()
    }

    /// Returns the external object-store reference, absent for inline artifacts.
    pub fn storage_ref(&self) -> Option<&str> {
        self.storage_ref.as_deref()
    }
}

/// Stable reference returned after a tool artifact is durably recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolArtifactRef {
    pub(crate) artifact_id: String,
    pub(crate) content_hash: String,
    pub(crate) bytes: i64,
    pub(crate) preview: String,
}

/// Receipt proving that the artifact write crossed the SQLite commit boundary.
///
/// [`journal_seq`](Self::journal_seq) lets the runner advance the session frontier
/// only after both the complete payload and its audit record are durable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedArtifact {
    reference: ToolArtifactRef,
    journal_seq: Seq,
}

impl CommittedArtifact {
    pub(crate) const fn new(reference: ToolArtifactRef, journal_seq: Seq) -> Self {
        Self {
            reference,
            journal_seq,
        }
    }

    /// Returns the stable reference projected into an active-context marker.
    pub fn reference(&self) -> &ToolArtifactRef {
        &self.reference
    }

    /// Returns the journal sequence of the artifact's initial audit commit.
    pub const fn journal_seq(&self) -> Seq {
        self.journal_seq
    }
}

impl ToolArtifactRef {
    /// Returns the stable artifact identifier for the complete payload.
    pub fn artifact_id(&self) -> &str {
        &self.artifact_id
    }

    /// Returns the content digest used to verify the complete payload.
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    /// Returns the UTF-8 byte length of the complete payload.
    pub fn bytes(&self) -> i64 {
        self.bytes
    }

    /// Returns the short preview written to the active-context marker.
    pub fn preview(&self) -> &str {
        &self.preview
    }
}

fn non_empty_artifact_field(
    field: &'static str,
    value: String,
) -> Result<String, SessionStoreError> {
    if value.trim().is_empty() {
        Err(SessionStoreError::InvalidToolArtifact(format!(
            "`{field}` must not be empty"
        )))
    } else {
        Ok(value)
    }
}
