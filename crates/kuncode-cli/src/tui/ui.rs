//! ratatui rendering: conversation log, input box, status line, approval modal.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Position, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Text},
    widgets::{Block, Paragraph, Wrap},
};

use kuncode_agent::todo::{TodoItem, TodoStatus};

use super::app::{App, Item, Status, ToolState, mode_label};
use super::bridge::ApprovalRequest;

/// Height of the approval panel when it takes the input box's place: a wrapped
/// summary line, the rule line, the choices line, plus the border.
const APPROVAL_HEIGHT: u16 = 6;

/// Background tint distinguishing user input from agent output.
const USER_BG: Color = Color::Rgb(45, 50, 70);

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

fn draw_conversation(frame: &mut Frame, app: &mut App, area: Rect) {
    let inner_width = area.width.saturating_sub(2).max(1);
    let inner_height = area.height.saturating_sub(2);

    // Wrap to exact physical lines ourselves rather than letting `Paragraph`
    // word-wrap: a hard-division estimate runs short of the real wrapped count,
    // so `max_scroll` came out too small and PageUp got swallowed. Exact wrapping
    // makes the scroll range correct.
    let lines = wrap_lines(conversation_lines(app), inner_width);
    // Tag user rows by their background so we can repaint them after layout:
    // `Paragraph` leaves a wide glyph's second cell untinted (sawtooth gaps).
    let user_rows: Vec<bool> = lines.iter().map(|l| l.style.bg == Some(USER_BG)).collect();
    let max_scroll = (lines.len() as u16).saturating_sub(inner_height);

    // Follow pins to the bottom; a manual scroll clamps within range and
    // re-enables follow once it lands back at the bottom.
    if app.follow {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        if app.scroll == max_scroll {
            app.follow = true;
        }
    }

    let para = Paragraph::new(Text::from(lines))
        .block(Block::bordered().title("kuncode"))
        .scroll((app.scroll, 0));
    frame.render_widget(para, area);
    paint_user_bg(frame, area, app.scroll, &user_rows);
}

/// Repaints the full-width background of user-message rows after layout.
///
/// A line-level background doesn't reach the second cell of a wide (CJK) glyph,
/// so `Paragraph` renders user rows with dotted/sawtooth gaps. This fills every
/// inner cell of those rows uniformly, including the continuation cells.
fn paint_user_bg(frame: &mut Frame, area: Rect, scroll: u16, user_rows: &[bool]) {
    let inner = area.inner(Margin::new(1, 1));
    let buf = frame.buffer_mut();
    for dy in 0..inner.height {
        let idx = scroll as usize + dy as usize;
        if user_rows.get(idx).copied().unwrap_or(false) {
            let y = inner.y + dy;
            for dx in 0..inner.width {
                if let Some(cell) = buf.cell_mut((inner.x + dx, y)) {
                    cell.bg = USER_BG;
                }
            }
        }
    }
}

/// Wraps logical lines to `width` display columns, preserving each line's style
/// and padding every physical line out to the full width. Exact wrapping keeps
/// the scroll range correct; full-width padding lets a line-level background
/// (user input) span the whole row instead of hugging the text. Splits on
/// character boundaries (not words) — fine for a conversation log.
fn wrap_lines(logical: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out = Vec::new();
    for line in &logical {
        let style = line.style;
        let content: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        let mut seg = String::new();
        let mut seg_width = 0u16;
        for ch in content.chars() {
            let cw = char_width(ch);
            if !seg.is_empty() && seg_width + cw > width {
                out.push(pad_line(seg, style, width));
                seg = String::new();
                seg_width = 0;
            }
            seg.push(ch);
            seg_width += cw;
        }
        out.push(pad_line(seg, style, width));
    }
    out
}

/// Pads `content` with trailing spaces to `width` display columns and applies
/// `style`, so a styled background fills the row to its right edge.
fn pad_line(content: String, style: Style, width: u16) -> Line<'static> {
    let used = Line::raw(content.as_str()).width() as u16;
    let mut padded = content;
    padded.push_str(&" ".repeat(width.saturating_sub(used) as usize));
    Line::from(padded).style(style)
}

/// Display width of a single char (`Line::width` is unicode-aware, counting CJK
/// as 2 cells).
fn char_width(ch: char) -> u16 {
    Line::raw(ch.to_string()).width() as u16
}

