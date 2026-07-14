//! Stable, payload-free compaction telemetry and failure-policy classification.
//!
//! Error codes deliberately exclude provider bodies, message text, artifact
//! contents, and persistence details that could contain user data.

use std::time::Instant;

use crate::{
    compaction::{
        CompactionError,
        budget::{BudgetLevel, ContextBudget},
    },
    observer::EventKind,
};

pub(super) fn pressure_reason(level: BudgetLevel) -> &'static str {
    match level {
        BudgetLevel::Soft => "soft_threshold",
        BudgetLevel::Hard => "hard_threshold",
        BudgetLevel::Normal => "below_soft_threshold",
    }
}

pub(super) fn failure_event(
    error: &CompactionError,
    level: BudgetLevel,
    before: ContextBudget,
    started: Instant,
) -> EventKind {
    EventKind::CompactionFailed {
        stage: failure_stage(error).to_string(),
        error: failure_code(error).to_string(),
        recoverable: is_recoverable(error, level),
        before_tokens: before.current_input(),
        summary_usage: match error {
            CompactionError::Summary(error) => error.usage(),
            CompactionError::NonDurableSession
            | CompactionError::InvalidThresholds
            | CompactionError::NoSafeBoundary
            | CompactionError::InsufficientReduction
            | CompactionError::AboveSoftThreshold
            | CompactionError::StaleActiveContext
            | CompactionError::ProtectedTailChanged
            | CompactionError::InvalidLineage
            | CompactionError::Budget(_)
            | CompactionError::TokenEstimation(_)
            | CompactionError::Projection(_)
            | CompactionError::Protocol(_)
            | CompactionError::Artifact(_)
            | CompactionError::Slimming(_)
            | CompactionError::Selection(_)
            | CompactionError::SummarySource(_)
            | CompactionError::Encoding(_)
            | CompactionError::Store(_)
            | CompactionError::Apply(_) => None,
        },
        latency_ms: elapsed_ms(started),
    }
}

pub(super) fn is_recoverable(error: &CompactionError, level: BudgetLevel) -> bool {
    // Soft pressure permits fallback only when the durable proof remains valid.
    level == BudgetLevel::Soft && !must_fail_closed(error)
}

pub(super) fn failure_message(error: &CompactionError) -> String {
    format!("context compaction failed: {}", failure_code(error))
}

fn must_fail_closed(error: &CompactionError) -> bool {
    invalidates_persistence_authority(error)
}

pub(super) fn invalidates_persistence_authority(error: &CompactionError) -> bool {
    // Keep this exhaustive: a new error variant must explicitly declare whether
    // the session may continue trusting its journal-to-memory relationship.
    match error {
        CompactionError::StaleActiveContext
        | CompactionError::InvalidLineage
        | CompactionError::SummarySource(_)
        | CompactionError::Apply(_) => true,
        CompactionError::Artifact(error) => error.invalidates_persistence_authority(),
        CompactionError::Store(error) => error.invalidates_compaction_authority(),
        CompactionError::NonDurableSession
        | CompactionError::InvalidThresholds
        | CompactionError::NoSafeBoundary
        | CompactionError::InsufficientReduction
        | CompactionError::AboveSoftThreshold
        | CompactionError::ProtectedTailChanged
        | CompactionError::Budget(_)
        | CompactionError::TokenEstimation(_)
        | CompactionError::Projection(_)
        | CompactionError::Protocol(_)
        | CompactionError::Slimming(_)
        | CompactionError::Selection(_)
        | CompactionError::Summary(_)
        | CompactionError::Encoding(_) => false,
    }
}

pub(super) fn failure_code(error: &CompactionError) -> &'static str {
    match error {
        CompactionError::Artifact(
            crate::compaction::artifact::ArtifactSpillError::PersistenceOutcomeUnknown { .. },
        )
        | CompactionError::Store(crate::session_store::SessionStoreError::CommitOutcomeUnknown {
            ..
        }) => "persistence_outcome_unknown",
        CompactionError::Artifact(
            crate::compaction::artifact::ArtifactSpillError::ReceiptMismatch,
        ) => "artifact_receipt_mismatch",
        CompactionError::Artifact(
            crate::compaction::artifact::ArtifactSpillError::JournalHeadConflict { .. },
        )
        | CompactionError::Store(crate::session_store::SessionStoreError::JournalHeadConflict {
            ..
        }) => "journal_head_conflict",
        CompactionError::Artifact(
            crate::compaction::artifact::ArtifactSpillError::JournalFrontierStale { .. },
        ) => "journal_frontier_stale",
        CompactionError::Artifact(
            crate::compaction::artifact::ArtifactSpillError::JournalMessageMismatch { .. },
        ) => "journal_message_mismatch",
        CompactionError::Artifact(
            crate::compaction::artifact::ArtifactSpillError::JournalIntegrity(_),
        ) => "journal_integrity_failed",
        CompactionError::Artifact(
            crate::compaction::artifact::ArtifactSpillError::ArtifactIntegrity(_),
        ) => "artifact_integrity_failed",
        CompactionError::NonDurableSession | CompactionError::Store(_) => "persistence_failed",
        CompactionError::Budget(_)
        | CompactionError::TokenEstimation(_)
        | CompactionError::Projection(_)
        | CompactionError::InvalidThresholds => "budget_failed",
        CompactionError::Protocol(_) | CompactionError::Selection(_) => "protocol_failed",
        CompactionError::Artifact(_) | CompactionError::Slimming(_) => "artifact_failed",
        CompactionError::SummarySource(_) | CompactionError::Summary(_) => "summary_failed",
        CompactionError::Encoding(_) => "checkpoint_failed",
        CompactionError::Apply(_) => "apply_failed",
        CompactionError::NoSafeBoundary => "no_safe_boundary",
        CompactionError::InsufficientReduction => "insufficient_reduction",
        CompactionError::AboveSoftThreshold => "above_soft_threshold",
        CompactionError::StaleActiveContext => "stale_active_context",
        CompactionError::ProtectedTailChanged => "protected_tail_changed",
        CompactionError::InvalidLineage => "invalid_lineage",
    }
}

fn failure_stage(error: &CompactionError) -> &'static str {
    match error {
        CompactionError::NonDurableSession | CompactionError::Store(_) => "persistence",
        CompactionError::Budget(_)
        | CompactionError::TokenEstimation(_)
        | CompactionError::Projection(_)
        | CompactionError::InvalidThresholds => "budget",
        CompactionError::Protocol(_) | CompactionError::Selection(_) => "protocol",
        CompactionError::Artifact(_) | CompactionError::Slimming(_) => "artifact",
        CompactionError::SummarySource(_) | CompactionError::Summary(_) => "summary",
        CompactionError::Encoding(_) => "checkpoint",
        CompactionError::Apply(_) => "apply",
        CompactionError::NoSafeBoundary
        | CompactionError::InsufficientReduction
        | CompactionError::AboveSoftThreshold
        | CompactionError::StaleActiveContext
        | CompactionError::ProtectedTailChanged
        | CompactionError::InvalidLineage => "validation",
    }
}

pub(super) fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
