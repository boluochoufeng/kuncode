use kuncode_core::completion::Message;

use super::{FixedMarkerCounter, artifact_source, exchange, fixture_groups, location};
use crate::{
    compaction::{
        protocol::{
            ProtectedRecentTail, ProtocolGroup, group_messages, select_protected_recent_tail,
        },
        slimming::{ToolResultSlimmingError, slim_tool_results},
    },
    session_store::Seq,
    tool::ToolOutput,
};

#[tokio::test]
async fn binds_projected_groups_to_the_exact_artifact_pass() {
    // Given: two value-equal artifact capabilities from distinct spill passes.
    let groups = fixture_groups();
    let protected = select_protected_recent_tail(&groups, 0, |_| 1)
        .expect("fixture should have a protected tail");
    let source = artifact_source(&groups, &[(location(0), "old", 1_000, Some(Seq::new(2)))]);
    let lookalike = artifact_source(&groups, &[(location(0), "old", 1_000, Some(Seq::new(2)))]);

    // When: projection succeeds against only the first capability.
    let result = slim_tool_results(
        &source,
        &protected,
        &[location(0)],
        &FixedMarkerCounter::new(100),
    )
    .await
    .expect("authorized projection should succeed");

    // Then: the projection retains only the exact authorizing capability.
    assert!(std::ptr::eq(result.source(), &source));
    assert!(!std::ptr::eq(result.source(), &lookalike));
}

#[tokio::test]
async fn rejects_tail_that_omits_the_latest_tool_exchange() {
    let mut groups = fixture_groups();
    groups.push(ProtocolGroup::Message(Message::assistant("after tools")));
    let protected = ProtectedRecentTail {
        group_range: 2..3,
        estimated_tokens: 1,
        budget_tokens: 1,
    };
    let source = artifact_source(&groups, &[]);

    let result = slim_tool_results(&source, &protected, &[], &FixedMarkerCounter::new(100)).await;

    assert_eq!(result, Err(ToolResultSlimmingError::InvalidProtectedTail));
}

#[tokio::test]
async fn rejects_noncanonical_tool_exchange_before_projection() {
    let mut groups = fixture_groups();
    let protected = select_protected_recent_tail(&groups, 0, |_| 1)
        .expect("fixture should have a protected tail");
    let ProtocolGroup::ToolExchange { results, .. } = &mut groups[0] else {
        panic!("fixture should start with a tool exchange");
    };
    results.clear();
    let source = artifact_source(&groups, &[]);

    let result = slim_tool_results(&source, &protected, &[], &FixedMarkerCounter::new(100)).await;

    assert_eq!(result, Err(ToolResultSlimmingError::InvalidProtocolGroups));
}

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
