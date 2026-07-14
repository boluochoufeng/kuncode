use super::*;
use kuncode_agent::compaction::budget::TokenCountPrecision;

#[test]
fn compaction_status_and_completion_render_without_token_values() {
    // Given
    let mut app = App::new("model", PermissionMode::Default);
    app.status = Status::Running;
    app.apply_event(EventKind::CompactionStarted {
        reason: "soft_threshold".to_string(),
        before_tokens: 98_765,
        precision: TokenCountPrecision::Exact,
    });
    let mut terminal = Terminal::new(TestBackend::new(60, 12)).expect("test terminal");

    // When
    terminal.draw(|frame| draw(frame, &mut app)).expect("draw");

    // Then
    let rendered = format!("{}", terminal.backend());
    assert!(rendered.contains("正在压缩上下文"));
    assert!(!rendered.contains("98765"));

    // When
    app.apply_event(EventKind::CompactionCompleted {
        before_tokens: 98_765,
        after_tokens: 12_345,
        target_reached: true,
        passes: vec!["semantic_summary".to_string(), "atomic_commit".to_string()],
        source_seq_start: 1,
        source_seq_end: 10,
        checkpoint_seq: 11,
        artifact_count: 0,
        summary_usage: None,
        summary_latency_ms: Some(50),
        latency_ms: 80,
    });
    terminal.draw(|frame| draw(frame, &mut app)).expect("draw");

    // Then
    let rendered = format!("{}", terminal.backend());
    assert!(rendered.contains("上下文已压缩"));
    assert!(!rendered.contains("98765"));
    assert!(!rendered.contains("12345"));
}
