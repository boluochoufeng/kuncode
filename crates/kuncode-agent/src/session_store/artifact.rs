//! Store-neutral tool-result artifact inputs and durable commit receipts.

use sha2::{Digest, Sha256};

use super::{SessionStoreError, hash::is_canonical_sha256};

mod receipt;

pub use receipt::{CommittedArtifact, ToolArtifactRef};

const CONTENT_HASH_PREFIX: &str = "sha256-";

/// Tool-result payload captured by the harness before active-context trimming.
///
/// Exactly one complete-payload source is retained by the store. The artifact is not
/// exposed as a workspace file; compaction uses the returned [`ToolArtifactRef`] as a
/// short marker in the active context.
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
    /// Returns an error when `content_hash` is non-canonical or does not match
    /// `payload`, or when the payload length cannot fit the durable byte-count field.
    pub fn inline(
        content_hash: impl Into<String>,
        preview: impl Into<String>,
        payload: impl Into<String>,
    ) -> Result<Self, SessionStoreError> {
        let content_hash = content_hash.into();
        let payload = payload.into();
        let bytes =
            i64::try_from(payload.len()).map_err(|_| SessionStoreError::ToolArtifactTooLarge {
                bytes: payload.len(),
            })?;
        let artifact = Self {
            artifact_id: format!("tool-result-{content_hash}"),
            content_hash,
            bytes,
            preview: preview.into(),
            payload_text: Some(payload),
            storage_ref: None,
        };
        artifact.validate_identity()?;
        Ok(artifact)
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

    /// Returns the preview proposed for the active-context marker.
    pub fn preview(&self) -> &str {
        &self.preview
    }

    /// Returns the inline payload persisted directly by the artifact store.
    pub fn payload_text(&self) -> Option<&str> {
        self.payload_text.as_deref()
    }

    /// Returns the external object-store reference, absent for inline artifacts.
    pub fn storage_ref(&self) -> Option<&str> {
        self.storage_ref.as_deref()
    }

    pub(crate) fn validate_identity(&self) -> Result<(), SessionStoreError> {
        validate_artifact_id(&self.artifact_id, &self.content_hash)?;
        let source = artifact_source(self.payload_text(), self.storage_ref())?;
        validate_artifact_content(&self.content_hash, self.bytes, source)
    }
}

pub(crate) enum ArtifactSource<'payload> {
    Inline(&'payload str),
    External,
}

pub(crate) fn artifact_source<'payload>(
    payload_text: Option<&'payload str>,
    storage_ref: Option<&'payload str>,
) -> Result<ArtifactSource<'payload>, SessionStoreError> {
    match (payload_text, storage_ref) {
        (Some(payload), None) => Ok(ArtifactSource::Inline(payload)),
        (None, Some(reference)) if !reference.trim().is_empty() => Ok(ArtifactSource::External),
        (None, Some(_)) => Err(SessionStoreError::InvalidToolArtifact(
            "`storage_ref` must not be empty".to_string(),
        )),
        (Some(_), Some(_)) | (None, None) => Err(SessionStoreError::InvalidToolArtifact(
            "exactly one of `payload_text` or `storage_ref` must be present".to_string(),
        )),
    }
}

pub(crate) fn validate_artifact_content(
    content_hash: &str,
    bytes: i64,
    source: ArtifactSource<'_>,
) -> Result<(), SessionStoreError> {
    let digest = content_hash
        .strip_prefix(CONTENT_HASH_PREFIX)
        .filter(|value| is_canonical_sha256(value))
        .ok_or_else(|| SessionStoreError::InvalidToolArtifactHashFormat {
            content_hash: content_hash.to_string(),
        })?;
    if bytes < 0 {
        return Err(SessionStoreError::InvalidToolArtifact(
            "`bytes` must not be negative".to_string(),
        ));
    }
    match source {
        ArtifactSource::Inline(payload) => {
            let computed_digest = format!("{:x}", Sha256::digest(payload.as_bytes()));
            if digest != computed_digest {
                return Err(SessionStoreError::ToolArtifactDigestMismatch {
                    claimed: content_hash.to_string(),
                    computed: format!("{CONTENT_HASH_PREFIX}{computed_digest}"),
                });
            }
            let actual_bytes = i64::try_from(payload.len()).map_err(|_| {
                SessionStoreError::ToolArtifactTooLarge {
                    bytes: payload.len(),
                }
            })?;
            if bytes != actual_bytes {
                return Err(SessionStoreError::InvalidToolArtifact(format!(
                    "`bytes` must equal inline payload length {actual_bytes}, found {bytes}"
                )));
            }
        }
        ArtifactSource::External => {}
    }
    Ok(())
}

pub(crate) fn validate_artifact_id(
    artifact_id: &str,
    content_hash: &str,
) -> Result<(), SessionStoreError> {
    let expected = format!("tool-result-{content_hash}");
    if artifact_id == expected {
        Ok(())
    } else {
        Err(SessionStoreError::InvalidToolArtifact(format!(
            "`artifact_id` must equal `{expected}`"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::NewToolArtifact;
    use crate::session_store::SessionStoreError;

    #[test]
    fn inline_rejects_non_canonical_content_hash() {
        // Given: a digest with the right algorithm label but a non-canonical body.
        let hash = "sha256-DEADBEEF";

        // When: the payload crosses the artifact construction boundary.
        let result = NewToolArtifact::inline(hash, "preview", "payload");

        // Then: callers receive a typed format error before persistence.
        assert!(matches!(
            result,
            Err(SessionStoreError::InvalidToolArtifactHashFormat { content_hash })
                if content_hash == hash
        ));
    }

    #[test]
    fn inline_rejects_digest_that_does_not_match_payload() {
        // Given: a canonical SHA-256 string forged for a different payload.
        let claimed = format!("sha256-{}", "0".repeat(64));

        // When: the artifact binds the claimed digest to its inline payload.
        let result = NewToolArtifact::inline(&claimed, "preview", "payload");

        // Then: the mismatch is reported without exposing the payload.
        assert!(matches!(
            result,
            Err(SessionStoreError::ToolArtifactDigestMismatch {
                claimed: value,
                computed,
            }) if value == claimed
                && computed
                    == "sha256-239f59ed55e737c77147cf55ad0c1b030b6d7ee748a7426952f9b852d5a935e5"
        ));
    }
}
