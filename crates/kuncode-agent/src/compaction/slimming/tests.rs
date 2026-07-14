use kuncode_core::{
    completion::{AssistantContent, Message, ToolResult, ToolResultContent, UserContent},
    non_empty_vec::NonEmptyVec,
};

use async_trait::async_trait;

use super::{SlimmingOutcome, SlimmingRetention, ToolResultSlimmingError, slim_tool_results};
use crate::{
    compaction::{
        artifact::{
            ArtifactResultLocation, ArtifactSpillOutcome, ArtifactSpillResult,
            ArtifactTokenCounter, ArtifactTokenCounterError, fixture_below_threshold,
            fixture_spill_result, tool_result_hash,
        },
        protocol::{ProtocolGroup, group_messages, select_protected_recent_tail},
    },
    session_store::Seq,
    tool::ToolOutput,
};

mod binding;
mod boundaries;
mod identity;
mod provenance;
mod retention;

#[tokio::test]
async fn slims_only_explicit_old_result_and_keeps_protected_tail_exact() {
    // Given: one authorized old result and one mandatory recent exchange.
    let groups = fixture_groups();
    let protected = select_protected_recent_tail(&groups, 0, |_| 1)
        .expect("non-empty history should have a protected tail");
    let source = artifact_source(&groups, &[(location(0), "old", 1_000, Some(Seq::new(2)))]);

    // When: the explicit slimming policy is applied.
    let result = slim_tool_results(
        &source,
        &protected,
        &[location(0)],
        &FixedMarkerCounter::new(100),
    )
    .await
    .expect("explicit old result should be valid");

    // Then: only the old result changes and the marker carries real provenance.
    assert_ne!(result.groups()[0], groups[0]);
    assert_eq!(result.groups()[1], groups[1]);
    assert_eq!(
        result.outcomes(),
        &[SlimmingOutcome::Slimmed {
            location: location(0),
            original_journal_seq: Seq::new(2),
            original_tokens: 1_000,
            slimmed_tokens: 100,
        }]
    );
    let marker = marker_json(&result.groups()[0]);
    assert_eq!(marker["tool_name"], "bash");
    assert_eq!(marker["tool_call_id"], "old");
    assert_eq!(marker["original_journal_seq"], 2);
    assert_eq!(marker["command"], "cargo test");
    assert_eq!(marker["exit_code"], 1);
    assert!(
        marker["preview"]
            .as_str()
            .is_some_and(|text| text.len() <= 2_048)
    );
}

#[tokio::test]
async fn empty_policy_is_conservative_and_protected_candidate_is_rejected() {
    // Given: valid grouped history with a mandatory protected exchange.
    let groups = fixture_groups();
    let protected = select_protected_recent_tail(&groups, 0, |_| 1)
        .expect("non-empty history should have a protected tail");
    let source = artifact_source(&groups, &[]);

    // When: no result is authorized for loss.
    let unchanged = slim_tool_results(&source, &protected, &[], &FixedMarkerCounter::new(100))
        .await
        .expect("empty policy should be valid");

    // Then: the conservative default is byte-for-byte semantic equality.
    assert_eq!(unchanged.groups(), groups);
    let protected_source = artifact_source(
        &groups,
        &[(location(1), "recent", 1_000, Some(Seq::new(4)))],
    );
    assert_eq!(
        slim_tool_results(
            &protected_source,
            &protected,
            &[location(1)],
            &FixedMarkerCounter::new(100),
        )
        .await,
        Err(ToolResultSlimmingError::ProtectedCandidate { group_index: 1 })
    );
}

#[tokio::test]
async fn retains_candidate_when_marker_has_no_provider_visible_savings() {
    // Given: a below-threshold sidecar and a provider count equal to the original.
    let groups = fixture_groups();
    let protected = select_protected_recent_tail(&groups, 0, |_| 1)
        .expect("history should have a protected tail");
    let source = artifact_source(&groups, &[(location(0), "old", 1_000, Some(Seq::new(2)))]);

    // When: the final marker is re-counted through the provider seam.
    let result = slim_tool_results(
        &source,
        &protected,
        &[location(0)],
        &FixedMarkerCounter::new(1_000),
    )
    .await
    .expect("no-savings is an isolated retention outcome");

    // Then: the original result remains exact and the reason is observable.
    assert_eq!(result.groups(), groups);
    assert_eq!(
        result.outcomes(),
        &[SlimmingOutcome::Retained {
            location: location(0),
            reason: SlimmingRetention::NoSavings,
        }]
    );
}

