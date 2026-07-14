//! Tool-artifact references and durable commit receipts.

use super::NewToolArtifact;
use crate::session_store::{Seq, SessionId};

/// Stable reference returned after a tool artifact is durably recorded.
///
/// The reference carries only artifact id, digest, byte count, and preview; it never
/// carries the complete payload or its storage location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolArtifactRef {
    pub(crate) artifact_id: String,
    pub(crate) content_hash: String,
    pub(crate) bytes: i64,
    pub(crate) preview: String,
}

/// Store-issued receipt that authorizes artifact-marker installation.
///
/// [`journal_seq`](Self::journal_seq) lets the runner advance the session frontier
/// only after the store reports both the complete payload and its audit record durable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedArtifact {
    session_id: SessionId,
    reference: ToolArtifactRef,
    journal_seq: Seq,
}

impl CommittedArtifact {
    /// Builds a receipt from facts reported by a store after a durable write.
    ///
    /// Custom [`SessionStore`](crate::session_store::SessionStore) implementations
    /// must call this only after the payload and matching journal fact are durable.
    /// This constructor copies the session, artifact id, digest, byte count, preview,
    /// and supplied sequence. It does not inspect the complete payload, query storage,
    /// or independently validate that durability claim.
    pub fn from_committed_write(
        session_id: SessionId,
        artifact: &NewToolArtifact,
        journal_seq: Seq,
    ) -> Self {
        Self::new(
            session_id,
            ToolArtifactRef {
                artifact_id: artifact.artifact_id.clone(),
                content_hash: artifact.content_hash.clone(),
                bytes: artifact.bytes,
                preview: artifact.preview.clone(),
            },
            journal_seq,
        )
    }

    pub(crate) const fn new(
        session_id: SessionId,
        reference: ToolArtifactRef,
        journal_seq: Seq,
    ) -> Self {
        Self {
            session_id,
            reference,
            journal_seq,
        }
    }

    /// Returns the session whose artifact journal contains this receipt.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Returns the stable reference projected into an active-context marker.
    pub fn reference(&self) -> &ToolArtifactRef {
        &self.reference
    }

    /// Returns the journal sequence of the artifact's initial audit commit.
    pub const fn journal_seq(&self) -> Seq {
        self.journal_seq
    }

    /// Checks the receipt fields required to install the requested marker.
    ///
    /// This compares a positive journal sequence, session, artifact id, digest, byte
    /// count, and preview. It does not inspect the complete payload or prove storage
    /// durability; those remain obligations of the issuing [`SessionStore`](crate::session_store::SessionStore).
    pub(crate) fn proves(&self, session: &SessionId, artifact: &NewToolArtifact) -> bool {
        self.journal_seq > Seq::ZERO
            && self.session_id == *session
            && self.reference.artifact_id == artifact.artifact_id
            && self.reference.content_hash == artifact.content_hash
            && self.reference.bytes == artifact.bytes
            && self.reference.preview == artifact.preview
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
