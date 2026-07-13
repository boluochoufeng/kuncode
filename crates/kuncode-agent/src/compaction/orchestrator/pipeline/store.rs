//! Adapts the session store to artifact-pass persistence operations.

use async_trait::async_trait;

use crate::{
    compaction::artifact::ArtifactStore,
    session_store::{
        CommittedArtifact, JournalEntry, NewToolArtifact, Seq, SessionId, SessionStore,
        SessionStoreError,
    },
};

pub(super) struct SessionArtifactStore<'a>(pub(super) &'a dyn SessionStore);

#[async_trait]
impl ArtifactStore for SessionArtifactStore<'_> {
    async fn replay(
        &self,
        session: &SessionId,
        after: Seq,
    ) -> Result<Vec<JournalEntry>, SessionStoreError> {
        self.0.replay_after(session, after).await
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
