use std::sync::Arc;

use async_trait::async_trait;

use crate::session_store::{
    Checkpoint, CommittedArtifact, CommittedCompaction, JournalEntry, NewCheckpoint,
    NewCompactionCommit, NewJournalEntry, NewSession, NewToolArtifact, Seq, SessionId,
    SessionStore, SessionStoreError, turso::TursoSessionStore,
};

pub(crate) struct UnknownCommitStore {
    inner: Arc<TursoSessionStore>,
}

pub(crate) struct RejectedReceiptStore {
    inner: Arc<TursoSessionStore>,
}

impl RejectedReceiptStore {
    pub(crate) fn new(inner: Arc<TursoSessionStore>) -> Self {
        Self { inner }
    }
}

impl UnknownCommitStore {
    pub(crate) fn new(inner: Arc<TursoSessionStore>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl SessionStore for UnknownCommitStore {
    async fn create_session(&self, session: NewSession) -> Result<SessionId, SessionStoreError> {
        self.inner.create_session(session).await
    }

    async fn append(
        &self,
        session: &SessionId,
        entry: NewJournalEntry,
    ) -> Result<Seq, SessionStoreError> {
        self.inner.append(session, entry).await
    }

    async fn put_tool_artifact(
        &self,
        session: &SessionId,
        expected_journal_head: Seq,
        artifact: NewToolArtifact,
    ) -> Result<CommittedArtifact, SessionStoreError> {
        self.inner
            .put_tool_artifact(session, expected_journal_head, artifact)
            .await
    }

    async fn latest_checkpoint(
        &self,
        session: &SessionId,
    ) -> Result<Option<Checkpoint>, SessionStoreError> {
        self.inner.latest_checkpoint(session).await
    }

    async fn write_checkpoint(&self, checkpoint: NewCheckpoint) -> Result<Seq, SessionStoreError> {
        self.inner.write_checkpoint(checkpoint).await
    }

    async fn commit_compaction(
        &self,
        _commit: NewCompactionCommit,
    ) -> Result<CommittedCompaction, SessionStoreError> {
        Err(SessionStoreError::CommitOutcomeUnknown {
            operation: "compaction",
            message: "injected ambiguous commit".to_string(),
        })
    }

    async fn replay_after(
        &self,
        session: &SessionId,
        seq: Seq,
    ) -> Result<Vec<JournalEntry>, SessionStoreError> {
        self.inner.replay_after(session, seq).await
    }
}

#[async_trait]
impl SessionStore for RejectedReceiptStore {
    async fn create_session(&self, session: NewSession) -> Result<SessionId, SessionStoreError> {
        self.inner.create_session(session).await
    }

    async fn append(
        &self,
        session: &SessionId,
        entry: NewJournalEntry,
    ) -> Result<Seq, SessionStoreError> {
        self.inner.append(session, entry).await
    }

    async fn put_tool_artifact(
        &self,
        session: &SessionId,
        expected_journal_head: Seq,
        artifact: NewToolArtifact,
    ) -> Result<CommittedArtifact, SessionStoreError> {
        self.inner
            .put_tool_artifact(session, expected_journal_head, artifact)
            .await
    }

    async fn latest_checkpoint(
        &self,
        session: &SessionId,
    ) -> Result<Option<Checkpoint>, SessionStoreError> {
        self.inner.latest_checkpoint(session).await
    }

    async fn write_checkpoint(&self, checkpoint: NewCheckpoint) -> Result<Seq, SessionStoreError> {
        self.inner.write_checkpoint(checkpoint).await
    }

    async fn commit_compaction(
        &self,
        commit: NewCompactionCommit,
    ) -> Result<CommittedCompaction, SessionStoreError> {
        let committed = self.inner.commit_compaction(commit).await?;
        Ok(CommittedCompaction::new(
            committed.session_id().clone(),
            committed.compaction_seq(),
            committed.checkpoint_seq(),
            "0".repeat(64),
        ))
    }

    async fn replay_after(
        &self,
        session: &SessionId,
        seq: Seq,
    ) -> Result<Vec<JournalEntry>, SessionStoreError> {
        self.inner.replay_after(session, seq).await
    }
}
