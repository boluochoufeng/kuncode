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

/// Exact source position for one artifact-pass decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArtifactResultLocation {
    /// Group containing the closed tool exchange.
    pub group_index: usize,
    /// Result-message position within the exchange.
    pub result_message_index: usize,
    /// Tool-result block position within the user message.
    pub content_index: usize,
}

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
    BelowThreshold(BelowThresholdArtifact),
    /// The item stayed inline because one isolated operation failed.
    Failed {
        /// Exact result retained inline after the failure.
        location: ArtifactResultLocation,
        /// Primary tool call identifier.
        tool_call_id: String,
        /// Typed failure category without the complete payload.
        failure: ArtifactSpillFailure,
    },
    /// A committed receipt authorized replacement in the candidate view.
    Spilled {
        /// Exact result replaced after its durable receipt.
        location: ArtifactResultLocation,
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

impl ArtifactSpillOutcome {
    /// Returns the exact result inspected by the artifact pass.
    pub const fn location(&self) -> ArtifactResultLocation {
        match self {
            Self::BelowThreshold(artifact) => artifact.location,
            Self::Failed { location, .. } | Self::Spilled { location, .. } => *location,
        }
    }
}

/// Opaque authorization for slimming one exact artifact-pass input.
///
/// Construction remains inside the artifact pass so callers cannot attach a
/// token count or journal sequence to a different payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BelowThresholdArtifact {
    location: ArtifactResultLocation,
    tool_call_id: String,
    tokens: u64,
    source_hash: String,
    source_journal_seq: Option<Seq>,
}

impl BelowThresholdArtifact {
    pub(super) fn new(
        location: ArtifactResultLocation,
        tool_call_id: String,
        tokens: u64,
        source_hash: String,
        source_journal_seq: Option<Seq>,
    ) -> Self {
        Self {
            location,
            tool_call_id,
            tokens,
            source_hash,
            source_journal_seq,
        }
    }

    /// Returns the exact result inspected by the artifact pass.
    pub const fn location(&self) -> ArtifactResultLocation {
        self.location
    }

    /// Returns the provider-visible cost measured by the artifact pass.
    pub const fn tokens(&self) -> u64 {
        self.tokens
    }

    /// Returns the stable hash of the complete source [`ToolResult`].
    pub fn source_hash(&self) -> &str {
        &self.source_hash
    }

    /// Returns durable per-message lineage when the journal exposes it.
    pub const fn source_journal_seq(&self) -> Option<Seq> {
        self.source_journal_seq
    }

    pub(crate) fn tool_call_id(&self) -> &str {
        &self.tool_call_id
    }
}

/// Candidate groups and durable frontier produced by one spill pass.
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactSpillResult {
    pub(super) session_id: SessionId,
    pub(super) groups: Vec<ProtocolGroup>,
    pub(super) source_frontier: Seq,
    pub(super) frontier: Seq,
    pub(super) outcomes: Vec<ArtifactSpillOutcome>,
}

impl ArtifactSpillResult {
    pub(super) fn new(
        session_id: SessionId,
        groups: Vec<ProtocolGroup>,
        source_frontier: Seq,
        outcomes: Vec<ArtifactSpillOutcome>,
    ) -> Self {
        Self {
            session_id,
            groups,
            source_frontier,
            frontier: source_frontier,
            outcomes,
        }
    }

    /// Returns the durable session audited before this pass began.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Returns the journal frontier that authorized the source messages.
    pub const fn source_frontier(&self) -> Seq {
        self.source_frontier
    }

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
