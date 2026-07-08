use crate::{
    session_store::{
        NewCheckpoint, NewJournalEntry, NewSession, Seq, SessionId, SessionStore,
        SessionStoreError, sqlite::SqliteSessionStore,
    },
    test_support::TestDir,
};
use kuncode_core::completion::Message;

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
            summary_json: None,
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
                summary_json: None,
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

async fn session_with_two_messages(root: &TestDir) -> (SqliteSessionStore, SessionId, Seq, Seq) {
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

    (store, session, first, second)
}

fn assert_invalid_checkpoint(result: Result<Seq, SessionStoreError>) {
    assert!(
        matches!(result, Err(SessionStoreError::InvalidCheckpoint(_))),
        "checkpoint should be rejected as invalid, got {result:?}"
    );
}

async fn assert_no_checkpoint(store: &SqliteSessionStore, session: &SessionId) {
    assert!(
        store
            .latest_checkpoint(session)
            .await
            .expect("checkpoint lookup should succeed")
            .is_none()
    );
}
