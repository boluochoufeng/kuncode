use kuncode_core::{
    completion::{AssistantContent, Message, ToolResult, ToolResultContent, UserContent},
    non_empty_vec::NonEmptyVec,
};

use super::support::{FixedCounter, persisted_session, tool_exchange};
use crate::{
    compaction::{
        artifact::{
            ArtifactResultLocation, ArtifactSpillFailure, ArtifactSpillInput, ArtifactSpillOutcome,
            spill_artifacts,
        },
        protocol::{group_messages, select_protected_recent_tail},
    },
    session_store::{NewSession, SessionStore, turso::TursoSessionStore},
    test_support::TestDir,
    tool::ToolOutput,
};

#[tokio::test]
async fn spills_valid_tool_while_preserving_failed_sibling() {
    // Given: one closed multi-tool exchange with one malformed result.
    let root = TestDir::new();
    let store = TursoSessionStore::open(root.path().join("sessions.db"))
        .await
        .expect("store should open");
    let session_id = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let old = multi_exchange();
    let messages = [old.clone(), tool_exchange("recent", "read_file", "recent")].concat();
    let session = persisted_session(&store, session_id, &messages).await;
    let groups = group_messages(session.messages()).expect("history should be valid");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1).expect("tail should exist");
    let input =
        ArtifactSpillInput::new(&groups, &protected, &session).expect("input should be valid");

    // When: the spill pass processes both siblings independently.
    let result = spill_artifacts(input, &store, &FixedCounter::new(9_000, 100))
        .await
        .expect("journal audit should pass");

    // Then: the valid result becomes a marker and the malformed payload stays exact.
    assert_ne!(result.groups()[0], groups[0]);
    assert_eq!(result.groups()[1], groups[1]);
    assert!(matches!(
        result.outcomes(),
        [
            ArtifactSpillOutcome::Spilled { location: spilled, tool_call_id, .. },
            ArtifactSpillOutcome::Failed {
                location: failed,
                failure: ArtifactSpillFailure::Parse(_),
                ..
            }
        ] if tool_call_id == "valid"
            && *spilled == ArtifactResultLocation {
                group_index: 0,
                result_message_index: 0,
                content_index: 0,
            }
            && *failed == ArtifactResultLocation {
                group_index: 0,
                result_message_index: 0,
                content_index: 1,
            }
    ));
    let crate::compaction::protocol::ProtocolGroup::ToolExchange { results, .. } =
        &result.groups()[0]
    else {
        panic!("candidate should remain a tool exchange");
    };
    let Message::User { content } = &results[0] else {
        panic!("candidate results should use the user role");
    };
    let UserContent::ToolResult(invalid) = &content[1] else {
        panic!("second block should remain a tool result");
    };
    let ToolResultContent::Text(text) = invalid.content.first();
    assert_eq!(text.text_ref(), "not JSON");
}

fn multi_exchange() -> Vec<Message> {
    let assistant = Message::Assistant {
        id: None,
        content: NonEmptyVec::from_first_rest(
            AssistantContent::tool_call("valid", "bash", serde_json::json!({})),
            vec![AssistantContent::tool_call(
                "invalid",
                "read_file",
                serde_json::json!({}),
            )],
        ),
    };
    let valid = ToolOutput::success(serde_json::json!({ "body": "valid" })).to_model_content();
    let results = Message::User {
        content: NonEmptyVec::from_first_rest(
            UserContent::ToolResult(ToolResult {
                id: "valid".to_string(),
                call_id: None,
                content: NonEmptyVec::new(ToolResultContent::text(valid)),
            }),
            vec![UserContent::ToolResult(ToolResult {
                id: "invalid".to_string(),
                call_id: None,
                content: NonEmptyVec::new(ToolResultContent::text("not JSON")),
            })],
        ),
    };
    vec![assistant, results]
}
