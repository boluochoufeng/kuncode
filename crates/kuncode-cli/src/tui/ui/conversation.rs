//! Conversation-log layout and styling.

use kuncode_agent::todo::{TodoItem, TodoStatus};
use ratatui::{
    Frame,
    layout::{Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};

use super::super::app::{App, Item, ToolState};
use super::Theme;

/// Renders the scrollable conversation viewport and its overflow indicator.
pub(super) fn draw_conversation(frame: &mut Frame, app: &mut App, area: Rect, theme: Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let horizontal_margin = if area.width >= 8 { 2 } else { 0 };
    let vertical_margin = u16::from(area.height >= 5);
    let viewport = area.inner(Margin::new(horizontal_margin, vertical_margin));
    let scrollbar_width = u16::from(viewport.width >= 5);
    let content_area = Rect::new(
        viewport.x,
        viewport.y,
        viewport.width.saturating_sub(scrollbar_width),
        viewport.height,
    );
    if content_area.width == 0 || content_area.height == 0 {
        return;
    }

    // Wrap to exact physical lines ourselves rather than letting `Paragraph`
    // word-wrap: a hard-division estimate runs short of the real wrapped count,
    // so `max_scroll` came out too small and PageUp got swallowed. Exact wrapping
    // makes the scroll range correct.
    let lines = wrap_lines(conversation_lines(app, theme), content_area.width);
    let content_rows = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let max_scroll = content_rows.saturating_sub(content_area.height);

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

    frame.render_widget(
        Paragraph::new(Text::from(lines)).scroll((app.scroll, 0)),
        content_area,
    );
    if max_scroll > 0 && scrollbar_width > 0 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .track_style(theme.muted())
            .thumb_symbol("┃")
            .thumb_style(theme.accent());
        let mut state = ScrollbarState::new(content_rows as usize)
            .position(app.scroll as usize)
            .viewport_content_length(content_area.height as usize);
        let scrollbar_area = Rect::new(
            viewport.x + viewport.width.saturating_sub(1),
            viewport.y,
            1,
            viewport.height,
        );
        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut state);
    }
}

/// Wraps logical lines to exact display columns while preserving span styles.
pub(super) fn wrap_lines(logical: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out = Vec::new();
    for line in logical {
        let line_style = line.style;
        let mut row = Vec::new();
        let mut row_width = 0u16;
        for span in line.spans {
            let span_style = span.style;
            let mut chunk = String::new();
            for ch in span.content.chars() {
                let char_width = char_width(ch);
                if row_width > 0 && row_width.saturating_add(char_width) > width {
                    push_chunk(&mut row, &mut chunk, span_style);
                    out.push(finish_line(std::mem::take(&mut row), line_style));
                    row_width = 0;
                }
                chunk.push(ch);
                row_width = row_width.saturating_add(char_width);
            }
            push_chunk(&mut row, &mut chunk, span_style);
        }
        out.push(finish_line(row, line_style));
    }
    out
}

fn push_chunk(spans: &mut Vec<Span<'static>>, chunk: &mut String, style: Style) {
    if !chunk.is_empty() {
        spans.push(Span::styled(std::mem::take(chunk), style));
    }
}

