use async_trait::async_trait;
use sha2::{Digest, Sha256};

use super::support::{FixedCounter, persisted_session, tool_exchange};
use crate::{
    compaction::{
        artifact::{ArtifactSpillError, ArtifactSpillInput, ArtifactStore, spill_artifacts},
        protocol::{group_messages, select_protected_recent_tail},
    },
    session_store::{
        CommittedArtifact, JournalEntry, NewSession, NewToolArtifact, Seq, SessionId, SessionStore,
        SessionStoreError, sqlite::SqliteSessionStore,
    },
    test_support::TestDir,
};

enum WrongReceipt {
    OtherSession(SessionId),
    OtherArtifact,
}

struct WrongReceiptStore<'a> {
    inner: &'a SqliteSessionStore,
    wrong: WrongReceipt,
}

#[async_trait]
impl ArtifactStore for WrongReceiptStore<'_> {
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
        match &self.wrong {
            WrongReceipt::OtherSession(other) => {
                self.inner
                    .put_tool_artifact(other, Seq::ZERO, artifact)
                    .await
            }
            WrongReceipt::OtherArtifact => {
                let payload = "different durable payload";
                let hash = format!("sha256-{:x}", Sha256::digest(payload.as_bytes()));
                let other = NewToolArtifact::inline(hash, "different preview", payload)?;
                self.inner
                    .put_tool_artifact(session, expected_journal_head, other)
                    .await
            }
        }
    }
}

#[tokio::test]
async fn rejects_receipt_from_another_session_before_replacing_the_result() {
    let fixture = receipt_fixture().await;
    let other_session = fixture
        .store
        .create_session(NewSession::new(fixture.root.path().to_path_buf()))
        .await
        .expect("other session should be created");
    let wrapper = WrongReceiptStore {
        inner: &fixture.store,
        wrong: WrongReceipt::OtherSession(other_session),
    };

    let result = spill_artifacts(fixture.input(), &wrapper, &FixedCounter::new(9_000, 100)).await;

    assert_eq!(result, Err(ArtifactSpillError::ReceiptMismatch));
}

#[tokio::test]
async fn rejects_receipt_for_another_artifact_before_replacing_the_result() {
    let fixture = receipt_fixture().await;
    let wrapper = WrongReceiptStore {
        inner: &fixture.store,
        wrong: WrongReceipt::OtherArtifact,
    };

    let result = spill_artifacts(fixture.input(), &wrapper, &FixedCounter::new(9_000, 100)).await;

    assert_eq!(result, Err(ArtifactSpillError::ReceiptMismatch));
}

struct ReceiptFixture {
    root: TestDir,
    store: SqliteSessionStore,
    session: crate::session::AgentSession,
    groups: Vec<crate::compaction::protocol::ProtocolGroup>,
    protected: crate::compaction::protocol::ProtectedRecentTail,
}

impl ReceiptFixture {
    fn input(&self) -> ArtifactSpillInput<'_> {
        ArtifactSpillInput::new(&self.groups, &self.protected, &self.session)
            .expect("fixture should authorize artifact spill")
    }
}

async fn receipt_fixture() -> ReceiptFixture {
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
    let session = persisted_session(&store, session_id, &messages).await;
    let groups = group_messages(session.messages()).expect("fixture protocol should close");
    let protected =
        select_protected_recent_tail(&groups, 0, |_| 1).expect("fixture should have a tail");
    ReceiptFixture {
        root,
        store,
        session,
        groups,
        protected,
    }
}
