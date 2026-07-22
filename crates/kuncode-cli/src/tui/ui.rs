//! Responsive ratatui shell for conversation, planning, input, and approval.

mod conversation;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Margin, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
};

use kuncode_agent::permission::PermissionMode;
use kuncode_agent::todo::TodoStatus;

use self::conversation::{
    char_width, draw_conversation, plan_item_line, truncate_display, wrap_lines,
};
use super::app::{App, Status, mode_label};
use super::bridge::ApprovalRequest;

const HEADER_HEIGHT: u16 = 2;
const FOOTER_HEIGHT: u16 = 1;
const INPUT_MAX_ROWS: u16 = 6;
const PLAN_MAX_ROWS: usize = 5;
const MIN_CONVERSATION_ROWS: u16 = 2;

#[derive(Clone, Copy)]
pub(super) struct Theme {
    colors: bool,
}

impl Theme {
    const fn new(colors: bool) -> Self {
        Self { colors }
    }

    fn color(self, color: Color) -> Style {
        if self.colors {
            Style::new().fg(color)
        } else {
            Style::new()
        }
    }

    pub(super) fn accent(self) -> Style {
        self.color(Color::Cyan)
    }

    pub(super) fn accent_strong(self) -> Style {
        self.accent().add_modifier(Modifier::BOLD)
    }

    pub(super) fn success(self) -> Style {
        self.color(Color::Green)
    }

    pub(super) fn warning(self) -> Style {
        self.color(Color::Yellow)
    }

    pub(super) fn danger(self) -> Style {
        self.color(Color::Red)
    }

    pub(super) fn muted(self) -> Style {
        self.color(Color::DarkGray).add_modifier(Modifier::DIM)
    }

    fn divider(self) -> Style {
        self.color(Color::DarkGray)
    }
}

/// Draws a responsive frame with stable priority: approval, composer, active
/// plan, then conversation history.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let theme = Theme::new(app.colors_enabled());
    let approval_lines = app
        .approval
        .as_ref()
        .map(|approval| approval_lines(approval, area.width.saturating_sub(2).max(1), theme));
    let requested_bottom = if let Some(lines) = &approval_lines {
        lines.len() as u16 + 2
    } else {
        input_height(app, area.width)
    };
    let (bottom_height, plan_height) = pane_heights(app, area.height, requested_bottom);

    let [header, body, plan_area, bottom, footer] = Layout::vertical([
        Constraint::Length(HEADER_HEIGHT),
        Constraint::Min(0),
        Constraint::Length(plan_height),
        Constraint::Length(bottom_height),
        Constraint::Length(FOOTER_HEIGHT),
    ])
    .areas(area);

    draw_header(frame, app, header, theme);
    draw_conversation(frame, app, body, theme);
    if plan_height > 0 {
        draw_plan(frame, app, plan_area, theme);
    }
    if let Some(lines) = approval_lines {
        draw_approval(frame, lines, bottom, theme);
    } else {
        draw_input(frame, app, bottom, theme);
    }
    draw_footer(frame, app, footer, theme);
}

fn pane_heights(app: &App, frame_height: u16, requested_bottom: u16) -> (u16, u16) {
    let fixed = HEADER_HEIGHT.saturating_add(FOOTER_HEIGHT);
    let usable = frame_height.saturating_sub(fixed);
    let bottom = requested_bottom.min(usable);
    if app.approval.is_some() {
        return (bottom, 0);
    }

    let plan_rows = visible_plan(app, PLAN_MAX_ROWS).len() as u16;
    let requested_plan = u16::from(plan_rows > 0).saturating_add(plan_rows);
    let plan_capacity = usable
        .saturating_sub(bottom)
        .saturating_sub(MIN_CONVERSATION_ROWS);
    (bottom, requested_plan.min(plan_capacity))
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
    frame.render_widget(
        Block::new()
            .borders(Borders::BOTTOM)
            .border_style(theme.divider()),
        area,
    );
    if area.height == 0 || area.width == 0 {
        return;
    }

    let state = match app.status {
        Status::Idle => Line::from(vec![Span::styled("●", theme.success()), Span::raw(" 就绪")]),
        Status::Running => Line::from(vec![
            Span::styled(app.activity_glyph(), theme.accent()),
            Span::raw(" 处理中"),
        ]),
        Status::Compacting => Line::from(vec![
            Span::styled(app.activity_glyph(), theme.warning()),
            Span::raw(" 整理上下文"),
        ]),
    };
    let brand_width = 12u16.min(area.width);
    let [brand, state_area] =
        Layout::horizontal([Constraint::Length(brand_width), Constraint::Min(0)])
            .areas(Rect::new(area.x, area.y, area.width, 1));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("◆", theme.accent()),
            Span::styled(" kuncode", theme.accent_strong()),
        ])),
        brand,
    );
    frame.render_widget(Paragraph::new(state), state_area);
}

