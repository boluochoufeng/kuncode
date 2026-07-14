//! Candidate-only slimming outcomes, retention reasons, and boundary errors.
//!
//! These types describe a projection of active context; durable journal entries
//! remain unchanged and are the authority behind any installed marker.

use thiserror::Error;

use crate::{
    compaction::{
        artifact::{ArtifactResultLocation, ArtifactSpillResult},
        protocol::ProtocolGroup,
    },
    session_store::Seq,
};

/// Structured outcome for one caller-authorized candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlimmingOutcome {
    /// The result was replaced with a bounded, strictly cheaper marker.
    Slimmed {
        /// Exact projected block.
        location: ArtifactResultLocation,
        /// Durable source sequence copied into the marker.
        original_journal_seq: Seq,
        /// Provider-visible cost measured before projection.
        original_tokens: u64,
        /// Provider-visible cost of the installed marker.
        slimmed_tokens: u64,
    },
    /// The original result remained exact.
    Retained {
        /// Exact block retained verbatim.
        location: ArtifactResultLocation,
        /// Deterministic reason that prevented a lossy replacement.
        reason: SlimmingRetention,
    },
}

/// Conservative per-item retention reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlimmingRetention {
    /// Checkpoint baselines do not expose original per-message journal lineage.
    MissingProvenance,
    /// Payload is not structured harness output.
    Parse,
    /// Failed outputs remain exact because current work may depend on them.
    FailedOutput,
    /// Truncated outputs remain exact because their omission semantics matter.
    TruncatedOutput,
    /// Provider-visible marker counting failed.
    Count,
    /// The bounded marker would not strictly reduce provider-visible tokens.
    NoSavings,
    /// Fixed marker metadata exceeds the provider-visible marker limit.
    MarkerTooLarge,
}

/// Candidate-only groups produced by the slimming pass.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolResultSlimmingResult<'source> {
    pub(super) source: &'source ArtifactSpillResult,
    pub(super) groups: Vec<ProtocolGroup>,
    pub(super) outcomes: Vec<SlimmingOutcome>,
}

impl ToolResultSlimmingResult<'_> {
    /// Returns projected groups without mutating durable history.
    pub fn groups(&self) -> &[ProtocolGroup] {
        &self.groups
    }

    /// Returns one decision for every authorized candidate.
    pub fn outcomes(&self) -> &[SlimmingOutcome] {
        &self.outcomes
    }

    pub(crate) const fn source(&self) -> &ArtifactSpillResult {
        self.source
    }
}

/// Boundary defects in an explicit slimming policy.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ToolResultSlimmingError {
    /// The protected range is not a suffix of the supplied groups.
    #[error("protected recent tail is not a suffix of slimming groups")]
    InvalidProtectedTail,
    /// Supplied groups are not canonical closed protocol groups.
    #[error("slimming requires canonical closed protocol groups")]
    InvalidProtocolGroups,
    /// A policy entry attempts to rewrite protected context.
    #[error("slimming candidate at group {group_index} is protected")]
    ProtectedCandidate {
        /// Protected group targeted by the caller policy.
        group_index: usize,
    },
    /// A policy entry does not identify a tool-result block.
    #[error("slimming candidate does not identify a tool result: {location:?}")]
    InvalidCandidate {
        /// Position that does not resolve to a canonical tool result.
        location: ArtifactResultLocation,
    },
    /// More than one policy entry targets the same block.
    #[error("slimming policy repeats candidate: {location:?}")]
    DuplicateCandidate {
        /// Position repeated by the caller policy.
        location: ArtifactResultLocation,
    },
    /// Artifact sidecar and active candidate disagree at the same location.
    #[error("artifact sidecar does not match active tool result: {location:?}")]
    ArtifactSidecarMismatch {
        /// Position whose active call id differs from the artifact decision.
        location: ArtifactResultLocation,
    },
    /// Artifact pass did not classify the result as below threshold.
    #[error("slimming requires a below-threshold artifact disposition")]
    IneligibleArtifactDisposition,
}