fn finish_line(spans: Vec<Span<'static>>, style: Style) -> Line<'static> {
    Line::from(spans).style(style)
}

/// Display width of a single char (`Line::width` is unicode-aware, counting CJK
/// as 2 cells).
pub(super) fn char_width(ch: char) -> u16 {
    Line::raw(ch.to_string()).width() as u16
}

/// Truncates text to `width` display cells and marks omitted content.
pub(super) fn truncate_display(text: &str, width: u16) -> String {
    if width == 0 {
        return String::new();
    }
    let total = text
        .chars()
        .fold(0u16, |used, ch| used.saturating_add(char_width(ch)));
    if total <= width {
        return text.to_string();
    }

    let available = width.saturating_sub(1);
    let mut output = String::new();
    let mut used = 0u16;
    for ch in text.chars() {
        let char_width = char_width(ch);
        if used.saturating_add(char_width) > available {
            break;
        }
        output.push(ch);
        used = used.saturating_add(char_width);
    }
    output.push('…');
    output
}

/// Flattens the conversation log into styled lines. Multi-line user/assistant
/// text is split so each physical line is its own [`Line`].
fn conversation_lines(app: &App, theme: Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    for item in &app.conversation {
        match item {
            Item::User(text) => {
                lines.push(Line::from(vec![
                    Span::styled("❯", theme.accent_strong()),
                    Span::styled(" 你", Style::new().add_modifier(Modifier::BOLD)),
                ]));
                for raw in text.split('\n') {
                    lines.push(Line::from(format!("  {raw}")));
                }
            }
            Item::Assistant(text) => {
                lines.push(assistant_heading(theme));
                for raw in text.split('\n') {
                    lines.push(Line::from(format!("  {raw}")));
                }
            }
            Item::Tool {
                name,
                summary,
                state,
                ..
            } => {
                append_tool(&mut lines, app, name, summary, state, theme);
            }
            Item::Error(text) => lines.push(Line::from(format!("× {text}")).style(theme.danger())),
            Item::Compaction => lines.push(Line::from("◇ 上下文已整理").style(theme.muted())),
            Item::Warning(text) => {
                lines.push(Line::from(format!("! {text}")).style(theme.warning()))
            }
        }
        lines.push(Line::from(""));
    }
    append_stream_preview(&mut lines, app, theme);
    lines
}

fn assistant_heading(theme: Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled("◆", theme.success()),
        Span::styled(" kuncode", theme.success().add_modifier(Modifier::BOLD)),
    ])
}

fn append_tool(
    lines: &mut Vec<Line<'static>>,
    app: &App,
    name: &str,
    summary: &str,
    state: &ToolState,
    theme: Theme,
) {
    let (glyph, glyph_style, suffix) = match state {
        ToolState::Running => (app.activity_glyph(), theme.accent(), None),
        ToolState::Ok { truncated: false } => ("✓", theme.success(), None),
        ToolState::Ok { truncated: true } => ("✓", theme.warning(), Some("输出已截断")),
        ToolState::Failed(_) => ("×", theme.danger(), None),
        ToolState::Denied(_) => ("!", theme.warning(), None),
    };
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(glyph, glyph_style),
        Span::raw(" "),
        Span::styled(
            display_tool_name(name),
            Style::new().add_modifier(Modifier::BOLD),
        ),
    ];
    if !summary.trim().is_empty() {
        spans.push(Span::styled(format!("  {summary}"), theme.muted()));
    }
    if let Some(suffix) = suffix {
        spans.push(Span::styled(format!("  {suffix}"), theme.warning()));
    }
    lines.push(Line::from(spans));
    match state {
        ToolState::Failed(message) => {
            lines.push(Line::from(format!("    {message}")).style(theme.danger()));
        }
        ToolState::Denied(message) => {
            lines.push(Line::from(format!("    {message}")).style(theme.warning()));
        }
        ToolState::Running | ToolState::Ok { .. } => {}
    }
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
fn append_stream_preview(lines: &mut Vec<Line<'static>>, app: &App, theme: Theme) {
    let reasoning = &app.stream_reasoning[..app.reasoning_revealed];
    let answer = &app.stream_answer[..app.answer_revealed];
    if !reasoning.is_empty() || !answer.is_empty() {
        lines.push(assistant_heading(theme));
    }
    if !reasoning.is_empty() {
        if answer.is_empty() {
            lines.push(Line::from("  思考中").style(theme.muted()));
            for raw in reasoning_preview(reasoning).split('\n') {
                lines.push(Line::from(format!("  {raw}")).style(theme.muted()));
            }
        } else {
            lines.push(Line::from("  思考完成").style(theme.muted()));
        }
    }
    if !answer.is_empty() {
        for raw in answer.split('\n') {
            lines.push(Line::from(format!("  {raw}")));
        }
    }
}

fn reasoning_preview(reasoning: &str) -> String {
    const MAX_LINES: usize = 4;
    const MAX_CHARS: usize = 240;

    let lines: Vec<&str> = reasoning.lines().rev().take(MAX_LINES).collect();
    let mut preview = lines.into_iter().rev().collect::<Vec<_>>().join("\n");
    let total_chars = preview.chars().count();
    if total_chars <= MAX_CHARS {
        return preview;
    }

    let start = preview
        .char_indices()
        .nth(total_chars - MAX_CHARS)
        .map_or(0, |(offset, _)| offset);
    preview = format!("…{}", &preview[start..]);
    preview
}

