use crate::{
    session_store::{
        JournalKind, NewCheckpoint, NewJournalEntry, NewSession, SessionStore,
        sqlite::SqliteSessionStore,
    },
    test_support::TestDir,
};
use kuncode_core::completion::Message;

#[tokio::test]
async fn latest_checkpoint_then_replay_after_rebuilds_active_context() {
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

    let checkpoint_seq = store
        .write_checkpoint(NewCheckpoint {
            session_id: session.clone(),
            covers_through_seq: second,
            source_seq_start: Some(first),
            source_seq_end: Some(second),
            active_messages: vec![Message::user("summary-through-two")],
            summary_json: None,
            model: Some("deepseek-v4-flash".to_string()),
            token_usage_json: Some(serde_json::json!({ "input_tokens": 10 })),
        })
        .await
        .expect("checkpoint should commit");
    let third = store
        .append(
            &session,
            NewJournalEntry::message(&Message::user("three")).expect("message"),
        )
        .await
        .expect("third append should commit");

    let checkpoint = store
        .latest_checkpoint(&session)
        .await
        .expect("checkpoint should load")
        .expect("checkpoint should exist");
    let tail = store
        .replay_after(&session, checkpoint.covers_through_seq)
        .await
        .expect("tail should replay");

    assert_eq!(checkpoint.checkpoint_seq, checkpoint_seq);
    assert_eq!(checkpoint.source_seq_start, Some(first));
    assert_eq!(checkpoint.source_seq_end, Some(second));
    let mut rebuilt = checkpoint.active_messages.clone();
    for entry in tail
        .iter()
        .filter(|entry| entry.kind == JournalKind::Message.as_str())
    {
        rebuilt.push(entry.clone().into_message().expect("message entry"));
    }

    assert_eq!(
        checkpoint.active_messages,
        vec![Message::user("summary-through-two")]
    );
    assert_eq!(checkpoint.model.as_deref(), Some("deepseek-v4-flash"));
    assert_eq!(
        checkpoint.token_usage_json,
        Some(serde_json::json!({ "input_tokens": 10 }))
    );
    assert!(!tail.iter().any(|entry| entry.seq == first));
    assert!(!tail.iter().any(|entry| entry.seq == second));
    assert!(tail.iter().any(|entry| entry.seq == checkpoint_seq));
    assert!(tail.iter().any(|entry| entry.seq == third));
    assert!(
        tail.iter()
            .any(|entry| entry.kind == JournalKind::CheckpointRef.as_str())
    );
    assert_eq!(
        rebuilt,
        vec![Message::user("summary-through-two"), Message::user("three")]
    );
}
