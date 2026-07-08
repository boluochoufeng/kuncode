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
    assert_eq!(first.artifact_id(), "tool-result-sha256-deadbeef");
    assert_eq!(first.content_hash(), "sha256-deadbeef");
    assert_eq!(first.bytes(), 18);
    assert_eq!(artifact_entries.len(), 1);
    assert_eq!(
        artifact_entries[0].payload_json["artifact_id"],
        first.artifact_id()
    );
}
