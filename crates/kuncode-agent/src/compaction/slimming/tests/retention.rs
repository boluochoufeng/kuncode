use async_trait::async_trait;
use kuncode_core::completion::ToolResult;

use super::{FixedMarkerCounter, artifact_source, exchange, location};
use crate::{
    compaction::{
        artifact::{ArtifactTokenCounter, ArtifactTokenCounterError},
        protocol::{group_messages, select_protected_recent_tail},
        slimming::{SlimmingOutcome, SlimmingRetention, slim_tool_results},
    },
    session_store::Seq,
    tool::ToolOutput,
};

#[tokio::test]
async fn failed_output_remains_verbatim() {
    // Given: a failed old tool result that artifact classified below threshold.
    let payload = ToolOutput::<serde_json::Value>::failure("non_zero_exit", "tests failed")
        .to_model_content();

    // When: an explicit policy still offers it to slimming.
    let result = slim_payload(&payload, &FixedMarkerCounter::new(100)).await;

    // Then: the conservative pass keeps failure evidence exact.
    assert_retained(&result, SlimmingRetention::FailedOutput);
}

#[tokio::test]
async fn truncated_output_remains_verbatim() {
    // Given: a successful but already truncated old tool result.
    let payload = ToolOutput::success(serde_json::json!({"body": "partial"}))
        .truncated()
        .to_model_content();

    // When: an explicit policy offers it to slimming.
    let result = slim_payload(&payload, &FixedMarkerCounter::new(100)).await;

    // Then: the pass does not compound unknown omission semantics.
    assert_retained(&result, SlimmingRetention::TruncatedOutput);
}

#[tokio::test]
async fn malformed_output_remains_verbatim() {
    // Given: an old result that is not a structured harness ToolOutput.
    let payload = "not-json";

    // When: the stale policy location reaches the deterministic pass.
    let result = slim_payload(payload, &FixedMarkerCounter::new(100)).await;

    // Then: parse failure is isolated and the original stays inline.
    assert_retained(&result, SlimmingRetention::Parse);
}

#[tokio::test]
async fn marker_count_failure_remains_verbatim() {
    // Given: a valid old result and an unavailable provider counter.
    let payload = ToolOutput::success(serde_json::json!({"body": "large"})).to_model_content();

    // When: provider-visible marker counting fails.
    let result = slim_payload(&payload, &FailingCounter).await;

    // Then: count failure is isolated and the original stays inline.
    assert_retained(&result, SlimmingRetention::Count);
}

#[tokio::test]
async fn marker_metadata_over_provider_limit_remains_verbatim() {
    let payload = ToolOutput::success(serde_json::json!({"body": "large"})).to_model_content();

    let result = slim_payload(&payload, &FixedMarkerCounter::new(3_000)).await;

    assert_retained(&result, SlimmingRetention::MarkerTooLarge);
}

async fn slim_payload(payload: &str, counter: &dyn ArtifactTokenCounter) -> Vec<SlimmingOutcome> {
    let messages = [
        exchange(
            "old",
            "read_file",
            serde_json::json!({"path": "old"}),
            payload,
        ),
        exchange(
            "recent",
            "read_file",
            serde_json::json!({"path": "new"}),
            "recent",
        ),
    ]
    .concat();
    let groups = group_messages(&messages).expect("fixture should contain closed exchanges");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1)
        .expect("fixture should have a protected tail");
    let source = artifact_source(&groups, &[(location(0), "old", 1_000, Some(Seq::new(2)))]);
    let result = slim_tool_results(&source, &protected, &[location(0)], counter)
        .await
        .expect("isolated retention should not fail the pass");
    result.outcomes().to_vec()
}

fn assert_retained(outcomes: &[SlimmingOutcome], reason: SlimmingRetention) {
    assert_eq!(
        outcomes,
        &[SlimmingOutcome::Retained {
            location: location(0),
            reason,
        }]
    );
}

struct FailingCounter;

#[async_trait]
impl ArtifactTokenCounter for FailingCounter {
    async fn count(&self, _result: &ToolResult) -> Result<u64, ArtifactTokenCounterError> {
        Err(ArtifactTokenCounterError::provider("unavailable"))
    }
}
