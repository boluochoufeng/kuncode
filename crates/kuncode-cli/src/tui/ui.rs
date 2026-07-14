//! ratatui rendering: conversation log, input box, status line, approval modal.

mod conversation;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Style, Stylize},
    text::{Line, Text},
    widgets::{Block, Paragraph, Wrap},
};

use kuncode_agent::todo::TodoStatus;

use self::conversation::{char_width, draw_conversation, plan_item_line, wrap_lines};
use super::app::{App, Status, mode_label};
use super::bridge::ApprovalRequest;

/// Height of the approval panel when it takes the input box's place: a wrapped
/// summary line, the rule line, the choices line, plus the border.
const APPROVAL_HEIGHT: u16 = 6;

/// Largest the plan panel grows to (border + this many task rows); longer plans
/// clip rather than crowd out the conversation.
const PLAN_MAX_ROWS: u16 = 8;

/// Draws one frame: conversation body, the sticky plan panel (while work is
/// outstanding), a bottom pane, and the status line. The bottom pane is the input
/// box, or —
/// while an approval is pending — the permission panel *in its place* (aligned to
/// the input, not a centered popup). The plan sits between the scrolling log and
/// the input so the live checklist stays pinned below the latest activity.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let bottom_height = if app.approval.is_some() {
        APPROVAL_HEIGHT
    } else {
        // Border (2) + content, capped so a long paste can't swallow the screen.
        let input_lines = app.input.split('\n').count().max(1) as u16;
        (input_lines + 2).min(8)
    };

    // Show the panel only while work is outstanding: an empty plan, or one whose
    // tasks are all completed, collapses the region to zero height. The last task
    // ticking to ✓ makes the panel vanish — that disappearance *is* the "done"
    // signal, instead of leaving an all-✓ checklist lingering as noise.
    let plan_outstanding = app
        .plan
        .iter()
        .any(|task| task.status != TodoStatus::Completed);
    let plan_height = if plan_outstanding {
        (app.plan.len() as u16).min(PLAN_MAX_ROWS) + 2 // + border
    } else {
        0
    };

    let [body, plan_area, bottom, status_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(plan_height),
        Constraint::Length(bottom_height),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_conversation(frame, app, body);
    if plan_height > 0 {
        draw_plan(frame, app, plan_area);
    }
    if let Some(approval) = &app.approval {
        draw_approval(frame, approval, bottom);
    } else {
        draw_input(frame, app, bottom);
    }
    draw_status(frame, app, status_area);
}

/// Renders the live task plan as a bordered panel: one colored checklist row per
/// task. Pinned above the input by [`draw`], so it never scrolls away with the
/// log.
fn draw_plan(frame: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<Line> = app.plan.iter().map(plan_item_line).collect();
    let panel = Paragraph::new(Text::from(rows)).block(
        Block::bordered()
            .title("任务计划")
            .border_style(Style::new().cyan()),
    );
    frame.render_widget(panel, area);
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let title = match app.status {
        Status::Idle => "input",
        Status::Running => "input (运行中)",
        Status::Compacting => "input (压缩上下文)",
    };
    let inner_width = area.width.saturating_sub(2).max(1);
    let inner_height = area.height.saturating_sub(2).max(1);

    // Wrap the input on char boundaries exactly as the conversation does, and
    // render without `Paragraph`'s word-wrap. The caret uses the *same* wrap
    // (`caret_position`), so cursor and scroll can't drift from what's drawn — a
    // word-wrap renderer paired with a char-width estimate would, and could scroll
    // the box to a blank row on ordinary input containing spaces.
    let logical: Vec<Line> = app
        .input
        .split('\n')
        .map(|seg| Line::raw(seg.to_string()))
        .collect();
    let wrapped = wrap_lines(logical, inner_width);

    // Caret sits at the cursor, not the end: greedy char-wrap is prefix-determined,
    // so wrapping `input[..cursor]` yields the cursor's exact (row, col).
    let (caret_row, caret_col) = caret_position(&app.input[..app.cursor], inner_width);
    // Scroll so the caret's row is the bottom visible row of the box.
    let scroll = caret_row.saturating_sub(inner_height - 1);

    let para = Paragraph::new(Text::from(wrapped))
        .block(Block::bordered().title(title))
        .scroll((scroll, 0));
    frame.render_widget(para, area);

    // Show the cursor only when the user can type (idle, no modal), clamped inside
    // the visible box.
    if app.status == Status::Idle && app.approval.is_none() {
        let cursor_x = area.x + 1 + caret_col.min(inner_width.saturating_sub(1));
        let cursor_y = area.y + 1 + (caret_row - scroll).min(inner_height - 1);
        frame.set_cursor_position(Position::new(cursor_x, cursor_y));
    }
}

/// Caret row/column at the end of `input`, wrapped to `inner_width` on the same
/// char-boundary rule as [`wrap_lines`] (greedy, breaking before a char that
/// would overflow; `'\n'` starts a new row). Returns 0-based (row, column) in
/// display cells so the cursor lands exactly where the rendered text wraps.
fn caret_position(input: &str, inner_width: u16) -> (u16, u16) {
    let inner_width = inner_width.max(1);
    let mut row = 0u16;
    let mut col = 0u16;
    for ch in input.chars() {
        if ch == '\n' {
            row += 1;
            col = 0;
            continue;
        }
        let cw = char_width(ch);
        if col > 0 && col + cw > inner_width {
            row += 1;
            col = 0;
        }
        col += cw;
    }
    (row, col)
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let (state, hint) = match app.status {
        Status::Idle => ("就绪", "⏎ 发送 · exit/^C 退出"),
        Status::Running => ("运行中", "^C 取消"),
        Status::Compacting => ("正在压缩上下文", "^C 取消"),
    };
    let text = format!(
        " {} · {} · {} · {} ",
        app.model_name,
        mode_label(app.mode),
        state,
        hint
    );
    frame.render_widget(Paragraph::new(text).dim(), area);
}

/// Renders the approval as a full-width bar in the bottom pane, sharing the
/// input box's left edge and width.
fn draw_approval(frame: &mut Frame, approval: &ApprovalRequest, area: Rect) {
    let lines = vec![
        Line::from(format!("⚠ 需要授权: {}", approval.summary)).yellow(),
        Line::from(format!("记住规则: {}", approval.scope_rule())).dim(),
        Line::from("[y] 允许一次  [a] 总是  [n] 否  [d] 永久拒绝  [c] 取消"),
    ];
    let panel = Paragraph::new(Text::from(lines))
        .block(
            Block::bordered()
                .title("权限")
                .border_style(Style::new().yellow()),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(panel, area);
}

#[cfg(test)]
mod tests {
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
}
