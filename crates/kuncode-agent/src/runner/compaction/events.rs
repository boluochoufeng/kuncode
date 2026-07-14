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

fn failure_code(error: &CompactionError) -> &'static str {
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
        CompactionError::NoSafeBoundary
        | CompactionError::InsufficientReduction
        | CompactionError::AboveSoftThreshold
        | CompactionError::StaleActiveContext
        | CompactionError::ProtectedTailChanged
        | CompactionError::InvalidLineage => "validation_failed",
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
mod tests {
    use kuncode_core::completion::CompletionError;

    use super::*;
    use crate::{
        compaction::{budget::TokenEstimate, summary::SummarizerError},
        session_store::SessionStoreError,
    };

    #[test]
    fn provider_failure_event_excludes_raw_response_body() {
        // Given
        let secret = "provider-secret-response-body";
        let error =
            CompactionError::Summary(SummarizerError::Completion(CompletionError::ApiError {
                status: 500,
                message: secret.to_string(),
            }));

        // When
        let event = failure_event(&error, BudgetLevel::Soft, soft_budget(), Instant::now());
        let serialized = serde_json::to_string(&event).expect("event should serialize");

        // Then
        assert!(matches!(
            event,
            EventKind::CompactionFailed {
                error,
                recoverable: true,
                ..
            } if error == "summary_failed"
        ));
        assert!(!serialized.contains(secret));
    }

    #[test]
    fn unknown_commit_event_is_not_recoverable_under_soft_pressure() {
        // Given
        let error = CompactionError::Store(SessionStoreError::CommitOutcomeUnknown {
            operation: "compaction",
            message: "ambiguous receipt".to_string(),
        });

        // When
        let event = failure_event(&error, BudgetLevel::Soft, soft_budget(), Instant::now());

        // Then
        assert!(matches!(
            event,
            EventKind::CompactionFailed {
                error,
                recoverable: false,
                ..
            } if error == "persistence_outcome_unknown"
        ));
    }

    #[test]
    fn mismatched_artifact_receipt_is_not_recoverable_under_soft_pressure() {
        let error = CompactionError::Artifact(
            crate::compaction::artifact::ArtifactSpillError::ReceiptMismatch,
        );

        let event = failure_event(&error, BudgetLevel::Soft, soft_budget(), Instant::now());

        assert!(matches!(
            event,
            EventKind::CompactionFailed {
                error,
                recoverable: false,
                ..
            } if error == "artifact_receipt_mismatch"
        ));
    }

    #[test]
    fn journal_head_conflict_is_not_recoverable_under_soft_pressure() {
        // Given
        let error = CompactionError::Store(SessionStoreError::JournalHeadConflict {
            expected: 7,
            actual: 8,
        });

        // When
        let event = failure_event(&error, BudgetLevel::Soft, soft_budget(), Instant::now());

        // Then
        assert!(matches!(
            event,
            EventKind::CompactionFailed {
                error,
                recoverable: false,
                ..
            } if error == "journal_head_conflict"
        ));
    }

    #[test]
    fn journal_audit_integrity_failures_are_not_recoverable_under_soft_pressure() {
        let failures = [
            crate::compaction::artifact::ArtifactSpillError::JournalFrontierStale {
                frontier: 7,
                actual: 8,
            },
            crate::compaction::artifact::ArtifactSpillError::JournalMessageMismatch { index: 2 },
            crate::compaction::artifact::ArtifactSpillError::JournalIntegrity(
                "invalid payload".to_string(),
            ),
            crate::compaction::artifact::ArtifactSpillError::ArtifactIntegrity(
                "invalid binding".to_string(),
            ),
        ];

        for failure in failures {
            assert!(!is_recoverable(
                &CompactionError::Artifact(failure),
                BudgetLevel::Soft
            ));
        }
        assert!(!is_recoverable(
            &CompactionError::InvalidLineage,
            BudgetLevel::Soft
        ));
        assert!(!is_recoverable(
            &CompactionError::SummarySource(
                crate::session::SummarySourceError::MissingMessageProvenance { message_index: 0 },
            ),
            BudgetLevel::Soft,
        ));
    }

    #[test]
    fn rejected_summary_usage_is_preserved_in_failure_telemetry() {
        // Given
        let usage = kuncode_core::completion::Usage {
            input_tokens: 21,
            output_tokens: 8,
            total_tokens: 29,
            ..kuncode_core::completion::Usage::default()
        };
        let error = CompactionError::Summary(SummarizerError::InvalidResponseShape { usage });

        // When
        let event = failure_event(&error, BudgetLevel::Soft, soft_budget(), Instant::now());

        // Then
        assert!(matches!(
            event,
            EventKind::CompactionFailed {
                summary_usage: Some(observed),
                ..
            } if observed == usage
        ));
    }

    fn soft_budget() -> ContextBudget {
        ContextBudget::new(
            1_000,
            TokenEstimate::new(700, crate::compaction::budget::TokenCountPrecision::Exact),
            100,
            0,
        )
        .expect("test budget should be valid")
    }
}