/// Flattens the conversation log into styled lines. Multi-line user/assistant
/// text is split so each physical line is its own [`Line`].
fn conversation_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    for item in &app.conversation {
        match item {
            Item::User(text) => {
                // Tag rows with the user background; the gap-free fill happens in
                // `paint_user_bg` after layout. A blank tinted row above and below
                // gives the block vertical breathing room instead of hugging text.
                lines.push(Line::from("").bg(USER_BG));
                for (i, raw) in text.split('\n').enumerate() {
                    let prefix = if i == 0 { "› " } else { "  " };
                    lines.push(Line::from(format!("{prefix}{raw}")).bold().bg(USER_BG));
                }
                lines.push(Line::from("").bg(USER_BG));
            }
            Item::Assistant(text) => {
                for raw in text.split('\n') {
                    lines.push(Line::from(raw.to_string()));
                }
            }
            Item::Tool {
                name,
                summary,
                state,
                ..
            } => {
                let title = display_tool_name(name);
                lines.push(Line::from(format!("⏺ {title}  {summary}")).cyan());
                lines.push(tool_state_line(state));
            }
            Item::Error(text) => lines.push(Line::from(format!("✗ {text}")).red()),
        }
        lines.push(Line::from(""));
    }
    append_stream_preview(&mut lines, app);
    lines
}

/// Appends the in-progress streamed answer/reasoning below the committed log.
///
/// Reasoning renders first, dimmed, as a separate "thinking" channel; the answer
/// follows in the normal assistant style. Both are ephemeral — the next commit
/// clears [`App::stream_answer`]/[`App::stream_reasoning`] and they vanish,
/// replaced by the committed item.
///
/// Only the typewriter-revealed prefix is drawn (see [`App::advance_reveal`]), so
/// a fast stream types out at a readable pace instead of flooding in at once.
fn append_stream_preview(lines: &mut Vec<Line<'static>>, app: &App) {
    let reasoning = &app.stream_reasoning[..app.reasoning_revealed];
    if !reasoning.is_empty() {
        for raw in reasoning.split('\n') {
            lines.push(Line::from(raw.to_string()).dim());
        }
    }
    let answer = &app.stream_answer[..app.answer_revealed];
    if !answer.is_empty() {
        for raw in answer.split('\n') {
            lines.push(Line::from(raw.to_string()));
        }
    }
}

/// One checklist row for the plan panel: the shared status glyph + text, colored
/// per status. The glyph and text-field choice come from
/// [`crate::observer::todo_glyph_and_text`] so this and the plain renderer stay
/// in lockstep; only the color is TUI-local.
fn plan_item_line(todo: &TodoItem) -> Line<'static> {
    let (glyph, text) = crate::observer::todo_glyph_and_text(todo);
    let body = format!(" {glyph} {text}");
    match todo.status {
        TodoStatus::Pending => Line::from(body).dim(),
        TodoStatus::InProgress => Line::from(body).cyan(),
        TodoStatus::Completed => Line::from(body).green(),
    }
}

fn tool_state_line(state: &ToolState) -> Line<'static> {
    match state {
        ToolState::Running => Line::from("  ⎿ …".to_string()).dim(),
        ToolState::Ok { truncated } => {
            let mark = if *truncated {
                "  ⎿ ✓ (截断)"
            } else {
                "  ⎿ ✓"
            };
            Line::from(mark.to_string()).green()
        }
        ToolState::Failed(msg) => Line::from(format!("  ⎿ ✗ {msg}")).red(),
        ToolState::Denied(msg) => Line::from(format!("  ⎿ ⛔ {msg}")).yellow(),
    }
}

/// Formats a tool's protocol name for display: snake_case → PascalCase, so a
/// call reads like a proper name (`read_file` → `ReadFile`, `bash` → `Bash`).
/// The wire name the model sees is unchanged; this is cosmetic only.
fn display_tool_name(name: &str) -> String {
    name.split('_')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            let mut chars = segment.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect()
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let title = if app.status == Status::Running {
        "input (运行中)"
    } else {
        "input"
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
    let (state, hint) = if app.status == Status::Running {
        ("运行中", "^C 取消")
    } else {
        ("就绪", "⏎ 发送 · exit/^C 退出")
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
}
