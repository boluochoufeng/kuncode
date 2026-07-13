//! Deny-by-default production authorization for lossy tool-result projection.

use crate::compaction::{
    artifact::{ArtifactResultLocation, ArtifactSpillResult},
    protocol::ProtectedRecentTail,
};
use crate::tool::ToolResultRetention;

/// Returns candidates proven safe for lossy projection in production.
///
/// Authorization comes only from harness-owned lineage minted by the concrete
/// tool implementation. Imported or derived messages default to verbatim, so
/// tool names and model-supplied arguments never grant this capability.
pub(crate) fn production_slimming_candidates(
    source: &ArtifactSpillResult,
    protected: &ProtectedRecentTail,
) -> Vec<ArtifactResultLocation> {
    source
        .outcomes()
        .iter()
        .filter_map(|outcome| match outcome {
            crate::compaction::artifact::ArtifactSpillOutcome::BelowThreshold(candidate)
                if candidate.location().group_index < protected.group_range.start
                    && candidate.retention() == ToolResultRetention::Slimmable =>
            {
                Some(candidate.location())
            }
            crate::compaction::artifact::ArtifactSpillOutcome::BelowThreshold(_)
            | crate::compaction::artifact::ArtifactSpillOutcome::Failed { .. }
            | crate::compaction::artifact::ArtifactSpillOutcome::Spilled { .. } => None,
        })
        .collect()
}
