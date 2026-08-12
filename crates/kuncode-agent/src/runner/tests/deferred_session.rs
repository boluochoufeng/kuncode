use super::support::{
    AgentRunner, AgentSession, Arc, AssistantContent, FakeModel, NewSession, SessionStore, TestDir,
    ToolRegistry, TursoSessionStore, response,
};

/// A deferred durable session must leave no store row until a message is
/// actually journaled — the whole point of deferral is that a run which never
/// exchanges one is never persisted.
#[tokio::test]
async fn deferred_session_is_created_only_when_a_message_is_journaled() {
    let root = TestDir::new();
    let project = root.path().to_path_buf();
    let store = Arc::new(
        TursoSessionStore::open(root.path().join("sessions.db"))
            .await
            .expect("store should open"),
    );
    let model = FakeModel::new([response(AssistantContent::text("done"))]);
    let runner = AgentRunner::new(model, ToolRegistry::new()).with_session_store(store.clone());
    let mut session = AgentSession::new();
    session
        .defer_durable_session(NewSession::new(project.clone()))
        .expect("fresh session should defer");

    assert!(
        store
            .list_sessions(&project, 10)
            .await
            .expect("listing should succeed")
            .is_empty(),
        "no row may exist before the first message"
    );
    assert!(session.session_id().is_none());

    runner
        .run_turn(&mut session, "hi")
        .await
        .expect("turn should complete");

    let sessions = store
        .list_sessions(&project, 10)
        .await
        .expect("listing should succeed");
    assert_eq!(sessions.len(), 1, "the first message materializes the row");
    assert_eq!(
        session.session_id(),
        Some(&sessions[0].id),
        "the session carries the created identity"
    );
    assert!(session.is_durable());
    // Prompt + final answer were both journaled into the materialized session.
    assert_eq!(sessions[0].message_count, 2);
}

/// Deferral declares intent to persist; a runner with no store cannot honor
/// it and must fail persistence closed rather than silently staying
/// in-memory.
#[tokio::test]
async fn deferred_session_without_a_store_fails_persistence_closed() {
    let model = FakeModel::new([response(AssistantContent::text("done"))]);
    let runner = AgentRunner::new(model, ToolRegistry::new());
    let mut session = AgentSession::new();
    session
        .defer_durable_session(NewSession::new("/p".into()))
        .expect("fresh session should defer");

    runner
        .run_turn(&mut session, "hi")
        .await
        .expect("the turn itself still completes");

    assert!(session.session_id().is_none());
    assert!(
        session.take_persistence_error().is_some(),
        "the unhonored deferral must surface as a persistence failure"
    );
}