/// Renders the active slice of a plan, centered around the in-progress item.
fn draw_plan(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
    let row_capacity = area.height.saturating_sub(1) as usize;
    let visible = visible_plan(app, row_capacity.min(PLAN_MAX_ROWS));
    let completed = app
        .plan
        .iter()
        .filter(|task| task.status == TodoStatus::Completed)
        .count();
    let inner_width = area.width.saturating_sub(2).max(1);
    let rows: Vec<Line> = visible
        .into_iter()
        .map(|task| plan_item_line(task, inner_width, theme))
        .collect();
    let title = format!(" 计划 {completed}/{} ", app.plan.len());
    let panel = Paragraph::new(Text::from(rows)).block(
        Block::new()
            .borders(Borders::TOP)
            .title(Line::from(title).style(theme.accent_strong()))
            .border_style(theme.divider()),
    );
    frame.render_widget(panel, area);
}

fn visible_plan(app: &App, max_rows: usize) -> Vec<&kuncode_agent::todo::TodoItem> {
    if max_rows == 0 {
        return Vec::new();
    }
    if !app
        .plan
        .iter()
        .any(|task| task.status != TodoStatus::Completed)
    {
        return Vec::new();
    }
    if app.plan.len() <= max_rows {
        return app.plan.iter().collect();
    }

    let focus = app
        .plan
        .iter()
        .position(|task| task.status == TodoStatus::InProgress)
        .or_else(|| {
            app.plan
                .iter()
                .position(|task| task.status == TodoStatus::Pending)
        })
        .unwrap_or(0);
    let start = focus
        .saturating_sub(max_rows / 2)
        .min(app.plan.len() - max_rows);
    app.plan[start..start + max_rows].iter().collect()
}

