//! Durable-first spilling of old tool results into compact markers.

mod audit;
mod boundary;
mod hash;
mod marker;
mod preview;
mod spill;
mod types;

pub use boundary::{ArtifactSpillError, ArtifactSpillInput};
pub(crate) use hash::tool_result_hash;
pub(crate) use preview::adaptive_preview;
pub use spill::spill_artifacts;
pub use types::{
    ArtifactResultLocation, ArtifactSpillFailure, ArtifactSpillOutcome, ArtifactSpillResult,
    ArtifactStore, ArtifactTokenCounter, ArtifactTokenCounterError, BelowThresholdArtifact,
};

#[cfg(test)]
pub(crate) fn fixture_below_threshold(
    location: ArtifactResultLocation,
    tool_call_id: String,
    tokens: u64,
    source_hash: String,
    source_journal_seq: Option<crate::session_store::Seq>,
) -> ArtifactSpillOutcome {
    ArtifactSpillOutcome::BelowThreshold(BelowThresholdArtifact::new(
        location,
        tool_call_id,
        tokens,
        source_hash,
        source_journal_seq,
        crate::tool::ToolResultRetention::Slimmable,
    ))
}

#[cfg(test)]
pub(crate) fn fixture_spill_result(
    groups: Vec<crate::compaction::protocol::ProtocolGroup>,
    frontier: crate::session_store::Seq,
    outcomes: Vec<ArtifactSpillOutcome>,
) -> ArtifactSpillResult {
    ArtifactSpillResult::new(
        crate::session_store::SessionId::new("artifact-fixture"),
        groups,
        frontier,
        outcomes,
    )
}

#[cfg(test)]
pub(crate) fn fixture_spill_result_for_session(
    session_id: crate::session_store::SessionId,
    groups: Vec<crate::compaction::protocol::ProtocolGroup>,
    frontier: crate::session_store::Seq,
    outcomes: Vec<ArtifactSpillOutcome>,
) -> ArtifactSpillResult {
    ArtifactSpillResult::new(session_id, groups, frontier, outcomes)
}

#[cfg(test)]
mod tests;
