use crate::{
    session_store::{
        JournalKind, NewJournalEntry, NewSession, NewToolArtifact, Seq, SessionStore,
        SessionStoreError, turso::TursoSessionStore,
    },
    test_support::TestDir,
};

#[tokio::test]
async fn put_tool_artifact_rejects_tampered_durable_payload_identity() {
    // Given: a valid artifact whose durable payload is later corrupted.
    let root = TestDir::new();
    let store = TursoSessionStore::open(root.path().join("sessions.db"))
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
    {
        let connection = store.connection_for_test().await;
        connection
            .execute(
                "UPDATE tool_artifacts SET payload_text = ?1 \
                 WHERE session_id = ?2 AND artifact_id = ?3",
                (
                    "forged payload",
                    session.as_str(),
                    receipt.reference().artifact_id(),
                ),
            )
            .await
            .expect("fixture should tamper with the durable payload");
    }

    // When: an idempotent write reads the corrupted durable identity.
    let result = store
        .put_tool_artifact(&session, receipt.journal_seq(), artifact)
        .await;

    // Then: Turso rejects the forged digest-to-payload binding explicitly.
    assert!(matches!(
        result,
        Err(SessionStoreError::ToolArtifactStoredIntegrity {
            artifact_id,
            message,
            ..
        }) if artifact_id == format!("tool-result-{hash}")
            && message.contains("digest mismatch")
    ));
}

#[tokio::test]
async fn put_tool_artifact_rejects_ambiguous_durable_storage_source() {
    // Given: an inline artifact row corrupted to also name external storage.
    let root = TestDir::new();
    let store = TursoSessionStore::open(root.path().join("sessions.db"))
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
    {
        let connection = store.connection_for_test().await;
        connection
            .execute(
                "UPDATE tool_artifacts SET storage_ref = ?1 \
                 WHERE session_id = ?2 AND artifact_id = ?3",
                (
                    "objects/forged",
                    session.as_str(),
                    receipt.reference().artifact_id(),
                ),
            )
            .await
            .expect("fixture should create an ambiguous storage source");
    }

    // When: an idempotent write reads the ambiguous durable row.
    let result = store
        .put_tool_artifact(&session, receipt.journal_seq(), artifact)
        .await;

    // Then: Turso refuses a row that is both inline and external.
    assert!(matches!(
        result,
        Err(SessionStoreError::ToolArtifactStoredIntegrity { .. })
    ));
}

#[tokio::test]
async fn put_tool_artifact_rejects_tampered_journal_identity() {
    // Given: a valid artifact whose audit fact is later corrupted.
    let root = TestDir::new();
    let store = TursoSessionStore::open(root.path().join("sessions.db"))
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
    {
        let payload = serde_json::json!({
            "artifact_id": receipt.reference().artifact_id(),
            "content_hash": receipt.reference().content_hash(),
            "bytes": 999,
            "preview": receipt.reference().preview(),
        });
        let connection = store.connection_for_test().await;
        connection
            .execute(
                "UPDATE journal_entries SET payload_json = ?1 \
                 WHERE session_id = ?2 AND kind = 'tool_artifact'",
                (
                    serde_json::to_string(&payload).expect("fixture payload should encode"),
                    session.as_str(),
                ),
            )
            .await
            .expect("fixture should tamper with the audit fact");
    }

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

#[tokio::test]
async fn put_tool_artifact_rejects_a_non_positive_journal_sequence() {
    let root = TestDir::new();
    let store = TursoSessionStore::open(root.path().join("sessions.db"))
        .await
        .expect("store should open");
    let session = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let hash = "sha256-239f59ed55e737c77147cf55ad0c1b030b6d7ee748a7426952f9b852d5a935e5";
    let artifact =
        NewToolArtifact::inline(hash, "preview", "payload").expect("artifact should be valid");
    store
        .put_tool_artifact(&session, Seq::ZERO, artifact.clone())
        .await
        .expect("artifact should commit");
    let head = store
        .append(
            &session,
            NewJournalEntry::raw(
                JournalKind::SessionNote,
                serde_json::json!({"note": "later durable fact"}),
            ),
        )
        .await
        .expect("later fact should commit");
    {
        let connection = store.connection_for_test().await;
        connection
            .execute(
                "UPDATE journal_entries SET seq = -1 \
                 WHERE session_id = ?1 AND kind = 'tool_artifact'",
                [session.as_str()],
            )
            .await
            .expect("fixture should corrupt the earlier artifact sequence");
    }

    let result = store.put_tool_artifact(&session, head, artifact).await;

    assert!(matches!(
        result,
        Err(SessionStoreError::ToolArtifactJournalIntegrity { .. })
    ));
}
