//! Conversation-log layout and styling.

use kuncode_agent::todo::{TodoItem, TodoStatus};
use ratatui::{
    Frame,
    layout::{Margin, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Text},
    widgets::{Block, Paragraph},
};

use super::super::app::{App, Item, ToolState};

pub(super) const USER_BG: Color = Color::Rgb(45, 50, 70);

pub(super) fn draw_conversation(frame: &mut Frame, app: &mut App, area: Rect) {
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
pub(super) fn paint_user_bg(frame: &mut Frame, area: Rect, scroll: u16, user_rows: &[bool]) {
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
pub(super) fn wrap_lines(logical: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
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
pub(super) fn char_width(ch: char) -> u16 {
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
            Item::Compaction => lines.push(Line::from("◆ 上下文已压缩").dim()),
            Item::Warning(text) => lines.push(Line::from(format!("⚠ {text}")).yellow()),
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
pub(super) fn plan_item_line(todo: &TodoItem) -> Line<'static> {
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
pub(super) fn display_tool_name(name: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::super::draw;
    use super::*;
    use kuncode_agent::observer::EventKind;
    use kuncode_agent::permission::PermissionMode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
