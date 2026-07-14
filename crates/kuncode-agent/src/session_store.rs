//! Durable session history storage.
//!
//! The store owns the complete journal and active-context checkpoints. An
//! [`AgentSession`](crate::session::AgentSession) remains the in-memory active
//! context.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

mod artifact;
mod dto;
mod error;
mod hash;
mod model;
pub mod sqlite;

pub(crate) use hash::active_messages_sha256;

pub use artifact::{CommittedArtifact, NewToolArtifact, ToolArtifactRef};
pub use error::SessionStoreError;
pub use model::{
    Checkpoint, CommittedCompaction, CompactionEvent, CompactionMetadata, CompactionPassKind,
    CompactionReason, JournalEntry, JournalKind, JournalSnapshot, NewCheckpoint,
    NewCompactionCommit, NewJournalEntry, NewSession, Seq, SessionId,
};

/// Persists the complete session journal and manages active-context checkpoints.
///
/// Implementations return a sequence or receipt only after the corresponding facts
/// are durably visible, allowing callers to advance the in-memory frontier safely.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Creates an isolated durable session.
    ///
    /// # Errors
    /// Returns the underlying storage error when session metadata cannot be written.
    async fn create_session(&self, session: NewSession) -> Result<SessionId, SessionStoreError>;

    /// Appends an immutable fact to the specified session journal.
    ///
    /// # Errors
    /// Returns a storage error when the payload cannot be serialized or committed.
    async fn append(
        &self,
        session: &SessionId,
        entry: NewJournalEntry,
    ) -> Result<Seq, SessionStoreError>;

    /// Persists a complete tool result only while the journal remains at the
    /// caller-audited head.
    ///
    /// # Errors
    /// Returns [`SessionStoreError::JournalHeadConflict`] when another fact was
    /// committed after the caller audited the session. Also returns an error when
    /// the artifact conflicts with durable content, lacks its journal fact, or
    /// cannot be committed.
    async fn put_tool_artifact(
        &self,
        session: &SessionId,
        expected_journal_head: Seq,
        artifact: NewToolArtifact,
    ) -> Result<CommittedArtifact, SessionStoreError>;

    /// Reads the latest committed active-context checkpoint for a session.
    ///
    /// # Errors
    /// Returns a storage error when the query fails or stored payload cannot be parsed.
    async fn latest_checkpoint(
        &self,
        session: &SessionId,
    ) -> Result<Option<Checkpoint>, SessionStoreError>;

    /// Writes a checkpoint and appends its journal reference in one atomic commit.
    ///
    /// # Errors
    /// Returns an error when checkpoint coverage is invalid or serialization or the
    /// transaction fails.
    async fn write_checkpoint(&self, checkpoint: NewCheckpoint) -> Result<Seq, SessionStoreError>;

    /// Commits a compaction event and checkpoint atomically using the journal head as CAS.
    ///
    /// The `compaction` journal entry, checkpoint, and `checkpoint_ref` journal entry
    /// are all durable before success is returned; failure leaves no partially visible
    /// commit.
    ///
    /// # Errors
    /// Returns [`SessionStoreError::JournalHeadConflict`] for a stale expected head,
    /// [`SessionStoreError::InvalidCompaction`] or
    /// [`SessionStoreError::InvalidCheckpoint`] for invariant violations, and the
    /// corresponding storage error when the transaction fails.
    async fn commit_compaction(
        &self,
        commit: NewCompactionCommit,
    ) -> Result<CommittedCompaction, SessionStoreError>;

    /// Replays every journal fact after the given position in ascending sequence order.
    ///
    /// # Errors
    /// Returns a storage error when the query fails or any payload cannot be parsed.
    async fn replay_after(
        &self,
        session: &SessionId,
        seq: Seq,
    ) -> Result<Vec<JournalEntry>, SessionStoreError>;

    /// Reads the current head and selected facts from one consistent observation.
    ///
    /// Implementations should override the default full-replay fallback with a
    /// bounded query when exact sequence lookup is available. The head and entries
    /// must not come from separate reads; returned entries must be unique, restricted
    /// to `seqs`, and ordered by sequence. Requested sequences that do not exist are
    /// omitted so the consumer can treat absence as an integrity failure where needed.
    ///
    /// # Errors
    /// Returns a storage error when the snapshot cannot be read or decoded.
    async fn journal_snapshot(
        &self,
        session: &SessionId,
        seqs: &[Seq],
    ) -> Result<JournalSnapshot, SessionStoreError> {
        let requested = seqs
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let mut all = self.replay_after(session, Seq::ZERO).await?;
        let head = all.iter().map(|entry| entry.seq).max().unwrap_or(Seq::ZERO);
        all.retain(|entry| requested.contains(&entry.seq));
        all.sort_by_key(|entry| entry.seq);
        Ok(JournalSnapshot::new(head, all))
    }
}

/// Returns the fixed SQLite session-store path under the user's home directory.
pub fn session_store_path(home: &Path) -> PathBuf {
    home.join(".kuncode")
        .join("sessions")
        .join("session-store.sqlite3")
}

/// Encodes a project root as a stable, filename-safe session grouping identifier.
pub fn project_slug(root: &Path) -> String {
    root.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect()
}
