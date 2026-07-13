use kuncode_core::{
    completion::{Message, ToolResultContent, UserContent},
    non_empty_vec::NonEmptyVec,
};

use super::{FixedMarkerCounter, artifact_source, authorization_for, fixture_groups, location};
use crate::{
    compaction::{
        artifact::{ArtifactSpillFailure, ArtifactSpillOutcome, fixture_spill_result},
        protocol::{ProtocolGroup, select_protected_recent_tail},
        slimming::{
            SlimmingOutcome, SlimmingRetention, ToolResultSlimmingError, slim_tool_results,
        },
    },
    session_store::Seq,
    tool::ToolOutput,
};

#[tokio::test]
async fn retains_checkpoint_baseline_without_per_message_provenance() {
    let groups = fixture_groups();
    let protected = select_protected_recent_tail(&groups, 0, |_| 1)
        .expect("history should have a protected tail");
    let source = artifact_source(&groups, &[(location(0), "old", 1_000, None)]);

    let result = slim_tool_results(
        &source,
        &protected,
        &[location(0)],
        &FixedMarkerCounter::new(100),
    )
    .await
    .expect("missing lineage should retain the source");

    assert_eq!(result.groups(), groups);
    assert_eq!(
        result.outcomes(),
        &[SlimmingOutcome::Retained {
            location: location(0),
            reason: SlimmingRetention::MissingProvenance,
        }]
    );
}

#[tokio::test]
async fn rejects_authorization_bound_to_different_payload() {
    let original = fixture_groups();
    let authorization = authorization_for(&original, location(0), "old", 1_000, Some(Seq::new(2)));
    let mut changed = original;
    let ProtocolGroup::ToolExchange { results, .. } = &mut changed[0] else {
        panic!("fixture should start with an exchange");
    };
    let Message::User { content } = &mut results[0] else {
        panic!("fixture should contain a result message");
    };
    let mut blocks = content.clone().into_vec();
    let Some(UserContent::ToolResult(result)) = blocks.first_mut() else {
        panic!("fixture should contain a tool result");
    };
    result.content = NonEmptyVec::new(ToolResultContent::text(
        ToolOutput::success(serde_json::json!({"stdout": "different"})).to_model_content(),
    ));
    let Some((first, rest)) = blocks.split_first() else {
        panic!("fixture should retain one result block");
    };
    *content = NonEmptyVec::from_first_rest(first.clone(), rest.to_vec());
    let protected = select_protected_recent_tail(&changed, 0, |_| 1)
        .expect("history should have a protected tail");
    let source = fixture_spill_result(changed, Seq::new(99), vec![authorization]);

    assert_eq!(
        slim_tool_results(
            &source,
            &protected,
            &[location(0)],
            &FixedMarkerCounter::new(100),
        )
        .await,
        Err(ToolResultSlimmingError::ArtifactSidecarMismatch {
            location: location(0),
        })
    );
}

#[tokio::test]
async fn rejects_spilled_and_failed_artifact_dispositions() {
    let groups = fixture_groups();
    let protected = select_protected_recent_tail(&groups, 0, |_| 1)
        .expect("history should have a protected tail");
    let spilled = ArtifactSpillOutcome::Spilled {
        location: location(0),
        tool_call_id: "same".to_string(),
        artifact_id: "artifact".to_string(),
        journal_seq: Seq::new(9),
        original_tokens: 9_000,
    };
    let failed = ArtifactSpillOutcome::Failed {
        location: location(0),
        tool_call_id: "same".to_string(),
        failure: ArtifactSpillFailure::Parse("bad".to_string()),
    };
    let spilled_source = fixture_spill_result(groups.clone(), Seq::new(9), vec![spilled]);
    let failed_source = fixture_spill_result(groups, Seq::new(9), vec![failed]);

    let spilled_result = slim_tool_results(
        &spilled_source,
        &protected,
        &[location(0)],
        &FixedMarkerCounter::new(100),
    )
    .await;
    let failed_result = slim_tool_results(
        &failed_source,
        &protected,
        &[location(0)],
        &FixedMarkerCounter::new(100),
    )
    .await;

    assert_eq!(
        spilled_result,
        Err(ToolResultSlimmingError::IneligibleArtifactDisposition)
    );
    assert_eq!(
        failed_result,
        Err(ToolResultSlimmingError::IneligibleArtifactDisposition)
    );
}
