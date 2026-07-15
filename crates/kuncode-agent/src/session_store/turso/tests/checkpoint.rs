use crate::{
    session_store::{
        JournalKind, NewCheckpoint, NewJournalEntry, NewSession, Seq, SessionId, SessionStore,
        SessionStoreError, turso::TursoSessionStore,
    },
    test_support::TestDir,
};
use kuncode_core::completion::Message;

#[tokio::test]
async fn latest_checkpoint_then_replay_after_rebuilds_active_context() {
    let root = TestDir::new();
    let store = TursoSessionStore::open(root.path().join("sessions.db"))
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
            summary_json: Some(serde_json::json!({"schema_version": 1})),
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
    // The reference remains replayable journal history, while only message facts
    // after the covered range extend the checkpoint's active context.
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

#[tokio::test]
async fn write_checkpoint_rejects_future_covers_through_seq() {
    let root = TestDir::new();
    let (store, session, first, second) = session_with_two_messages(&root).await;

    let result = store
        .write_checkpoint(NewCheckpoint {
            session_id: session.clone(),
            covers_through_seq: Seq::new(second.get() + 1),
            source_seq_start: Some(first),
            source_seq_end: Some(second),
            active_messages: vec![Message::user("summary")],
            summary_json: Some(serde_json::json!({"schema_version": 1})),
            model: None,
            token_usage_json: None,
        })
        .await;

    assert_invalid_checkpoint(result);
    assert_no_checkpoint(&store, &session).await;
}

#[tokio::test]
async fn write_checkpoint_rejects_invalid_source_ranges() {
    let root = TestDir::new();
    let (store, session, first, second) = session_with_two_messages(&root).await;
    let cases = [
        ("missing_end", second, Some(first), None),
        ("missing_start", second, None, Some(second)),
        ("zero_start", second, Some(Seq::ZERO), Some(second)),
        ("negative_start", second, Some(Seq::new(-1)), Some(second)),
        ("reversed_range", second, Some(second), Some(first)),
        ("end_beyond_covered", first, Some(first), Some(second)),
    ];

    for (case, covers, start, end) in cases {
        let result = store
            .write_checkpoint(NewCheckpoint {
                session_id: session.clone(),
                covers_through_seq: covers,
                source_seq_start: start,
                source_seq_end: end,
                active_messages: vec![Message::user(format!("summary-{case}"))],
                summary_json: Some(serde_json::json!({"schema_version": 1})),
                model: None,
                token_usage_json: None,
            })
            .await;

        assert!(
            matches!(result, Err(SessionStoreError::InvalidCheckpoint(_))),
            "{case} should be rejected as an invalid checkpoint, got {result:?}"
        );
    }
    assert_no_checkpoint(&store, &session).await;
}

#[tokio::test]
async fn write_checkpoint_rejects_summary_provenance_without_summary() {
    let root = TestDir::new();
    let (store, session, first, second) = session_with_two_messages(&root).await;

    for (start, end, model, usage) in [
        (Some(first), Some(second), None, None),
        (None, None, Some("test-model".to_string()), None),
        (
            None,
            None,
            None,
            Some(serde_json::json!({"input_tokens": 1})),
        ),
    ] {
        let result = store
            .write_checkpoint(NewCheckpoint {
                session_id: session.clone(),
                covers_through_seq: second,
                source_seq_start: start,
                source_seq_end: end,
                active_messages: vec![Message::user("deterministic")],
                summary_json: None,
                model,
                token_usage_json: usage,
            })
            .await;

        assert_invalid_checkpoint(result);
    }
    assert_no_checkpoint(&store, &session).await;
}

async fn session_with_two_messages(root: &TestDir) -> (TursoSessionStore, SessionId, Seq, Seq) {
    let store = TursoSessionStore::open(root.path().join("sessions.db"))
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

    (store, session, first, second)
}

fn assert_invalid_checkpoint(result: Result<Seq, SessionStoreError>) {
    assert!(
        matches!(result, Err(SessionStoreError::InvalidCheckpoint(_))),
        "checkpoint should be rejected as invalid, got {result:?}"
    );
}

async fn assert_no_checkpoint(store: &TursoSessionStore, session: &SessionId) {
    assert!(
        store
            .latest_checkpoint(session)
            .await
            .expect("checkpoint lookup should succeed")
            .is_none()
    );
}
