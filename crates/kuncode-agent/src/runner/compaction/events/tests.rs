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
    let error = CompactionError::Summary(SummarizerError::Completion(CompletionError::ApiError {
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
fn validation_failures_expose_distinct_safe_codes() {
    // Given
    let cases = [
        (CompactionError::NoSafeBoundary, "no_safe_boundary"),
        (
            CompactionError::InsufficientReduction,
            "insufficient_reduction",
        ),
        (CompactionError::AboveSoftThreshold, "above_soft_threshold"),
        (CompactionError::StaleActiveContext, "stale_active_context"),
        (
            CompactionError::ProtectedTailChanged,
            "protected_tail_changed",
        ),
        (CompactionError::InvalidLineage, "invalid_lineage"),
    ];

    for (failure, expected) in cases {
        // When
        let event = failure_event(&failure, BudgetLevel::Soft, soft_budget(), Instant::now());

        // Then
        assert!(matches!(
            event,
            EventKind::CompactionFailed { error, .. } if error == expected
        ));
        assert_eq!(
            failure_message(&failure),
            format!("context compaction failed: {expected}")
        );
    }
}

#[test]
fn terminal_compaction_error_adds_the_human_prefix_once() {
    // Given
    let failure = CompactionError::NoSafeBoundary;

    // When
    let rendered = super::super::compaction_error(failure).to_string();

    // Then
    assert_eq!(rendered, "context compaction failed: no_safe_boundary");
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
    let error =
        CompactionError::Artifact(crate::compaction::artifact::ArtifactSpillError::ReceiptMismatch);

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
