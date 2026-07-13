use super::support::{FixedCounter, tool_exchange};
use crate::{
    compaction::{
        artifact::{ArtifactSpillError, ArtifactSpillInput, spill_artifacts},
        protocol::{group_messages, select_protected_recent_tail},
    },
    session::AgentSession,
    session_store::{
        JournalKind, NewJournalEntry, NewSession, Seq, SessionStore, sqlite::SqliteSessionStore,
    },
    test_support::TestDir,
};

#[tokio::test]
async fn rejects_frontier_when_only_assistant_is_durable() {
    // Given: active context has a closed exchange but SQLite only has its assistant fact.
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
    let frontier = store
        .append(
            &session_id,
            NewJournalEntry::message(&messages[0]).expect("assistant should encode"),
        )
        .await
        .expect("assistant should commit");
    let session = attached_session(&messages, session_id, frontier);
    let groups = group_messages(session.messages()).expect("active context should be closed");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1).expect("tail should exist");

    // When: spill audits the claimed durable context before writing an artifact.
    let input = ArtifactSpillInput::new(&groups, &protected, &session)
        .expect("session supplies a durable context");
    let result = spill_artifacts(input, &store, &FixedCounter::new(9_000, 100)).await;

    // Then: missing journal messages are fatal and no artifact receipt is produced.
    assert!(matches!(
        result,
        Err(ArtifactSpillError::JournalMessageCountMismatch { .. })
    ));
}

#[tokio::test]
async fn rejects_any_journal_fact_beyond_session_frontier() {
    // Given: complete durable messages followed by a newer non-message journal fact.
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
    let frontier = persist_messages(&store, &session_id, &messages).await;
    let session = attached_session(&messages, session_id.clone(), frontier);
    let newer = store
        .append(
            &session_id,
            NewJournalEntry::raw(
                JournalKind::SessionNote,
                serde_json::json!({ "note": "newer fact" }),
            ),
        )
        .await
        .expect("newer fact should commit");
    let groups = group_messages(session.messages()).expect("active context should be closed");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1).expect("tail should exist");
    let input = ArtifactSpillInput::new(&groups, &protected, &session)
        .expect("session supplies its acknowledged frontier");

    // When: spill replays all facts before attempting an artifact write.
    let result = spill_artifacts(input, &store, &FixedCounter::new(9_000, 100)).await;

    // Then: the first newer fact invalidates the entire candidate.
    assert_eq!(
        result,
        Err(ArtifactSpillError::JournalFrontierStale {
            frontier: frontier.get(),
            actual: newer.get(),
        })
    );
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

#[tokio::test]
async fn rejects_active_message_that_differs_from_durable_journal() {
    // Given: SQLite has a complete but different exchange at the claimed frontier.
    let root = TestDir::new();
    let store = SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
        .await
        .expect("store should open");
    let session_id = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let durable = [
        tool_exchange("old", "bash", "durable payload"),
        tool_exchange("recent", "read_file", "recent"),
    ]
    .concat();
    let active = [
        tool_exchange("old", "bash", "changed payload"),
        tool_exchange("recent", "read_file", "recent"),
    ]
    .concat();
    let frontier = persist_messages(&store, &session_id, &durable).await;
    let session = attached_session(&active, session_id, frontier);
    let groups = group_messages(session.messages()).expect("active context should be closed");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1).expect("tail should exist");

    // When: spill compares active messages with authoritative journal facts.
    let input = ArtifactSpillInput::new(&groups, &protected, &session)
        .expect("session supplies a durable context");
    let result = spill_artifacts(input, &store, &FixedCounter::new(9_000, 100)).await;

    // Then: the first mismatched fact rejects the entire pass.
    assert!(matches!(
        result,
        Err(ArtifactSpillError::JournalMessageMismatch { .. })
    ));
}

#[tokio::test]
async fn rejects_phantom_session_frontier() {
    // Given: active messages match SQLite but the session claims a nonexistent head.
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
    let actual = persist_messages(&store, &session_id, &messages).await;
    let mut session = attached_session(&messages, session_id, actual);
    let phantom = Seq::new(actual.get() + 10);
    session.advance_durable_seq(phantom);
    let groups = group_messages(session.messages()).expect("active context should be closed");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1).expect("tail should exist");
    let input = ArtifactSpillInput::new(&groups, &protected, &session)
        .expect("active context should bind to the session");

    // When: audit compares the observed journal head with the claimed frontier.
    let result = spill_artifacts(input, &store, &FixedCounter::new(9_000, 100)).await;

    // Then: the phantom frontier is fatal before any artifact write.
    assert_eq!(
        result,
        Err(ArtifactSpillError::JournalFrontierStale {
            frontier: phantom.get(),
            actual: actual.get(),
        })
    );
}

fn attached_session(
    messages: &[kuncode_core::completion::Message],
    session_id: crate::session_store::SessionId,
    frontier: crate::session_store::Seq,
) -> AgentSession {
    let mut session = AgentSession::from_messages(messages.to_vec());
    session.attach_session_id(session_id);
    session.advance_durable_seq(frontier);
    session
}

async fn persist_messages(
    store: &SqliteSessionStore,
    session: &crate::session_store::SessionId,
    messages: &[kuncode_core::completion::Message],
) -> crate::session_store::Seq {
    let mut frontier = crate::session_store::Seq::ZERO;
    for message in messages {
        frontier = store
            .append(
                session,
                NewJournalEntry::message(message).expect("message should encode"),
            )
            .await
            .expect("message should commit");
    }
    frontier
}
