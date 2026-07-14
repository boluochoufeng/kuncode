use sha2::{Digest, Sha256};

use crate::{
    session_store::{
        JournalKind, NewSession, NewToolArtifact, Seq, SessionStore, SessionStoreError,
        sqlite::SqliteSessionStore,
    },
    test_support::TestDir,
};

fn inline_artifact(preview: &str, payload: &str) -> NewToolArtifact {
    let content_hash = format!("sha256-{:x}", Sha256::digest(payload.as_bytes()));
    NewToolArtifact::inline(content_hash, preview, payload).expect("artifact should be valid")
}

#[tokio::test]
async fn concurrent_artifact_writers_return_one_typed_head_conflict() {
    let root = TestDir::new();
    let database = root.path().join("sessions.sqlite3");
    let first_store = SqliteSessionStore::open(&database)
        .await
        .expect("first store should open");
    let second_store = SqliteSessionStore::open(&database)
        .await
        .expect("second store should open");
    let session = first_store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    // Both writers prove against the same head; the barrier makes them exercise
    // the writer-lock CAS instead of intentionally sequencing the operations.
    let barrier = tokio::sync::Barrier::new(2);
    let first_write = async {
        barrier.wait().await;
        first_store
            .put_tool_artifact(
                &session,
                Seq::ZERO,
                inline_artifact("first", "first payload"),
            )
            .await
    };
    let second_write = async {
        barrier.wait().await;
        second_store
            .put_tool_artifact(
                &session,
                Seq::ZERO,
                inline_artifact("second", "second payload"),
            )
            .await
    };

    let (first, second) = tokio::join!(first_write, second_write);
    let (committed, rejected) = match (first, second) {
        (Ok(committed), Err(rejected)) | (Err(rejected), Ok(committed)) => (committed, rejected),
        (first, second) => panic!("expected one commit and one rejection: {first:?}, {second:?}"),
    };
    assert!(matches!(
        rejected,
        SessionStoreError::JournalHeadConflict { expected: 0, actual }
            if actual == committed.journal_seq().get()
    ));
    let entries = first_store
        .replay_after(&session, Seq::ZERO)
        .await
        .expect("journal should replay");
    let artifact_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM tool_artifacts WHERE session_id = ?")
            .bind(session.as_str())
            .fetch_one(&first_store.pool)
            .await
            .expect("artifact rows should be queryable");

    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.kind == JournalKind::ToolArtifact.as_str())
            .count(),
        1
    );
    assert_eq!(artifact_rows, 1);
}
