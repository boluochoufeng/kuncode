use async_trait::async_trait;
use kuncode_core::completion::ToolResult;
use thiserror::Error;

use crate::{
    compaction::protocol::ProtocolGroup,
    session_store::{
        Checkpoint, CommittedArtifact, JournalEntry, NewToolArtifact, Seq, SessionId, SessionStore,
        SessionStoreError,
    },
};

/// Counts the final provider-visible representation of one tool result.
#[async_trait]
pub trait ArtifactTokenCounter: Send + Sync {
    /// Counts a complete result after provider-specific serialization rules.
    ///
    /// # Errors
    /// Returns [`ArtifactTokenCounterError`] when the provider cannot count it.
    async fn count(&self, result: &ToolResult) -> Result<u64, ArtifactTokenCounterError>;
}

/// Provider-safe failure returned by an artifact token counter.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("tool-result token counting failed: {message}")]
pub struct ArtifactTokenCounterError {
    message: String,
}

impl ArtifactTokenCounterError {
    /// Preserves adapter context without coupling compaction to a provider type.
    pub fn provider(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Narrow durable-write seam used by the deterministic spill pass.
#[async_trait]
pub trait ArtifactStore: Send + Sync {
    /// Reads the newest active-context baseline for checkpoint-aware replay.
    ///
    /// # Errors
    /// Returns [`SessionStoreError`] when the durable checkpoint cannot be read.
    async fn latest_checkpoint(
        &self,
        session: &SessionId,
    ) -> Result<Option<Checkpoint>, SessionStoreError>;

    /// Reads the authoritative journal before any lossy candidate is produced.
    ///
    /// # Errors
    /// Returns [`SessionStoreError`] when journal replay cannot be completed.
    async fn replay(
        &self,
        session: &SessionId,
        after: Seq,
    ) -> Result<Vec<JournalEntry>, SessionStoreError>;

    /// Persists a complete payload before any candidate marker is installed.
    ///
    /// # Errors
    /// Returns [`SessionStoreError`] when the journal head changed, the artifact
    /// conflicts with durable content, or the transaction cannot commit.
    async fn put(
        &self,
        session: &SessionId,
        expected_journal_head: Seq,
        artifact: NewToolArtifact,
    ) -> Result<CommittedArtifact, SessionStoreError>;
}

#[async_trait]
impl<T> ArtifactStore for T
where
    T: SessionStore + ?Sized,
{
    async fn latest_checkpoint(
        &self,
        session: &SessionId,
    ) -> Result<Option<Checkpoint>, SessionStoreError> {
        SessionStore::latest_checkpoint(self, session).await
    }

    async fn replay(
        &self,
        session: &SessionId,
        after: Seq,
    ) -> Result<Vec<JournalEntry>, SessionStoreError> {
        self.replay_after(session, after).await
    }

    async fn put(
        &self,
        session: &SessionId,
        expected_journal_head: Seq,
        artifact: NewToolArtifact,
    ) -> Result<CommittedArtifact, SessionStoreError> {
        self.put_tool_artifact(session, expected_journal_head, artifact)
            .await
    }
}

/// Per-result reason that kept an eligible-looking payload inline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtifactSpillFailure {
    /// Provider-visible counting failed.
    Count(String),
    /// The result was not a serialized harness tool output.
    Parse(String),
    /// Stable hashing cannot represent the payload length.
    HashLength,
    /// Marker metadata alone exceeds the provider-visible marker limit.
    MarkerTooLarge,
    /// Deterministic marker serialization failed.
    Marker(String),
    /// Durable persistence failed before candidate replacement.
    Store(String),
}

/// Audit outcome for each old durable tool result inspected by the pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtifactSpillOutcome {
    /// The result did not cross the strict spill threshold.
    BelowThreshold {
        /// Primary tool call identifier.
        tool_call_id: String,
        /// Final provider-visible result cost.
        tokens: u64,
    },
    /// The item stayed inline because one isolated operation failed.
    Failed {
        /// Primary tool call identifier.
        tool_call_id: String,
        /// Typed failure category without the complete payload.
        failure: ArtifactSpillFailure,
    },
    /// A committed receipt authorized replacement in the candidate view.
    Spilled {
        /// Primary tool call identifier.
        tool_call_id: String,
        /// Stable content-addressed artifact identifier.
        artifact_id: String,
        /// Journal sequence proving payload and audit durability.
        journal_seq: Seq,
        /// Original provider-visible result cost.
        original_tokens: u64,
    },
}

/// Candidate groups and durable frontier produced by one spill pass.
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactSpillResult {
    pub(super) groups: Vec<ProtocolGroup>,
    pub(super) frontier: Seq,
    pub(super) outcomes: Vec<ArtifactSpillOutcome>,
}

impl ArtifactSpillResult {
    /// Returns the candidate-only protocol groups.
    pub fn groups(&self) -> &[ProtocolGroup] {
        &self.groups
    }

    /// Returns the latest artifact receipt observed by this pass.
    pub const fn frontier(&self) -> Seq {
        self.frontier
    }

    /// Returns one structured decision per inspected result.
    pub fn outcomes(&self) -> &[ArtifactSpillOutcome] {
        &self.outcomes
    }
}
