use super::support::{
    AgentCompactionConfig, AgentConfig, AgentRunner, AgentSession, Arc, AssistantContent,
    CollectingObserver, CompactionConfig, CompactionMode, Deserialize, EventKind, FakeModel,
    FixedRunnerGroupEstimator, JsonSchema, LARGE_RESULT_BYTES, Message, NewSession,
    PermissionAction, PermissionRequest, ScriptedRequestEstimator, Seq, SessionStore,
    SqliteSessionStore, TestDir, ToolContext, ToolDefinition, ToolOutput, ToolRegistry,
    ToolResultContent, TypedTool, UserContent, Value, async_trait, definition_for, response,
};

use crate::compaction::protocol::group_messages;

#[derive(Deserialize, JsonSchema)]
struct ScriptedResultArgs {
    large: bool,
}

struct ScriptedResultTool {
    definition: ToolDefinition,
}

impl ScriptedResultTool {
    fn new() -> Self {
        Self {
            definition: definition_for::<ScriptedResultArgs>(
                "scripted_result",
                "Returns a deterministic test payload",
            ),
        }
    }
}

#[async_trait]
impl TypedTool for ScriptedResultTool {
    type Args = ScriptedResultArgs;
    type Output = String;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn permission(&self, _args: &Self::Args, _ctx: &ToolContext) -> PermissionRequest {
        PermissionRequest::new(
            "scripted_result",
            PermissionAction::Read,
            None,
            "return scripted result",
        )
    }

    async fn run(&self, args: Self::Args, _ctx: &ToolContext) -> ToolOutput<Self::Output> {
        let payload = if args.large {
            "L".repeat(LARGE_RESULT_BYTES)
        } else {
            "small".to_string()
        };
        ToolOutput::success(payload)
    }
}

#[tokio::test]
async fn runner_spills_old_large_tool_result_and_commits_sqlite_checkpoint() {
    // Given
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
    let mut session = AgentSession::new();
    session
        .attach_session_id(session_id.clone())
        .expect("fresh session should attach");
    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_large",
            "scripted_result",
            serde_json::json!({ "large": true }),
        )),
        response(AssistantContent::tool_call(
            "call_small",
            "scripted_result",
            serde_json::json!({ "large": false }),
        )),
        response(AssistantContent::text("done")),
    ]);
    let mut registry = ToolRegistry::new();
    registry.register(ScriptedResultTool::new());
    let policy = CompactionConfig::new(CompactionMode::Enabled, 1_000, 100, 0)
        .expect("test window should be valid");
    let compaction = AgentCompactionConfig::new(policy, "test-model", 128)
        .expect("test compaction runtime should be valid");
    let observer = Arc::new(CollectingObserver::default());
    let mut runner = AgentRunner::with_config(
        model.clone(),
        registry,
        AgentConfig {
            max_tokens: Some(100),
            compaction: Some(compaction),
            ..AgentConfig::default()
        },
    )
    .with_session_store(store.clone())
    .with_observer(observer.clone());
    runner.token_estimator = Arc::new(ScriptedRequestEstimator);
    runner.group_estimator = Arc::new(FixedRunnerGroupEstimator(100));

    // When
    let turn = runner
        .run_turn(&mut session, "exercise artifact compaction")
        .await
        .expect("large result should compact and continue");

    // Then
    assert_eq!(turn.final_text(&session), "done");
    assert_eq!(turn.iterations, 3);
    let requests = model.requests();
    assert_eq!(requests.len(), 3, "summary must not call the model");
    assert!(
        requests
            .iter()
            .all(|request| request.output_schema.is_none())
    );
    let third_messages = requests[2].chat_history.to_vec();
    assert!(group_messages(&third_messages).is_ok());
    let large_marker = tool_result_text_by_id(&third_messages, "call_large")
        .expect("third request should retain a marker for the large result");
    let marker: Value = serde_json::from_str(large_marker).expect("marker should be JSON");
    assert!(marker.get("artifact_id").is_some());
    assert_eq!(
        tool_result_text_by_id(&third_messages, "call_small"),
        Some(r#"{"ok":true,"data":"small","truncated":false}"#)
    );

    let checkpoint = store
        .latest_checkpoint(&session_id)
        .await
        .expect("checkpoint read should succeed")
        .expect("compaction should persist a checkpoint");
    assert_eq!(
        checkpoint.active_messages,
        third_messages[1..third_messages.len() - 1]
    );
    assert!(checkpoint.summary_json.is_none());
    let journal = store
        .replay_after(&session_id, Seq::ZERO)
        .await
        .expect("journal replay should succeed");
    let original_large = journal
        .iter()
        .filter(|entry| entry.kind == "message")
        .filter_map(|entry| entry.clone().into_message().ok())
        .find_map(|message| {
            tool_result_text_by_id(std::slice::from_ref(&message), "call_large").map(str::to_string)
        })
        .expect("journal should retain the original large result");
    let output: ToolOutput =
        serde_json::from_str(&original_large).expect("journaled output should be JSON");
    assert_eq!(
        output.data,
        Some(Value::String("L".repeat(LARGE_RESULT_BYTES)))
    );
    for kind in ["tool_artifact", "compaction", "checkpoint_ref"] {
        assert_eq!(
            journal.iter().filter(|entry| entry.kind == kind).count(),
            1,
            "journal should contain one {kind} fact"
        );
    }

    let events = observer.events();
    let started = events
        .iter()
        .position(|event| matches!(event.kind, EventKind::CompactionStarted { .. }))
        .expect("start event should be emitted");
    let completed = events
        .iter()
        .position(|event| matches!(event.kind, EventKind::CompactionCompleted { .. }))
        .expect("completion event should be emitted");
    let third_model_start = events
        .iter()
        .position(|event| event.iteration == Some(2) && matches!(event.kind, EventKind::ModelStart))
        .expect("third model start should be emitted");
    assert!(started < completed);
    assert!(completed < third_model_start);
    assert!(matches!(
        &events[completed].kind,
        EventKind::CompactionCompleted {
            passes,
            artifact_count: 1,
            summary_usage: None,
            ..
        } if passes == &["artifact_spill", "atomic_commit"]
    ));
}

fn tool_result_text_by_id<'a>(messages: &'a [Message], id: &str) -> Option<&'a str> {
    messages.iter().find_map(|message| {
        let Message::User { content } = message else {
            return None;
        };
        content.iter().find_map(|block| {
            let UserContent::ToolResult(result) = block else {
                return None;
            };
            if result.id != id {
                return None;
            }
            let ToolResultContent::Text(text) = result.content.first();
            Some(text.text_ref())
        })
    })
}
