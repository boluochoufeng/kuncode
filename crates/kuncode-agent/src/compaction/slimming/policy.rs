//! Deny-by-default production authorization for lossy tool-result projection.

use crate::compaction::{
    artifact::{ArtifactResultLocation, ArtifactSpillResult},
    protocol::ProtectedRecentTail,
};
use crate::tool::ToolResultRetention;

/// Returns candidates proven safe for lossy projection in production.
///
/// Authorization requires a below-threshold artifact decision outside the
/// protected suffix and `Slimmable` retention minted by the concrete tool after
/// live execution. Imported or derived messages default to verbatim, so tool
/// names and model-supplied arguments never grant this capability. Marker
/// preparation separately requires durable lineage, successful non-truncated
/// output, a fixed size cap, and strict token savings.
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
