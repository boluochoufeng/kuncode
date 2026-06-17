//! ratatui rendering: conversation log, input box, status line, approval modal.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Position, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Text},
    widgets::{Block, Paragraph, Wrap},
};

use super::app::{App, Item, Status, ToolState, mode_label};
use super::bridge::ApprovalRequest;

/// Height of the approval panel when it takes the input box's place: a wrapped
/// summary line, the rule line, the choices line, plus the border.
const APPROVAL_HEIGHT: u16 = 6;

/// Background tint distinguishing user input from agent output.
const USER_BG: Color = Color::Rgb(45, 50, 70);

/// Draws one frame: conversation body, a bottom pane, and the status line. The
/// bottom pane is the input box, or — while an approval is pending — the
/// permission panel *in its place* (aligned to the input, not a centered popup).
pub fn draw(frame: &mut Frame, app: &mut App) {
    let bottom_height = if app.approval.is_some() {
        APPROVAL_HEIGHT
    } else {
        // Border (2) + content, capped so a long paste can't swallow the screen.
        let input_lines = app.input.split('\n').count().max(1) as u16;
        (input_lines + 2).min(8)
    };

    let [body, bottom, status_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(bottom_height),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_conversation(frame, app, body);
    if let Some(approval) = &app.approval {
        draw_approval(frame, approval, bottom);
    } else {
        draw_input(frame, app, bottom);
    }
    draw_status(frame, app, status_area);
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
    lines
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

    let (caret_row, caret_col) = caret_position(&app.input, inner_width);
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
