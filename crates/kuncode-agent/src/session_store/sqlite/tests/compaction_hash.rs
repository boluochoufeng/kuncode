use kuncode_core::completion::Message;

use crate::{
    session_store::{
        CompactionEvent, CompactionMetadata, CompactionPassKind, CompactionReason, NewCheckpoint,
        NewCompactionCommit, NewJournalEntry, NewSession, SessionStore, SessionStoreError,
        active_messages_sha256, sqlite::SqliteSessionStore,
    },
    test_support::TestDir,
};

const VALID_INPUT_HASH: &str = "6a92d9a0755b29fe5c408e21938f4870c3401e539dfc91f4cc3f1dd59d8592b7";

#[tokio::test]
async fn compaction_rejects_non_canonical_sha256_hashes() {
    let (store, session, source) = store_with_source().await;
    let checkpoint = checkpoint(&session, source, "summary");
    let valid_output = active_messages_sha256(&checkpoint.active_messages).expect("output hash");

    for (input_hash, output_hash) in [
        ("short", valid_output.as_str()),
        (
            VALID_INPUT_HASH,
            "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789",
        ),
    ] {
        let result = store
            .commit_compaction(NewCompactionCommit {
                session_id: session.clone(),
                expected_journal_head: source,
                event: event(input_hash, output_hash, source),
                checkpoint: checkpoint.clone(),
            })
            .await;

        assert!(matches!(
            result,
            Err(SessionStoreError::InvalidCompaction(_))
        ));
    }
}

#[tokio::test]
async fn compaction_rejects_output_hash_for_different_active_messages() {
    let (store, session, source) = store_with_source().await;
    let checkpoint = checkpoint(&session, source, "installed summary");
    let other_messages = vec![Message::user("different summary")];
    let wrong_output = active_messages_sha256(&other_messages).expect("output hash");

    let result = store
        .commit_compaction(NewCompactionCommit {
            session_id: session.clone(),
            expected_journal_head: source,
            event: event(VALID_INPUT_HASH, wrong_output, source),
            checkpoint,
        })
        .await;
    let entries = store
        .replay_after(&session, source)
        .await
        .expect("journal should replay");
    let latest = store
        .latest_checkpoint(&session)
        .await
        .expect("checkpoint lookup should succeed");

    assert!(matches!(
        result,
        Err(SessionStoreError::InvalidCompaction(_))
    ));
    assert!(entries.is_empty());
    assert!(latest.is_none());
}

fn event(
    input_hash: &str,
    output_hash: impl Into<String>,
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

async fn store_with_source() -> (
    SqliteSessionStore,
    crate::session_store::SessionId,
    crate::session_store::Seq,
) {
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
    (store, session, source)
}

fn checkpoint(
    session: &crate::session_store::SessionId,
    source: crate::session_store::Seq,
    text: &str,
) -> NewCheckpoint {
    NewCheckpoint {
        session_id: session.clone(),
        covers_through_seq: source,
        source_seq_start: Some(source),
        source_seq_end: Some(source),
        active_messages: vec![Message::user(text)],
        summary_json: Some(serde_json::json!({"schema_version": 1})),
        model: Some("test-model".to_string()),
        token_usage_json: Some(serde_json::json!({"input_tokens": 10, "total_tokens": 20})),
    }
}
