use crate::{
    session_store::{
        NewSession, NewToolArtifact, Seq, SessionStore, SessionStoreError,
        sqlite::SqliteSessionStore,
    },
    test_support::TestDir,
};

#[tokio::test]
async fn put_tool_artifact_rejects_tampered_durable_payload_identity() {
    // Given: a valid artifact whose durable payload is later corrupted.
    let root = TestDir::new();
    let store = SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
        .await
        .expect("store should open");
    let session = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let hash = "sha256-239f59ed55e737c77147cf55ad0c1b030b6d7ee748a7426952f9b852d5a935e5";
    let artifact =
        NewToolArtifact::inline(hash, "preview", "payload").expect("artifact should be valid");
    let receipt = store
        .put_tool_artifact(&session, Seq::ZERO, artifact.clone())
        .await
        .expect("artifact should commit");
    sqlx::query(
        "UPDATE tool_artifacts SET payload_text = ? WHERE session_id = ? AND artifact_id = ?",
    )
    .bind("forged payload")
    .bind(session.as_str())
    .bind(receipt.reference().artifact_id())
    .execute(&store.pool)
    .await
    .expect("fixture should tamper with the durable payload");

    // When: an idempotent write reads the corrupted durable identity.
    let result = store
        .put_tool_artifact(&session, receipt.journal_seq(), artifact)
        .await;

    // Then: SQLite rejects the forged digest-to-payload binding explicitly.
    assert!(matches!(
        result,
        Err(SessionStoreError::ToolArtifactDigestMismatch { claimed, .. }) if claimed == hash
    ));
}

#[tokio::test]
async fn put_tool_artifact_rejects_ambiguous_durable_storage_source() {
    // Given: an inline artifact row corrupted to also name external storage.
    let root = TestDir::new();
    let store = SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
        .await
        .expect("store should open");
    let session = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let hash = "sha256-239f59ed55e737c77147cf55ad0c1b030b6d7ee748a7426952f9b852d5a935e5";
    let artifact =
        NewToolArtifact::inline(hash, "preview", "payload").expect("artifact should be valid");
    let receipt = store
        .put_tool_artifact(&session, Seq::ZERO, artifact.clone())
        .await
        .expect("artifact should commit");
    sqlx::query(
        "UPDATE tool_artifacts SET storage_ref = ? WHERE session_id = ? AND artifact_id = ?",
    )
    .bind("objects/forged")
    .bind(session.as_str())
    .bind(receipt.reference().artifact_id())
    .execute(&store.pool)
    .await
    .expect("fixture should create an ambiguous storage source");

    // When: an idempotent write reads the ambiguous durable row.
    let result = store
        .put_tool_artifact(&session, receipt.journal_seq(), artifact)
        .await;

    // Then: SQLite refuses a row that is both inline and external.
    assert!(matches!(
        result,
        Err(SessionStoreError::InvalidToolArtifact(_))
    ));
}

#[tokio::test]
async fn put_tool_artifact_rejects_tampered_journal_identity() {
    // Given: a valid artifact whose audit fact is later corrupted.
    let root = TestDir::new();
    let store = SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
        .await
        .expect("store should open");
    let session = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let hash = "sha256-239f59ed55e737c77147cf55ad0c1b030b6d7ee748a7426952f9b852d5a935e5";
    let artifact =
        NewToolArtifact::inline(hash, "preview", "payload").expect("artifact should be valid");
    let receipt = store
        .put_tool_artifact(&session, Seq::ZERO, artifact.clone())
        .await
        .expect("artifact should commit");
    sqlx::query(
        "UPDATE journal_entries SET payload_json = json_set(payload_json, '$.bytes', 999) \
         WHERE session_id = ? AND kind = 'tool_artifact'",
    )
    .bind(session.as_str())
    .execute(&store.pool)
    .await
    .expect("fixture should tamper with the audit fact");

    // When: an idempotent write tries to reuse the corrupted receipt fact.
    let result = store
        .put_tool_artifact(&session, receipt.journal_seq(), artifact)
        .await;

    // Then: the store refuses to issue a receipt for mismatched audit metadata.
    assert!(matches!(
        result,
        Err(SessionStoreError::ToolArtifactJournalMismatch { .. })
    ));
}
