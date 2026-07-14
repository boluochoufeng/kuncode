use kuncode_core::completion::Message;

use crate::{
    session_store::{
        CompactionEvent, CompactionMetadata, CompactionPassKind, CompactionReason, JournalKind,
        NewCheckpoint, NewCompactionCommit, NewJournalEntry, NewSession, SessionStore,
        SessionStoreError, active_messages_sha256, sqlite::SqliteSessionStore,
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
    let checkpoint = checkpoint(&session, source);
    let output_hash = active_messages_sha256(&checkpoint.active_messages).expect("output hash");

    let event = semantic_event(
        "6a92d9a0755b29fe5c408e21938f4870c3401e539dfc91f4cc3f1dd59d8592b7",
        output_hash.clone(),
        source,
        &checkpoint,
    );
    let committed = store
        .commit_compaction(NewCompactionCommit {
            session_id: session.clone(),
            expected_journal_head: source,
            event,
            checkpoint,
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
    assert_eq!(entries[0].payload_json["schema_version"], 2);
    assert_eq!(
        entries[0].payload_json["input_hash"],
        "6a92d9a0755b29fe5c408e21938f4870c3401e539dfc91f4cc3f1dd59d8592b7"
    );
    assert_eq!(entries[0].payload_json["output_hash"], output_hash);
    assert_eq!(entries[0].payload_json["reason"], "soft_threshold");
    assert_eq!(
        entries[0].payload_json["passes"],
        serde_json::json!(["semantic_summary", "atomic_commit"])
    );
    assert_eq!(committed.output_hash(), output_hash);
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
    // Simulate an append that lands after the candidate binds to `expected`.
    store
        .append(
            &session,
            NewJournalEntry::message(&Message::user("racing append")).expect("message"),
        )
        .await
        .expect("racing message should commit");
    let checkpoint = checkpoint(&session, expected);
    let output_hash = active_messages_sha256(&checkpoint.active_messages).expect("output hash");

    let result = store
        .commit_compaction(NewCompactionCommit {
            session_id: session.clone(),
            expected_journal_head: expected,
            event: deterministic_event(
                "7990ba3e1d64d72ed4307f9d398ce981b021fd9fa5bd80d03d2d6ca93dce50f1",
                output_hash,
                expected,
            ),
            checkpoint,
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
    let output_hash = active_messages_sha256(&invalid.active_messages).expect("output hash");

    let result = store
        .commit_compaction(NewCompactionCommit {
            session_id: session.clone(),
            expected_journal_head: source,
            event: deterministic_event(
                "ac01a958a68fc9b085623a7232edc1a45c92b8a0d6cb5cd3ac7318f961e6a066",
                output_hash,
                source,
            ),
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
        token_usage_json: Some(serde_json::json!({"input_tokens": 10, "total_tokens": 20})),
    }
}

fn deterministic_event(
    input_hash: &str,
    output_hash: String,
    source: crate::session_store::Seq,
) -> CompactionEvent {
    CompactionEvent::new(
        input_hash,
        output_hash,
        source..=source,
        CompactionMetadata::new(
            CompactionReason::SoftThreshold,
            vec![CompactionPassKind::AtomicCommit],
        ),
    )
}

fn semantic_event(
    input_hash: &str,
    output_hash: String,
    source: crate::session_store::Seq,
    checkpoint: &NewCheckpoint,
) -> CompactionEvent {
    let metadata = CompactionMetadata::new(
        CompactionReason::SoftThreshold,
        vec![
            CompactionPassKind::SemanticSummary,
            CompactionPassKind::AtomicCommit,
        ],
    )
    .with_generated_summary(
        checkpoint.summary_json.clone().expect("summary fixture"),
        checkpoint.model.clone().expect("model fixture"),
        checkpoint.token_usage_json.clone().expect("usage fixture"),
    );
    CompactionEvent::new(input_hash, output_hash, source..=source, metadata)
}
