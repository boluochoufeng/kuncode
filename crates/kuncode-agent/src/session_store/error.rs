//! Errors exposed by durable session storage.

/// Failures while persisting session journals, checkpoints, and artifacts.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SessionStoreError {
    /// A directory, permission, or storage-file operation failed.
    #[error("session store I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// A SQLite query or transaction failed.
    #[error("session store database failed: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// SQLite may have committed even though the client did not receive a receipt.
    #[error("{operation} commit outcome is unknown: {message}")]
    CommitOutcomeUnknown {
        /// Durable operation whose receipt could not be established.
        operation: &'static str,
        /// Storage error returned after commit was attempted.
        message: String,
    },
    /// A versioned payload could not be encoded or decoded as JSON.
    #[error("session store JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    /// A stored message violates the completion-message protocol.
    #[error("invalid stored message payload: {0}")]
    InvalidMessagePayload(String),
    /// A payload schema version is newer than this implementation supports.
    #[error("unsupported {payload} payload schema version: {version}")]
    UnsupportedPayloadVersion {
        /// Payload category with the incompatible version.
        payload: &'static str,
        /// Schema version found in storage.
        version: u64,
    },
    /// Checkpoint coverage or its source range violates persistence invariants.
    #[error("invalid checkpoint: {0}")]
    InvalidCheckpoint(String),
    /// An artifact lacks an identifier, content digest, or valid payload source.
    #[error("invalid tool artifact: {0}")]
    InvalidToolArtifact(String),
    /// An artifact digest is not the canonical `sha256-<lowercase hex>` form.
    #[error("invalid tool artifact content hash format: `{content_hash}`")]
    InvalidToolArtifactHashFormat {
        /// Rejected digest supplied at the persistence boundary.
        content_hash: String,
    },
    /// An inline payload does not match its claimed content digest.
    #[error("tool artifact digest mismatch: claimed `{claimed}`, computed `{computed}`")]
    ToolArtifactDigestMismatch {
        /// Digest supplied by the artifact producer.
        claimed: String,
        /// Digest computed from the complete inline payload.
        computed: String,
    },
    /// The artifact length cannot be represented by SQLite's signed integer type.
    #[error("tool artifact is too large to store: {bytes} bytes")]
    ToolArtifactTooLarge {
        /// Actual payload byte length supplied by the caller.
        bytes: usize,
    },
    /// A reused artifact identifier no longer names the originally stored content.
    #[error(
        "tool artifact `{artifact_id}` conflicts with durable content in session `{session_id}`"
    )]
    ToolArtifactConflict {
        /// Session containing the durable artifact.
        session_id: String,
        /// Stable identifier whose content binding was violated.
        artifact_id: String,
    },
    /// An idempotent artifact row exists without the durable journal fact required
    /// to issue a receipt.
    #[error("tool artifact `{artifact_id}` has no durable journal entry in session `{session_id}`")]
    ToolArtifactJournalMissing {
        /// Session missing the audit fact.
        session_id: String,
        /// Existing artifact for which no receipt can be issued.
        artifact_id: String,
    },
    /// An artifact row and its durable journal identity disagree.
    #[error(
        "tool artifact `{artifact_id}` has a mismatched journal fact in session `{session_id}`"
    )]
    ToolArtifactJournalMismatch {
        /// Session containing the inconsistent audit fact.
        session_id: String,
        /// Artifact whose journal identity failed validation.
        artifact_id: String,
    },
    /// A compaction commit violates session, source-range, or checkpoint invariants.
    #[error("invalid compaction commit: {0}")]
    InvalidCompaction(String),
    /// The journal advanced after a compaction or artifact candidate was audited,
    /// so the caller must discard the candidate and measure again.
    #[error("journal head conflict: expected {expected}, found {actual}")]
    JournalHeadConflict {
        /// Journal head observed when the candidate was generated or audited.
        expected: i64,
        /// Journal head read by the write transaction.
        actual: i64,
    },
}

impl SessionStoreError {
    pub(crate) fn commit_outcome_unknown(operation: &'static str, error: sqlx::Error) -> Self {
        Self::CommitOutcomeUnknown {
            operation,
            message: error.to_string(),
        }
    }
}
