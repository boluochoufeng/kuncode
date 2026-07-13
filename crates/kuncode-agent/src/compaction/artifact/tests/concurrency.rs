use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;

use super::support::{FixedCounter, persisted_session, tool_exchange};
use crate::{
    compaction::{
        artifact::{ArtifactSpillError, ArtifactSpillInput, ArtifactStore, spill_artifacts},
        protocol::{group_messages, select_protected_recent_tail},
    },
    session_store::{
        Checkpoint, CommittedArtifact, JournalEntry, JournalKind, NewJournalEntry, NewSession,
        NewToolArtifact, Seq, SessionId, SessionStore, SessionStoreError,
        sqlite::SqliteSessionStore,
    },
    test_support::TestDir,
};

struct InterleavingStore<'a> {
    inner: &'a SqliteSessionStore,
    interleaved: AtomicBool,
}

#[async_trait]
impl ArtifactStore for InterleavingStore<'_> {
    async fn latest_checkpoint(
        &self,
        session: &SessionId,
    ) -> Result<Option<Checkpoint>, SessionStoreError> {
        SessionStore::latest_checkpoint(self.inner, session).await
    }

    async fn replay(
        &self,
        session: &SessionId,
        after: Seq,
    ) -> Result<Vec<JournalEntry>, SessionStoreError> {
        self.inner.replay_after(session, after).await
    }

    async fn put(
        &self,
        session: &SessionId,
        expected_journal_head: Seq,
        artifact: NewToolArtifact,
    ) -> Result<CommittedArtifact, SessionStoreError> {
        if !self.interleaved.swap(true, Ordering::SeqCst) {
            self.inner
                .append(
                    session,
                    NewJournalEntry::raw(
                        JournalKind::SessionNote,
                        serde_json::json!({ "note": "interleaved" }),
                    ),
                )
                .await?;
        }
        self.inner
            .put_tool_artifact(session, expected_journal_head, artifact)
            .await
    }
}

#[tokio::test]
async fn concurrent_journal_append_aborts_the_entire_spill_pass() {
    // Given: journal audit succeeds before a store wrapper appends a competing fact.
    let root = TestDir::new();
    let store = SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
        .await
        .expect("store should open");
    let session_id = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let messages = [
        tool_exchange("old", "bash", "payload"),
        tool_exchange("recent", "read_file", "recent"),
    ]
    .concat();
    let session = persisted_session(&store, session_id.clone(), &messages).await;
    let expected = session.durable_seq().expect("session should be durable");
    let groups = group_messages(session.messages()).expect("active context should be closed");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1).expect("tail should exist");
    let wrapper = InterleavingStore {
        inner: &store,
        interleaved: AtomicBool::new(false),
    };

    // When: the first artifact transaction compares its stale expected head.
    let input = ArtifactSpillInput::new(&groups, &protected, &session)
        .expect("active context should bind to the session");
    let result = spill_artifacts(input, &wrapper, &FixedCounter::new(9_000, 100)).await;

    // Then: the pass returns a fatal conflict and persists no artifact fact.
    assert!(matches!(
        result,
        Err(ArtifactSpillError::JournalHeadConflict { expected: value, actual })
            if value == expected.get() && actual == expected.get() + 1
    ));
    let entries = store
        .replay_after(&session_id, Seq::ZERO)
        .await
        .expect("journal should replay");
    assert!(
        entries
            .iter()
            .all(|entry| entry.kind != JournalKind::ToolArtifact.as_str())
    );
}
