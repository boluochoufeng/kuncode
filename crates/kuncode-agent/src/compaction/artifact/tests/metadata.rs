use kuncode_core::completion::{ToolResultContent, UserContent};

use super::support::{
    SerializedByteCounter, persisted_session, tool_exchange, tool_exchange_with_output,
};
use crate::{
    compaction::{
        artifact::{ArtifactSpillInput, spill_artifacts},
        protocol::{ProtocolGroup, group_messages, select_protected_recent_tail},
    },
    session_store::{NewSession, SessionStore, sqlite::SqliteSessionStore},
    test_support::TestDir,
    tool::ToolOutput,
};

#[tokio::test]
async fn bounds_adversarial_metadata_and_recounts_final_marker() {
    // Given: a large error payload with oversized free-text metadata.
    let root = TestDir::new();
    let store = SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
        .await
        .expect("store should open");
    let session_id = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let tool_name = "tool".repeat(2_000);
    let error_message = "failure".repeat(2_000);
    let output = ToolOutput::failure("custom_kind", error_message).truncated();
    let messages = [
        tool_exchange_with_output("old", &tool_name, output),
        tool_exchange("recent", "read_file", "recent"),
    ]
    .concat();
    let session = persisted_session(&store, session_id, &messages).await;
    let groups = group_messages(session.messages()).expect("history should be valid");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1).expect("tail should exist");
    let input =
        ArtifactSpillInput::new(&groups, &protected, &session).expect("input should be valid");

    // When: the same serialized-byte counter measures source and marker.
    let result = spill_artifacts(input, &store, &SerializedByteCounter)
        .await
        .expect("journal audit should pass");

    // Then: capped metadata and preview keep the final provider-visible result bounded.
    let marker_result = first_result(&result.groups()[0]);
    let marker_text = match marker_result.content.first() {
        ToolResultContent::Text(text) => text.text_ref(),
    };
    let marker: serde_json::Value =
        serde_json::from_str(marker_text).expect("marker should be JSON");
    let visible_bytes = serde_json::to_vec(marker_result)
        .expect("marker result should serialize")
        .len();
    assert!(visible_bytes <= 2_048);
    assert_eq!(marker["ok"], false);
    assert_eq!(marker["truncated"], true);
    assert_eq!(marker["error"]["kind"], "custom_kind");
    assert_eq!(
        marker["original_tokens"],
        u64::try_from(
            serde_json::to_vec(first_result(&groups[0]))
                .expect("source should serialize")
                .len()
        )
        .expect("test payload should fit u64")
    );
    assert!(
        marker["content_hash"]
            .as_str()
            .expect("hash should be text")
            .starts_with("sha256-")
    );
    assert!(
        marker["tool_name"]
            .as_str()
            .expect("name should be text")
            .len()
            <= 256
    );
    assert!(
        marker["error"]["message"]
            .as_str()
            .expect("message should be text")
            .len()
            <= 512
    );
    assert_eq!(result.groups()[1], groups[1]);
}

fn first_result(group: &ProtocolGroup) -> &kuncode_core::completion::ToolResult {
    let ProtocolGroup::ToolExchange { results, .. } = group else {
        panic!("fixture should be a tool exchange");
    };
    let kuncode_core::completion::Message::User { content } = &results[0] else {
        panic!("fixture result should use the user role");
    };
    let UserContent::ToolResult(result) = content.first() else {
        panic!("fixture should contain a tool result");
    };
    result
}
