use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use super::support::{FixedCounter, persisted_session, tool_exchange};
use crate::{
    compaction::{
        artifact::{ArtifactSpillInput, ArtifactSpillOutcome, spill_artifacts},
        protocol::{group_messages, select_protected_recent_tail},
    },
    session_store::{
        Checkpoint, CommittedArtifact, CommittedCompaction, JournalEntry, NewCheckpoint,
        NewCompactionCommit, NewJournalEntry, NewSession, NewToolArtifact, Seq, SessionId,
        SessionStore, SessionStoreError, turso::TursoSessionStore,
    },
    test_support::TestDir,
};

struct CheckpointReadSpy<'a> {
    inner: &'a TursoSessionStore,
    checkpoint_reads: AtomicUsize,
}

impl CheckpointReadSpy<'_> {
    fn checkpoint_reads(&self) -> usize {
        self.checkpoint_reads.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SessionStore for CheckpointReadSpy<'_> {
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
        self.checkpoint_reads.fetch_add(1, Ordering::SeqCst);
        self.inner.latest_checkpoint(session).await
    }

    async fn write_checkpoint(&self, checkpoint: NewCheckpoint) -> Result<Seq, SessionStoreError> {
        self.inner.write_checkpoint(checkpoint).await
    }

    async fn commit_compaction(
        &self,
        commit: NewCompactionCommit,
    ) -> Result<CommittedCompaction, SessionStoreError> {
        self.inner.commit_compaction(commit).await
    }

    async fn replay_after(
        &self,
        session: &SessionId,
        seq: Seq,
    ) -> Result<Vec<JournalEntry>, SessionStoreError> {
        self.inner.replay_after(session, seq).await
    }
}

#[tokio::test]
async fn spill_audits_live_lineage_without_reading_checkpoint() {
    // Given: a live durable session whose store also contains a checkpoint.
    let root = TestDir::new();
    let store = TursoSessionStore::open(root.path().join("sessions.db"))
        .await
        .expect("store should open");
    let session_id = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let messages = [
        tool_exchange("old", "bash", "old payload"),
        tool_exchange("recent", "read_file", "recent payload"),
    ]
    .concat();
    let mut session = persisted_session(&store, session_id.clone(), &messages).await;
    let message_head = session.durable_seq().expect("session should be durable");
    let checkpoint_head = store
        .write_checkpoint(NewCheckpoint {
            session_id,
            covers_through_seq: message_head,
            source_seq_start: None,
            source_seq_end: None,
            active_messages: messages.clone(),
            summary_json: None,
            model: None,
            token_usage_json: None,
        })
        .await
        .expect("checkpoint should commit");
    session.advance_durable_seq(checkpoint_head);
    let groups = group_messages(session.messages()).expect("active context should be closed");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1).expect("tail should exist");
    let spy = CheckpointReadSpy {
        inner: &store,
        checkpoint_reads: AtomicUsize::new(0),
    };

    // When: the automatic spill path audits the original journal fact.
    let input = ArtifactSpillInput::new(&groups, &protected, &session)
        .expect("live session should authorize the pass");
    let result = spill_artifacts(input, &spy, &FixedCounter::new(9_000, 100))
        .await
        .expect("live journal audit should pass");

    // Then: the old result spills without consulting the resumable checkpoint view.
    assert_eq!(spy.checkpoint_reads(), 0);
    assert!(matches!(
        result.outcomes(),
        [ArtifactSpillOutcome::Spilled { .. }]
    ));
    assert_ne!(result.groups()[0], groups[0]);
    assert_eq!(result.groups()[1], groups[1]);
}
