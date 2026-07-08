use crate::{
    session_store::{NewJournalEntry, NewSession, Seq, SessionStore, sqlite::SqliteSessionStore},
    test_support::TestDir,
};
use kuncode_core::completion::Message;

#[tokio::test]
async fn append_assigns_monotonic_seq_per_session() {
    let root = TestDir::new();
    let store = SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
        .await
        .expect("store should open");
    let session = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");

    let first = store
        .append(
            &session,
            NewJournalEntry::message(&Message::user("one")).expect("message"),
        )
        .await
        .expect("first append should commit");
    let second = store
        .append(
            &session,
            NewJournalEntry::message(&Message::user("two")).expect("message"),
        )
        .await
        .expect("second append should commit");

    assert_eq!(first, Seq::new(1));
    assert_eq!(second, Seq::new(2));
}

#[tokio::test]
async fn message_journal_uses_versioned_store_payload() {
    let root = TestDir::new();
    let store = SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
        .await
        .expect("store should open");
    let session = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");

    store
        .append(
            &session,
            NewJournalEntry::message(&Message::user("persist me")).expect("message"),
        )
        .await
        .expect("append should commit");

    let entries = store
        .replay_after(&session, Seq::ZERO)
        .await
        .expect("journal should replay");
    let payload = &entries[0].payload_json;

    assert_eq!(payload["schema_version"], 1);
    assert_eq!(payload["message"]["role"], "user");
    assert_eq!(payload["message"]["content"][0]["kind"], "text");
    assert_eq!(payload["message"]["content"][0]["text"], "persist me");
    assert!(payload.get("role").is_none());
}
