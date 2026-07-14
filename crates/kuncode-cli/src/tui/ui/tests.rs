use super::*;
use crate::tui::app::{Item, ToolState};
use kuncode_agent::compaction::budget::TokenCountPrecision;
use kuncode_agent::observer::EventKind;
use kuncode_agent::permission::PermissionMode;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

#[test]
fn renders_key_elements_without_panicking() {
    let mut app = App::new("model-x", PermissionMode::Default);
    app.push_user("hi".to_string());
    app.conversation.push(Item::Tool {
        id: "1".to_string(),
        name: "bash".to_string(),
        summary: "run ls".to_string(),
        state: ToolState::Ok { truncated: false },
    });
    app.push_assistant("done".to_string());

    let mut terminal = Terminal::new(TestBackend::new(60, 16)).expect("test terminal");
    terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
    // Scrolling must also render without panicking.
    app.scroll_up(5);
    terminal
        .draw(|frame| draw(frame, &mut app))
        .expect("draw after scroll");

    let rendered = format!("{}", terminal.backend());
    assert!(
        rendered.contains("model-x"),
        "status line should show model"
    );
    assert!(rendered.contains("Bash"), "tool call should be visible");
}

#[test]
fn streamed_preview_renders_answer_and_reasoning_below_the_log() {
    let mut app = App::new("model-x", PermissionMode::Default);
    app.apply_event(EventKind::ReasoningDelta {
        text: "weighing options".to_string(),
    });
    app.apply_event(EventKind::TextDelta {
        text: "partial answer".to_string(),
    });
    // Only the revealed prefix draws; a huge budget reveals everything for
    // this assertion (the typewriter pacing itself is tested in app.rs).
    app.advance_reveal(std::time::Duration::from_secs(1), 100_000);

    let mut terminal = Terminal::new(TestBackend::new(60, 16)).expect("test terminal");
    terminal.draw(|frame| draw(frame, &mut app)).expect("draw");

    let rendered = format!("{}", terminal.backend());
    assert!(
        rendered.contains("partial answer"),
        "in-progress answer should render live"
    );
    assert!(
        rendered.contains("weighing options"),
        "in-progress reasoning should render live"
    );
}

#[test]
fn plan_panel_renders_the_live_plan() {
    use kuncode_agent::todo::{TodoItem, TodoStatus};
    let mut app = App::new("m", PermissionMode::Default);
    // A long log: the plan panel must still show even when the log scrolls.
    for i in 0..30 {
        app.push_user(format!("line {i}"));
    }
    app.plan = vec![
        TodoItem {
            content: "First step".to_string(),
            active_form: "Doing first step".to_string(),
            status: TodoStatus::Completed,
        },
        TodoItem {
            content: "Second step".to_string(),
            active_form: "Doing second step".to_string(),
            status: TodoStatus::InProgress,
        },
    ];

    let mut terminal = Terminal::new(TestBackend::new(60, 20)).expect("test terminal");
    terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
    let rendered = format!("{}", terminal.backend());

    assert!(rendered.contains("任务计划"), "plan panel title shown");
    // The in_progress row shows the present-tense active_form, not content.
    assert!(
        rendered.contains("Doing second step"),
        "in_progress shows active_form"
    );
    assert!(
        rendered.contains("First step"),
        "completed row shows content"
    );
}

#[test]
fn plan_panel_hides_once_every_task_is_completed() {
    use kuncode_agent::todo::{TodoItem, TodoStatus};
    let mut app = App::new("m", PermissionMode::Default);
    app.plan = vec![
        TodoItem {
            content: "First step".to_string(),
            active_form: "Doing first step".to_string(),
            status: TodoStatus::Completed,
        },
        TodoItem {
            content: "Second step".to_string(),
            active_form: "Doing second step".to_string(),
            status: TodoStatus::Completed,
        },
    ];

    let mut terminal = Terminal::new(TestBackend::new(60, 20)).expect("test terminal");
    terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
    let rendered = format!("{}", terminal.backend());

    // All tasks done → the panel collapses, so its title is gone.
    assert!(
        !rendered.contains("任务计划"),
        "an all-completed plan hides the panel"
    );
}

