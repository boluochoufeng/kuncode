use kuncode_core::completion::Message;

use crate::{
    session_store::{
        CompactionEvent, JournalKind, NewCheckpoint, NewCompactionCommit, NewJournalEntry,
        NewSession, SessionStore, SessionStoreError, sqlite::SqliteSessionStore,
    },
    test_support::TestDir,
};

#[tokio::test]
async fn commit_compaction_atomically_persists_event_checkpoint_and_ref() {
    let root = TestDir::new();
    let store = SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
        .await
        .expect("store should open");
    let session = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let source = store
        .append(
            &session,
            NewJournalEntry::message(&Message::user("original")).expect("message"),
        )
        .await
        .expect("message should commit");

    let committed = store
        .commit_compaction(NewCompactionCommit {
            session_id: session.clone(),
            expected_journal_head: source,
            event: CompactionEvent::new("sha256-input", source, source),
            checkpoint: checkpoint(&session, source),
        })
        .await
        .expect("compaction should commit");
    let entries = store
        .replay_after(&session, source)
        .await
        .expect("journal should replay");
    let latest = store
        .latest_checkpoint(&session)
        .await
        .expect("checkpoint lookup should succeed")
        .expect("checkpoint should exist");

    assert_eq!(committed.compaction_seq(), entries[0].seq);
    assert_eq!(committed.checkpoint_seq(), entries[1].seq);
    assert_eq!(committed.journal_head(), entries[1].seq);
    assert_eq!(entries[0].kind, JournalKind::Compaction.as_str());
    assert_eq!(entries[1].kind, JournalKind::CheckpointRef.as_str());
    assert_eq!(entries[0].payload_json["schema_version"], 1);
    assert_eq!(entries[0].payload_json["input_hash"], "sha256-input");
    assert_eq!(latest.checkpoint_seq, committed.checkpoint_seq());
}

#[tokio::test]
async fn stale_journal_head_rolls_back_every_compaction_record() {
    let root = TestDir::new();
    let store = SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
        .await
        .expect("store should open");
    let session = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let expected = store
        .append(
            &session,
            NewJournalEntry::message(&Message::user("one")).expect("message"),
        )
        .await
        .expect("first message should commit");
    store
        .append(
            &session,
            NewJournalEntry::message(&Message::user("racing append")).expect("message"),
        )
        .await
        .expect("racing message should commit");

    let result = store
        .commit_compaction(NewCompactionCommit {
            session_id: session.clone(),
            expected_journal_head: expected,
            event: CompactionEvent::new("sha256-stale", expected, expected),
            checkpoint: checkpoint(&session, expected),
        })
        .await;
    let entries = store
        .replay_after(&session, crate::session_store::Seq::ZERO)
        .await
        .expect("journal should replay");

    assert!(matches!(
        result,
        Err(SessionStoreError::JournalHeadConflict { .. })
    ));
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| entry.kind == JournalKind::Message.as_str())
    );
    assert!(
        store
            .latest_checkpoint(&session)
            .await
            .expect("lookup")
            .is_none()
    );
}

#[tokio::test]
async fn invalid_checkpoint_writes_no_compaction_records() {
    let root = TestDir::new();
    let store = SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
        .await
        .expect("store should open");
    let session = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let source = store
        .append(
            &session,
            NewJournalEntry::message(&Message::user("one")).expect("message"),
        )
        .await
        .expect("message should commit");
    let mut invalid = checkpoint(&session, source);
    invalid.covers_through_seq = crate::session_store::Seq::new(source.get() + 10);

    let result = store
        .commit_compaction(NewCompactionCommit {
            session_id: session.clone(),
            expected_journal_head: source,
            event: CompactionEvent::new("sha256-invalid", source, source),
            checkpoint: invalid,
        })
        .await;
    let entries = store
        .replay_after(&session, source)
        .await
        .expect("journal should replay");

    assert!(matches!(
        result,
        Err(SessionStoreError::InvalidCompaction(_))
    ));
    assert!(entries.is_empty());
    assert!(
        store
            .latest_checkpoint(&session)
            .await
            .expect("lookup")
            .is_none()
    );
}

fn checkpoint(
    session: &crate::session_store::SessionId,
    source: crate::session_store::Seq,
) -> NewCheckpoint {
    NewCheckpoint {
        session_id: session.clone(),
        covers_through_seq: source,
        source_seq_start: Some(source),
        source_seq_end: Some(source),
        active_messages: vec![Message::user("summary")],
        summary_json: Some(serde_json::json!({"schema_version": 1})),
        model: Some("test-model".to_string()),
        token_usage_json: None,
    }
}