fn input_height(app: &App, width: u16) -> u16 {
    let content_width = width.saturating_sub(4).max(1);
    let logical: Vec<Line> = app
        .input
        .split('\n')
        .map(|segment| Line::raw(segment.to_string()))
        .collect();
    let rows = wrap_lines(logical, content_width).len() as u16;
    rows.clamp(1, INPUT_MAX_ROWS).saturating_add(2)
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
    let title = match app.status {
        Status::Idle => " 提问 ",
        Status::Running => " 处理中 ",
        Status::Compacting => " 整理上下文 ",
    };
    let block = Block::bordered()
        .title(Line::from(title).style(if app.status == Status::Idle {
            theme.accent_strong()
        } else {
            theme.muted()
        }))
        .border_style(if app.status == Status::Idle {
            theme.accent()
        } else {
            theme.divider()
        });
    let inner = area.inner(Margin::new(1, 1));
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let prompt_width = inner.width.min(2);
    let [prompt_area, text_area] =
        Layout::horizontal([Constraint::Length(prompt_width), Constraint::Min(0)]).areas(inner);
    frame.render_widget(
        Paragraph::new(if app.status == Status::Idle {
            "›"
        } else {
            "·"
        })
        .style(if app.status == Status::Idle {
            theme.accent_strong()
        } else {
            theme.muted()
        }),
        prompt_area,
    );

    let inner_width = text_area.width.max(1);
    let inner_height = text_area.height.max(1);

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

    let content = if app.input.is_empty() {
        let placeholder = match app.status {
            Status::Idle => "描述你想完成的任务",
            Status::Running => "等待当前任务完成",
            Status::Compacting => "正在整理会话上下文",
        };
        Text::from(Line::from(placeholder).style(theme.muted()))
    } else {
        Text::from(wrapped)
    };
    frame.render_widget(Paragraph::new(content).scroll((scroll, 0)), text_area);

    // Show the cursor only when the user can type (idle, no modal), clamped inside
    // the visible box.
    if app.status == Status::Idle && app.approval.is_none() {
        let cursor_x = text_area.x + caret_col.min(inner_width.saturating_sub(1));
        let cursor_y = text_area.y + (caret_row - scroll).min(inner_height - 1);
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

fn draw_footer(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let metadata = format!("{} · {}", app.model_name, mode_label(app.mode));
    let metadata = truncate_display(&metadata, area.width.saturating_sub(1));
    let mode_style = if app.mode == PermissionMode::BypassPermissions {
        theme.warning()
    } else {
        theme.muted()
    };

    if !app.follow && area.width >= 24 {
        let left_width = 14u16.min(area.width);
        let [left, right] =
            Layout::horizontal([Constraint::Length(left_width), Constraint::Min(0)]).areas(area);
        frame.render_widget(
            Paragraph::new(Line::from("↑ 较早内容").style(theme.warning())),
            left,
        );
        frame.render_widget(
            Paragraph::new(Line::from(metadata).style(mode_style)).alignment(Alignment::Right),
            right,
        );
    } else {
        frame.render_widget(
            Paragraph::new(Line::from(metadata).style(mode_style)).alignment(Alignment::Right),
            area,
        );
    }
}

fn approval_lines(approval: &ApprovalRequest, width: u16, theme: Theme) -> Vec<Line<'static>> {
    let detail_rows = if width >= 46 { 2 } else { 1 };
    let summary = truncate_display(
        &format!("操作  {}", approval.summary),
        width.saturating_mul(detail_rows),
    );
    let scope = truncate_display(
        &format!("范围  {}", approval.persistence_label()),
        width.saturating_mul(detail_rows),
    );
    let mut lines = wrap_lines(
        vec![
            Line::from(summary).style(theme.warning()),
            Line::from(scope).style(theme.muted()),
        ],
        width,
    );

    let mut actions = vec![("y", "允许一次")];
    if approval.allow_session.is_some() {
        actions.push(("a", "本次会话允许"));
    }
    actions.push(("n", "拒绝一次"));
    if approval.deny_session.is_some() {
        actions.push(("d", "本次会话拒绝"));
    }
    actions.push(("Esc", "取消任务"));
    lines.extend(action_lines(&actions, width, theme));
    lines
}

fn action_lines(actions: &[(&str, &str)], width: u16, theme: Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut used = 0u16;
    for (key, label) in actions {
        let key_text = format!("[{key}]");
        let action_width = char_widths(&key_text)
            .saturating_add(1)
            .saturating_add(char_widths(label));
        let separator = u16::from(used > 0).saturating_mul(2);
        if used > 0 && used.saturating_add(separator).saturating_add(action_width) > width {
            lines.push(Line::from(std::mem::take(&mut spans)));
            used = 0;
        }
        if used > 0 {
            spans.push(Span::raw("  "));
            used = used.saturating_add(2);
        }
        spans.push(Span::styled(key_text, theme.accent_strong()));
        spans.push(Span::raw(format!(" {label}")));
        used = used.saturating_add(action_width);
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

fn char_widths(text: &str) -> u16 {
    text.chars()
        .fold(0u16, |width, ch| width.saturating_add(char_width(ch)))
}

/// Renders a permission decision in place of the composer so it cannot be
/// mistaken for ordinary model output.
fn draw_approval(frame: &mut Frame, lines: Vec<Line<'static>>, area: Rect, theme: Theme) {
    let panel = Paragraph::new(Text::from(lines)).block(
        Block::bordered()
            .title(Line::from(" 需要授权 ").style(theme.warning()))
            .border_style(theme.warning()),
    );
    frame.render_widget(panel, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{Item, ToolState};
    use crate::tui::bridge::ApprovalRequest;
    use kuncode_agent::compaction::budget::TokenCountPrecision;
    use kuncode_agent::observer::EventKind;
    use kuncode_agent::permission::PermissionMode;
    use kuncode_agent::todo::{TodoItem, TodoStatus};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::oneshot;

    fn approval(summary: impl Into<String>) -> ApprovalRequest {
        let (respond, _rx) = oneshot::channel();
        ApprovalRequest {
            summary: summary.into(),
            targets: vec!["Bash(cargo test --workspace)".to_string()],
            allow_session: None,
            deny_session: None,
            respond,
        }
    }

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
        assert!(rendered.contains("计划 1/2"), "plan progress is shown");
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
            !rendered.contains("计划 2/2"),
            "an all-completed plan hides the panel"
        );
    }

    #[test]
    fn plan_window_keeps_a_late_active_item_visible() {
        let mut app = App::new("m", PermissionMode::Default);
        app.plan = (0..10)
            .map(|index| TodoItem {
                content: format!("Task {}", index + 1),
                active_form: format!("Executing task {}", index + 1),
                status: if index < 8 {
                    TodoStatus::Completed
                } else if index == 8 {
                    TodoStatus::InProgress
                } else {
                    TodoStatus::Pending
                },
            })
            .collect();

        let mut terminal = Terminal::new(TestBackend::new(48, 16)).expect("terminal");
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
        let rendered = format!("{}", terminal.backend());

        assert!(rendered.contains("计划 8/10"));
        assert!(rendered.contains("Executing task 9"));

        let mut terminal = Terminal::new(TestBackend::new(32, 10)).expect("small terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("small draw");
        assert!(
            format!("{}", terminal.backend()).contains("Executing task 9"),
            "the active item remains visible when the plan shrinks to one row"
        );
    }

    #[test]
    fn narrow_approval_keeps_real_actions_visible() {
        let mut app = App::new("m", PermissionMode::Default);
        app.set_approval(approval(
            "run a deliberately long command summary that must not hide decisions",
        ));

        let mut terminal = Terminal::new(TestBackend::new(32, 10)).expect("terminal");
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
        let rendered = format!("{}", terminal.backend());

        assert!(rendered.contains("[y]"));
        assert!(rendered.contains("[n]"));
        assert!(rendered.contains("[Esc]"));
        assert!(!rendered.contains("[a]"));
        assert!(!rendered.contains("[d]"));
    }

    #[test]
    fn responsive_layout_renders_at_supported_small_sizes() {
        for (width, height) in [(80, 24), (48, 14), (32, 10)] {
            let mut app = App::new("a-model-name-that-is-long", PermissionMode::Default);
            app.push_user("分析这个项目并运行测试".to_string());
            app.plan = (0..8)
                .map(|index| TodoItem {
                    content: format!("Long plan task {index}"),
                    active_form: format!("Working on long plan task {index}"),
                    status: if index == 7 {
                        TodoStatus::InProgress
                    } else {
                        TodoStatus::Completed
                    },
                })
                .collect();
            app.set_approval(approval("run cargo test --workspace"));

            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal
                .draw(|frame| draw(frame, &mut app))
                .expect("responsive draw");
            let rendered = format!("{}", terminal.backend());
            assert!(rendered.contains("需要授权"));
            assert!(rendered.contains("[y]"));
            assert!(rendered.contains("[n]"));
        }
    }

    #[test]
    fn no_color_mode_emits_no_foreground_or_background_colors() {
        let mut app = App::new("model", PermissionMode::Default);
        app.set_colors_enabled(false);
        app.push_user("hello".to_string());
        app.push_assistant("world".to_string());
        let (width, height) = (40u16, 12u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");

        let buffer = terminal.backend().buffer();
        for y in 0..height {
            for x in 0..width {
                let cell = buffer.cell((x, y)).expect("cell");
                assert_eq!(cell.fg, Color::Reset, "foreground color at ({x}, {y})");
                assert_eq!(cell.bg, Color::Reset, "background color at ({x}, {y})");
            }
        }
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

        // The composer reserves border(1) + prompt(2), then uses the edit column.
        let pos = terminal.get_cursor_position().expect("cursor position");
        assert_eq!(
            pos.x, 6,
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
        assert!(rendered.contains("整理上下文"));
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
        assert!(rendered.contains("上下文已整理"));
        assert!(!rendered.contains("98765"));
        assert!(!rendered.contains("12345"));
    }
}