pub(super) fn fixture_groups() -> Vec<ProtocolGroup> {
    let old_output = ToolOutput::success(serde_json::json!({
        "exit_code": 1,
        "stdout": "x".repeat(8_000),
        "stderr": "failure"
    }))
    .to_model_content();
    let messages = [
        exchange(
            "old",
            "bash",
            serde_json::json!({"cmd": "cargo test"}),
            &old_output,
        ),
        exchange(
            "recent",
            "read_file",
            serde_json::json!({"path": "src/lib.rs"}),
            "recent",
        ),
    ]
    .concat();
    group_messages(&messages).expect("fixture should contain closed exchanges")
}

pub(super) fn exchange(
    id: &str,
    name: &str,
    arguments: serde_json::Value,
    payload: &str,
) -> Vec<Message> {
    vec![
        Message::Assistant {
            id: None,
            content: NonEmptyVec::new(AssistantContent::tool_call(id, name, arguments)),
        },
        Message::User {
            content: NonEmptyVec::new(UserContent::ToolResult(ToolResult {
                id: id.to_string(),
                call_id: None,
                content: NonEmptyVec::new(ToolResultContent::text(payload)),
            })),
        },
    ]
}

fn marker_json(group: &ProtocolGroup) -> serde_json::Value {
    let ProtocolGroup::ToolExchange { results, .. } = group else {
        panic!("fixture should remain a tool exchange");
    };
    let Message::User { content } = &results[0] else {
        panic!("fixture result should remain a user message");
    };
    let UserContent::ToolResult(result) = content.first() else {
        panic!("fixture should remain a tool result");
    };
    let ToolResultContent::Text(text) = result.content.first();
    serde_json::from_str(text.text_ref()).expect("slimming marker should be JSON")
}

pub(super) fn location(group_index: usize) -> ArtifactResultLocation {
    ArtifactResultLocation {
        group_index,
        result_message_index: 0,
        content_index: 0,
    }
}

pub(super) fn artifact_source(
    groups: &[ProtocolGroup],
    authorizations: &[(ArtifactResultLocation, &str, u64, Option<Seq>)],
) -> ArtifactSpillResult {
    let outcomes = authorizations
        .iter()
        .map(|(location, tool_call_id, tokens, source_journal_seq)| {
            authorization_for(
                groups,
                *location,
                tool_call_id,
                *tokens,
                *source_journal_seq,
            )
        })
        .collect();
    fixture_spill_result(groups.to_vec(), Seq::new(99), outcomes)
}

pub(super) fn authorization_for(
    groups: &[ProtocolGroup],
    location: ArtifactResultLocation,
    tool_call_id: &str,
    tokens: u64,
    source_journal_seq: Option<Seq>,
) -> ArtifactSpillOutcome {
    fixture_below_threshold(
        location,
        tool_call_id.to_string(),
        tokens,
        tool_result_hash(result_at(groups, location))
            .expect("fixture tool result should serialize"),
        source_journal_seq,
    )
}

pub(super) fn result_at(groups: &[ProtocolGroup], location: ArtifactResultLocation) -> &ToolResult {
    let ProtocolGroup::ToolExchange { results, .. } = &groups[location.group_index] else {
        panic!("fixture location should identify an exchange");
    };
    let Message::User { content } = &results[location.result_message_index] else {
        panic!("fixture location should identify a result message");
    };
    let UserContent::ToolResult(result) = content
        .iter()
        .nth(location.content_index)
        .expect("fixture content index should exist")
    else {
        panic!("fixture location should identify a tool result");
    };
    result
}

pub(super) struct FixedMarkerCounter {
    tokens: u64,
}

impl FixedMarkerCounter {
    pub(super) const fn new(tokens: u64) -> Self {
        Self { tokens }
    }
}

#[async_trait]
impl ArtifactTokenCounter for FixedMarkerCounter {
    async fn count(&self, _result: &ToolResult) -> Result<u64, ArtifactTokenCounterError> {
        Ok(self.tokens)
    }
}
