use super::*;
use kuncode_agent::compaction::budget::TokenCountPrecision;

#[test]
fn compaction_events_drive_tui_only_state_without_numeric_log_data() {
    // Given
    let mut app = app();
    app.status = Status::Running;

    // When
    app.apply_event(compaction_started());

    // Then
    assert_eq!(app.status, Status::Compacting);
    assert!(app.conversation.is_empty());

    // When
    app.apply_event(compaction_completed());

    // Then
    assert_eq!(app.status, Status::Running);
    assert!(matches!(app.conversation.as_slice(), [Item::Compaction]));
}

#[test]
fn compaction_failure_and_turn_error_clear_the_transient_state() {
    // Given
    let mut app = app();
    app.status = Status::Running;
    app.apply_event(compaction_started());

    // When
    app.apply_event(EventKind::CompactionFailed {
        stage: "validation".to_string(),
        error: "no_safe_boundary".to_string(),
        recoverable: true,
        before_tokens: 42_000,
        summary_usage: None,
        latency_ms: 10,
    });

    // Then
    assert_eq!(app.status, Status::Running);
    assert!(app.conversation.is_empty());

    // When
    app.apply_event(compaction_started());
    app.push_error("已取消".to_string());

    // Then
    assert_eq!(app.status, Status::Running);
}

fn compaction_started() -> EventKind {
    EventKind::CompactionStarted {
        reason: "soft_threshold".to_string(),
        before_tokens: 42_000,
        precision: TokenCountPrecision::Exact,
    }
}

fn compaction_completed() -> EventKind {
    EventKind::CompactionCompleted {
        before_tokens: 42_000,
        after_tokens: 18_000,
        target_reached: true,
        passes: vec!["semantic_summary".to_string(), "atomic_commit".to_string()],
        source_seq_start: 1,
        source_seq_end: 10,
        checkpoint_seq: 11,
        artifact_count: 0,
        summary_usage: None,
        summary_latency_ms: Some(50),
        latency_ms: 80,
    }
}