/// One checklist row for the plan panel: the shared status glyph + text, colored
/// per status. The glyph and text-field choice come from
/// [`crate::observer::todo_glyph_and_text`] so this and the plain renderer stay
/// in lockstep; only the color is TUI-local.
pub(super) fn plan_item_line(todo: &TodoItem, width: u16, theme: Theme) -> Line<'static> {
    let (glyph, text) = crate::observer::todo_glyph_and_text(todo);
    let body = truncate_display(&format!(" {glyph} {text}"), width);
    match todo.status {
        TodoStatus::Pending => Line::from(body).style(theme.muted()),
        TodoStatus::InProgress => Line::from(body).style(theme.accent_strong()),
        TodoStatus::Completed => Line::from(body).style(theme.success()),
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
    fn streamed_answer_collapses_completed_reasoning() {
        let mut app = App::new("model-x", PermissionMode::Default);
        app.apply_event(EventKind::ReasoningDelta {
            text: "weighing options".to_string(),
        });
        app.advance_reveal(std::time::Duration::from_secs(1), 100_000);
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).expect("test terminal");
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
        assert!(format!("{}", terminal.backend()).contains("weighing options"));

        app.apply_event(EventKind::TextDelta {
            text: "partial answer".to_string(),
        });
        // Only the revealed prefix draws; a huge budget reveals everything for
        // this assertion (the typewriter pacing itself is tested in app.rs).
        app.advance_reveal(std::time::Duration::from_secs(1), 100_000);

        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");

        let rendered = format!("{}", terminal.backend());
        assert!(
            rendered.contains("partial answer"),
            "in-progress answer should render live"
        );
        assert!(
            rendered.contains("思考完成"),
            "completed reasoning should collapse once the answer starts"
        );
        assert!(!rendered.contains("weighing options"));
    }

    #[test]
    fn tool_names_display_as_pascal_case() {
        assert_eq!(display_tool_name("bash"), "Bash");
        assert_eq!(display_tool_name("read_file"), "ReadFile");
        assert_eq!(display_tool_name("glob"), "Glob");
    }

    #[test]
    fn wraps_to_exact_lines_without_padding_short_rows() {
        let wrapped = wrap_lines(vec![Line::from("abcdef".to_string())], 4);
        assert_eq!(wrapped.len(), 2);
        assert_eq!(wrapped[0].width(), 4);
        assert_eq!(wrapped[1].width(), 2);
    }

    #[test]
    fn user_message_uses_a_role_marker_without_forcing_a_background() {
        let mut app = App::new("m", PermissionMode::Default);
        app.set_colors_enabled(true);
        app.push_user("你好世界".to_string());
        let (w, h) = (20u16, 8u16);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");

        let rendered = format!("{}", terminal.backend());
        assert!(rendered.contains("你"), "the user role should be labeled");
        let buf = terminal.backend().buffer();
        for y in 0..h {
            for x in 0..w {
                assert_eq!(
                    buf.cell((x, y)).expect("cell").bg,
                    ratatui::style::Color::Reset,
                    "conversation should inherit the terminal background"
                );
            }
        }
    }

    #[test]
    fn wrapping_preserves_span_styles() {
        let accent = Style::new().fg(ratatui::style::Color::Cyan);
        let wrapped = wrap_lines(
            vec![Line::from(vec![
                Span::styled("ab", accent),
                Span::raw("cdef"),
            ])],
            3,
        );

        assert_eq!(wrapped.len(), 2);
        assert_eq!(
            wrapped[0].spans[0].style.fg,
            Some(ratatui::style::Color::Cyan)
        );
        assert_eq!(wrapped[0].width(), 3);
        assert_eq!(wrapped[1].width(), 3);
    }

    #[test]
    fn reasoning_preview_keeps_only_a_bounded_tail() {
        let reasoning = (0..10)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = reasoning_preview(&reasoning);

        assert!(!preview.contains("line 5"));
        assert!(preview.contains("line 6"));
        assert!(preview.contains("line 9"));
        assert_eq!(preview.lines().count(), 4);
    }

    #[test]
    fn display_truncation_respects_wide_characters() {
        assert_eq!(truncate_display("abcdef", 4), "abc…");
        assert_eq!(truncate_display("你好世界", 5), "你好…");
        assert_eq!(truncate_display("abc", 3), "abc");
    }
}
