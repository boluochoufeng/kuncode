use kuncode_core::completion::Message;

use super::{FixedMarkerCounter, artifact_source, fixture_groups};
use crate::compaction::{
    protocol::{ProtectedRecentTail, ProtocolGroup, select_protected_recent_tail},
    slimming::{ToolResultSlimmingError, slim_tool_results},
};

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
