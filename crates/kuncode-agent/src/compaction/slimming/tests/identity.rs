use super::{FixedMarkerCounter, artifact_source, exchange, location};
use crate::{
    compaction::{
        protocol::{group_messages, select_protected_recent_tail},
        slimming::slim_tool_results,
    },
    session_store::Seq,
    tool::ToolOutput,
};

#[tokio::test]
async fn repeated_tool_call_ids_are_disambiguated_by_exact_location() {
    // Given: two old exchanges reuse one call id but have distinct sidecar positions.
    let payload =
        ToolOutput::success(serde_json::json!({"body": "x".repeat(2_000)})).to_model_content();
    let messages = [
        exchange(
            "same",
            "read_file",
            serde_json::json!({"path": "one"}),
            &payload,
        ),
        exchange(
            "same",
            "read_file",
            serde_json::json!({"path": "two"}),
            &payload,
        ),
        exchange(
            "same",
            "read_file",
            serde_json::json!({"path": "recent"}),
            "recent",
        ),
    ]
    .concat();
    let groups = group_messages(&messages).expect("fixture should contain closed exchanges");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1)
        .expect("fixture should have a protected tail");
    let source = artifact_source(&groups, &[(location(1), "same", 1_000, Some(Seq::new(4)))]);

    // When: only the second old result location is authorized.
    let result = slim_tool_results(
        &source,
        &protected,
        &[location(1)],
        &FixedMarkerCounter::new(100),
    )
    .await
    .expect("exact sidecar location should be valid");

    // Then: equal call ids do not cause the first or protected result to change.
    assert_eq!(result.groups()[0], groups[0]);
    assert_ne!(result.groups()[1], groups[1]);
    assert_eq!(result.groups()[2], groups[2]);
}
