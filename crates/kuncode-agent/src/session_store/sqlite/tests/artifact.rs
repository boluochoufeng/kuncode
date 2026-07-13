use sqlx::Row;

use crate::{
    session_store::{
        JournalKind, NewSession, NewToolArtifact, Seq, SessionStore, sqlite::SqliteSessionStore,
    },
    test_support::TestDir,
};

#[tokio::test]
async fn put_tool_artifact_is_idempotent_and_journaled_once() {
    let root = TestDir::new();
    let store = SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
        .await
        .expect("store should open");
    let session = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let artifact = NewToolArtifact::inline("sha256-deadbeef", "preview text", "large payload text")
        .expect("artifact should be valid");

    let first = store
        .put_tool_artifact(&session, artifact.clone())
        .await
        .expect("first artifact write should commit");
    let second = store
        .put_tool_artifact(&session, artifact)
        .await
        .expect("duplicate artifact write should reuse the existing record");
    let entries = store
        .replay_after(&session, Seq::ZERO)
        .await
        .expect("journal should replay");
    let artifact_entries: Vec<_> = entries
        .iter()
        .filter(|entry| entry.kind == JournalKind::ToolArtifact.as_str())
        .collect();

    assert_eq!(first, second);
    assert_eq!(first.journal_seq(), artifact_entries[0].seq);
    assert_eq!(
        first.reference().artifact_id(),
        "tool-result-sha256-deadbeef"
    );
    assert_eq!(first.reference().content_hash(), "sha256-deadbeef");
    assert_eq!(first.reference().bytes(), 18);
    assert_eq!(artifact_entries.len(), 1);
    assert_eq!(
        artifact_entries[0].payload_json["artifact_id"],
        first.reference().artifact_id()
    );
}

#[tokio::test]
async fn put_tool_artifact_rejects_existing_id_with_different_content() {
    // Given: a durable artifact and its original receipt.
    let root = TestDir::new();
    let store = SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
        .await
        .expect("store should open");
    let session = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let original = NewToolArtifact::inline("caller-hash", "original preview", "original payload")
        .expect("artifact should be valid");
    let first = store
        .put_tool_artifact(&session, original.clone())
        .await
        .expect("first artifact write should commit");

    // When: the caller reuses that identifier for different durable content.
    let conflict = store
        .put_tool_artifact(
            &session,
            NewToolArtifact::inline("caller-hash", "changed preview", "changed payload")
                .expect("conflicting artifact should be structurally valid"),
        )
        .await;

    // Then: the conflict is rejected without changing the durable row or journal receipt.
    assert!(matches!(
        &conflict,
        Err(crate::session_store::SessionStoreError::ToolArtifactConflict {
            session_id,
            artifact_id,
        }) if session_id == session.as_str() && artifact_id == first.reference().artifact_id()
    ));
    let error_text = conflict
        .expect_err("different durable content must be rejected")
        .to_string();
    let repeated = store
        .put_tool_artifact(&session, original)
        .await
        .expect("unchanged duplicate should retain its original receipt");
    let row = sqlx::query(
        r#"
        SELECT content_hash, bytes, preview, payload_text, storage_ref
        FROM tool_artifacts
        WHERE session_id = ? AND artifact_id = ?
        "#,
    )
    .bind(session.as_str())
    .bind(first.reference().artifact_id())
    .fetch_one(&store.pool)
    .await
    .expect("durable artifact should remain queryable");
    let journal_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM journal_entries WHERE session_id = ? AND kind = ?",
    )
    .bind(session.as_str())
    .bind(JournalKind::ToolArtifact.as_str())
    .fetch_one(&store.pool)
    .await
    .expect("journal count should be queryable");

    assert_eq!(repeated, first);
    assert_eq!(row.get::<String, _>("content_hash"), "caller-hash");
    assert_eq!(row.get::<i64, _>("bytes"), 16);
    assert_eq!(row.get::<String, _>("preview"), "original preview");
    assert_eq!(row.get::<String, _>("payload_text"), "original payload");
    assert_eq!(row.get::<Option<String>, _>("storage_ref"), None);
    assert_eq!(journal_count, 1);
    assert!(!error_text.contains("original payload"));
    assert!(!error_text.contains("changed payload"));
}

#[tokio::test]
async fn distinct_artifacts_receive_monotonic_journal_receipts() {
    let root = TestDir::new();
    let store = SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
        .await
        .expect("store should open");
    let session = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");

    let first = store
        .put_tool_artifact(
            &session,
            NewToolArtifact::inline("sha256-first", "first", "first payload")
                .expect("first artifact should be valid"),
        )
        .await
        .expect("first artifact should commit");
    let second = store
        .put_tool_artifact(
            &session,
            NewToolArtifact::inline("sha256-second", "second", "second payload")
                .expect("second artifact should be valid"),
        )
        .await
        .expect("second artifact should commit");

    assert!(second.journal_seq() > first.journal_seq());
}

#[tokio::test]
async fn existing_artifact_without_journal_entry_is_rejected() {
    let root = TestDir::new();
    let store = SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
        .await
        .expect("store should open");
    let session = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let artifact = NewToolArtifact::inline("sha256-orphan", "preview", "payload")
        .expect("artifact should be valid");
    store
        .put_tool_artifact(&session, artifact.clone())
        .await
        .expect("artifact should commit");
    sqlx::query("DELETE FROM journal_entries WHERE session_id = ?")
        .bind(session.as_str())
        .execute(&store.pool)
        .await
        .expect("test fixture should remove journal entry");

    let result = store.put_tool_artifact(&session, artifact).await;

    assert!(matches!(
        result,
        Err(crate::session_store::SessionStoreError::ToolArtifactJournalMissing { .. })
    ));
}