#[test]
fn caret_position_agrees_with_char_wrap() {
    // Exact fill stays on row 0 — no phantom next row that would blank the box.
    assert_eq!(caret_position("abcd", 4), (0, 4));
    // Overflow advances a row.
    assert_eq!(caret_position("abcde", 4), (1, 1));
    // Spaces don't get special word-break treatment: char-wrap, same as render.
    assert_eq!(caret_position("word word", 4), (2, 1));
    // Wide (CJK) glyphs: width 3 fits one per row.
    assert_eq!(caret_position("你你你", 3), (2, 2));
    // Explicit newline starts a fresh row.
    assert_eq!(caret_position("ab\nc", 4), (1, 1));
}

#[test]
fn caret_row_never_exceeds_rendered_rows() {
    // The caret's row must stay within the wrapped line count, or scroll would
    // blank the box. Check against the actual `wrap_lines` output.
    for input in ["", "abcd", "abcde", "word word", "你你你", "a\nbb\nccc"] {
        let (row, _) = caret_position(input, 4);
        let logical: Vec<Line> = input
            .split('\n')
            .map(|s| Line::raw(s.to_string()))
            .collect();
        let rendered = wrap_lines(logical, 4).len() as u16;
        assert!(
            row < rendered,
            "{input:?}: caret row {row} >= {rendered} rows"
        );
    }
}

#[test]
fn cursor_renders_at_the_edit_position_not_the_end() {
    let mut app = App::new("m", PermissionMode::Default);
    for c in "hello".chars() {
        app.insert_char(c);
    }
    app.move_left();
    app.move_left(); // cursor between the two 'l's → column 3

    let mut terminal = Terminal::new(TestBackend::new(40, 10)).expect("terminal");
    terminal.draw(|frame| draw(frame, &mut app)).expect("draw");

    // Full-width box at x=0: caret column 3 renders at x = 0 + border(1) + 3.
    // Were the caret still pinned to the input's end it would sit at x=6.
    let pos = terminal.get_cursor_position().expect("cursor position");
    assert_eq!(
        pos.x, 4,
        "cursor sits at the edit column, not the input end"
    );
}

#[test]
fn tool_names_display_as_pascal_case() {
    assert_eq!(display_tool_name("bash"), "Bash");
    assert_eq!(display_tool_name("read_file"), "ReadFile");
    assert_eq!(display_tool_name("glob"), "Glob");
}

#[test]
fn wraps_to_exact_lines_and_pads_full_width() {
    // "abcdef" at width 4 → "abcd" + "ef", each padded back out to width 4 so
    // a line background would fill the row.
    let wrapped = wrap_lines(vec![Line::from("abcdef".to_string())], 4);
    assert_eq!(wrapped.len(), 2);
    for line in &wrapped {
        assert_eq!(line.width(), 4, "each physical line filled to full width");
    }
}

#[test]
fn user_rows_get_a_gapless_background() {
    // A wide (CJK) glyph occupies two cells but a line-level background only
    // tints the first; `paint_user_bg` must fill the continuation cell too so
    // the background has no sawtooth gaps.
    let mut app = App::new("m", PermissionMode::Default);
    app.push_user("你好世界".to_string());
    let (w, h) = (20u16, 8u16);
    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
    terminal.draw(|frame| draw(frame, &mut app)).expect("draw");

    let buf = terminal.backend().buffer();
    // Inner content spans columns 1..w-1 (inside the border). Find a row whose
    // first inner cell is tinted (a user row) and assert the whole inner span
    // is tinted — no gaps.
    let tinted = |x: u16, y: u16| buf.cell((x, y)).unwrap().bg == USER_BG;
    let user_row = (0..h).find(|&y| tinted(1, y)).expect("a tinted user row");
    for x in 1..w - 1 {
        assert!(tinted(x, user_row), "gap in user background at column {x}");
    }
}

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
