//! Read-only planning for shadow compaction rollout.

use kuncode_core::completion::{Message, ToolResultContent, UserContent};

use crate::{
    compaction::{
        CompactionError, GroupTokenEstimator,
        artifact::ArtifactTokenCounter,
        budget::{CompactionConfig, ContextBudget},
        protocol::{ProtocolGroup, group_messages, select_protected_recent_tail_from_estimates},
    },
    tool::ToolOutput,
};

const ARTIFACT_THRESHOLD_TOKENS: u64 = 8_192;
const MARKER_LIMIT_TOKENS: u64 = 2_048;

pub(super) struct ShadowReport {
    pub(super) projected_after_tokens: u64,
    pub(super) safe_prefix_groups: usize,
    pub(super) artifact_shape_candidates: usize,
    pub(super) requires_summary: bool,
}

pub(super) async fn observe(
    messages: &[Message],
    config: &CompactionConfig,
    before: ContextBudget,
    group_estimator: &dyn GroupTokenEstimator,
    artifact_counter: &dyn ArtifactTokenCounter,
) -> Result<ShadowReport, CompactionError> {
    let groups = group_messages(messages)?;
    let mut estimates = Vec::with_capacity(groups.len());
    for group in &groups {
        estimates.push(group_estimator.estimate(group).await?);
    }
    let recent_tokens = ratio_tokens(before.usable_input_limit(), config.recent_ratio());
    let protected = select_protected_recent_tail_from_estimates(&groups, recent_tokens, &estimates)
        .ok_or(CompactionError::NoSafeBoundary)?;
    let mut artifact_shape_candidates = 0;
    let mut minimum_savings = 0_u64;
    for group in &groups[..protected.group_range.start] {
        let ProtocolGroup::ToolExchange { results, .. } = group else {
            continue;
        };
        for message in results {
            let Message::User { content } = message else {
                continue;
            };
            for block in content.iter() {
                let UserContent::ToolResult(result) = block else {
                    continue;
                };
                if result.content.len() != 1 {
                    continue;
                }
                let payload = match result.content.first() {
                    ToolResultContent::Text(text) => text.text_ref(),
                };
                if serde_json::from_str::<ToolOutput>(payload).is_err() {
                    continue;
                }
                let Ok(tokens) = artifact_counter.count(result).await else {
                    continue;
                };
                if tokens <= ARTIFACT_THRESHOLD_TOKENS {
                    continue;
                }
                artifact_shape_candidates += 1;
                minimum_savings =
                    minimum_savings.saturating_add(tokens.saturating_sub(MARKER_LIMIT_TOKENS));
            }
        }
    }
    let projected_after_tokens = before.current_input().saturating_sub(minimum_savings);
    let target = ratio_tokens(before.usable_input_limit(), config.target_ratio()).max(1);
    Ok(ShadowReport {
        projected_after_tokens,
        safe_prefix_groups: protected.group_range.start,
        artifact_shape_candidates,
        requires_summary: projected_after_tokens > target,
    })
}

fn ratio_tokens(limit: u64, ratio: f64) -> u64 {
    (limit as f64 * ratio).floor() as u64
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use kuncode_core::{
        completion::{AssistantContent, ToolResult},
        non_empty_vec::NonEmptyVec,
    };

    use super::*;
    use crate::compaction::{
        artifact::ArtifactTokenCounterError,
        budget::{CompactionMode, TokenCountPrecision, TokenEstimate},
    };

    struct FixedGroups;

    #[async_trait]
    impl GroupTokenEstimator for FixedGroups {
        async fn estimate(&self, _group: &ProtocolGroup) -> Result<u64, CompactionError> {
            Ok(800)
        }
    }

    struct ByIdCounter;

    #[async_trait]
    impl ArtifactTokenCounter for ByIdCounter {
        async fn count(&self, result: &ToolResult) -> Result<u64, ArtifactTokenCounterError> {
            Ok(if result.id == "old" { 9_000 } else { 10 })
        }
    }

    #[tokio::test]
    async fn old_large_result_produces_a_read_only_shadow_candidate() {
        // Given
        let messages = [tool_exchange("old"), tool_exchange("recent")].concat();
        let config = CompactionConfig::new(CompactionMode::Shadow, 10_000, 0, 0)
            .expect("shadow config should be valid");
        let before = ContextBudget::new(
            10_000,
            TokenEstimate::new(9_000, TokenCountPrecision::Exact),
            0,
            0,
        )
        .expect("budget should be valid");

        // When
        let report = observe(&messages, &config, before, &FixedGroups, &ByIdCounter)
            .await
            .expect("shadow planning should succeed");

        // Then
        assert_eq!(report.safe_prefix_groups, 1);
        assert_eq!(report.artifact_shape_candidates, 1);
        assert_eq!(report.projected_after_tokens, 2_048);
        assert!(!report.requires_summary);
    }

    fn tool_exchange(id: &str) -> Vec<Message> {
        let output = ToolOutput::success(serde_json::json!({"result": id})).to_model_content();
        vec![
            Message::Assistant {
                id: None,
                content: NonEmptyVec::new(AssistantContent::tool_call(
                    id,
                    "test_tool",
                    serde_json::json!({}),
                )),
            },
            Message::tool_result(id, output),
        ]
    }
}
