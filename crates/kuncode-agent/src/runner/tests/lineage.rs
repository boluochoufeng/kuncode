use super::support::{
    AgentConfig, AgentRunner, AgentSession, Arc, AssistantContent, FakeModel, NewSession,
    ScriptedHook, Seq, SessionStore, SqliteSessionStore, TestDir, ToolRegistry, bash, response,
};

// Verifies that only direct turn input receives human-authored lineage.

#[tokio::test]
async fn only_real_prompt_is_human_and_durable_appends_get_exact_coverage() {
    let root = TestDir::new();
    let store = Arc::new(
        SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
            .await
            .expect("store should open"),
    );
    let session_id = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_1",
            "bash",
            serde_json::json!({ "cmd": "printf ok" }),
        )),
        response(AssistantContent::text("draft")),
        response(AssistantContent::text("done")),
    ]);
    let mut registry = ToolRegistry::new();
    registry.register(bash().await);
    let hook = ScriptedHook::default()
        .add_context("HOOK_CONTEXT")
        .add_feedback("POST_TOOL_FEEDBACK")
        .stop_continue(1, "STOP_CONTINUE");
    let runner = AgentRunner::with_config(
        model,
        registry,
        AgentConfig {
            todo_reminder_interval: Some(1),
            ..AgentConfig::default()
        },
    )
    .with_hook(Arc::new(hook))
    .with_session_store(store);
    let mut session = AgentSession::new();
    session
        .attach_session_id(session_id)
        .expect("fresh session should attach");

    runner
        .run_turn(&mut session, "REAL_PROMPT")
        .await
        .expect("runs");

    assert_eq!(
        session.trusted_human_message_indices().collect::<Vec<_>>(),
        [0]
    );
    assert_eq!(session.message_lineage().len(), session.messages().len());
    for (index, lineage) in session.message_lineage().iter().enumerate() {
        let expected = Seq::new(i64::try_from(index + 1).expect("small test index"));
        let coverage = lineage.coverage().expect("every append persisted");
        assert_eq!((coverage.start(), coverage.end()), (expected, expected));
        assert!(lineage.artifact_refs().is_empty());
    }
}
