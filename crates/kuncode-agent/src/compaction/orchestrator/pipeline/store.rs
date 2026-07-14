//! Adapts the session store to artifact-pass persistence operations.
//!
//! Delegation preserves the store's snapshot and compare-and-swap contracts;
//! the adapter must not reconstruct journal authority from separate reads.

use async_trait::async_trait;

use crate::{
    compaction::artifact::ArtifactStore,
    session_store::{
        CommittedArtifact, JournalEntry, JournalSnapshot, NewToolArtifact, Seq, SessionId,
        SessionStore, SessionStoreError,
    },
};

pub(super) struct SessionArtifactStore<'a>(pub(super) &'a dyn SessionStore);

#[async_trait]
impl ArtifactStore for SessionArtifactStore<'_> {
    async fn replay(
        &self,
        session: &SessionId,
        seq: Seq,
    ) -> Result<Vec<JournalEntry>, SessionStoreError> {
        self.0.replay_after(session, seq).await
    }

    async fn journal_snapshot(
        &self,
        session: &SessionId,
        seqs: &[Seq],
    ) -> Result<JournalSnapshot, SessionStoreError> {
        self.0.journal_snapshot(session, seqs).await
    }

    async fn put(
        &self,
        session: &SessionId,
        expected_journal_head: Seq,
        artifact: NewToolArtifact,
    ) -> Result<CommittedArtifact, SessionStoreError> {
        self.0
            .put_tool_artifact(session, expected_journal_head, artifact)
            .await
    }
}
