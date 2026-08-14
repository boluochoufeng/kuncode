//! Responsive ratatui shell for conversation, planning, input, and approval.

mod conversation;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Margin, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};

use kuncode_agent::permission::PermissionMode;
use kuncode_agent::todo::TodoStatus;
use kuncode_core::completion::Usage;

use self::conversation::{
    char_width, display_width, draw_conversation, plan_item_line, truncate_display, wrap_lines,
};
use super::app::{App, Status, mode_label};
use super::bridge::ApprovalRequest;
use super::command;

const HEADER_HEIGHT: u16 = 2;
const FOOTER_HEIGHT: u16 = 1;
const INPUT_MAX_ROWS: u16 = 6;
const MENU_MAX_ROWS: usize = 8;
const PLAN_MAX_ROWS: usize = 5;
const MIN_CONVERSATION_ROWS: u16 = 2;
/// Footer hint shown while the transcript is scrolled off its tail.
const SCROLL_HINT: &str = "↑ earlier output";
/// Columns the metadata needs before the hint is allowed to share the row;
/// below this the hint is dropped and the metadata keeps the full width.
const MIN_METADATA_WIDTH: u16 = 10;

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
        draw_command_menu(frame, app, bottom, theme);
        draw_model_picker(frame, app, bottom, theme);
        draw_session_picker(frame, app, bottom, theme);
    }
    draw_footer(frame, app, footer, theme);
}

/// Floats the slash-command completion menu directly above the composer while
/// a command name is being typed. Painted after (over) the conversation, like
/// a dropdown; the highlighted row is what Enter will run, so the marker must
/// survive `NO_COLOR` (a `❯` glyph, not color alone).
fn draw_command_menu(frame: &mut Frame, app: &App, anchor: Rect, theme: Theme) {
    let Some(menu) = command::completions(&app.input) else {
        return;
    };
    if menu.is_empty() {
        return;
    }
    let rows = menu.len().min(MENU_MAX_ROWS) as u16;
    let height = rows.saturating_add(2).min(anchor.y); // borders; clipped on tiny frames
    if height < 3 {
        return;
    }
    let area = Rect::new(anchor.x, anchor.y - height, anchor.width, height);
    let selected = app.menu_selection.min(menu.len() - 1);
    let name_width = menu.iter().map(|spec| spec.name.len()).max().unwrap_or(0);
    let lines: Vec<Line> = menu
        .iter()
        .take(rows as usize)
        .enumerate()
        .map(|(index, spec)| {
            let name = format!("/{:<name_width$}", spec.name);
            if index == selected {
                Line::from(vec![
                    Span::styled("❯ ", theme.accent_strong()),
                    Span::styled(name, theme.accent_strong()),
                    Span::styled(format!("  {}", spec.description), theme.muted()),
                ])
            } else {
                Line::from(vec![
                    Span::raw("  "),
                    Span::raw(name),
                    Span::styled(format!("  {}", spec.description), theme.muted()),
                ])
            }
        })
        .collect();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::new()
                .borders(Borders::ALL)
                .title(Line::from(" Commands ").style(theme.accent_strong()))
                .border_style(theme.divider()),
        ),
        area,
    );
}

/// Floats the `/model` selection dialog above the composer, in the command
/// menu's spot (the two never show together: the picker only opens after the
/// composer was cleared, which closes the menu). Same `NO_COLOR`-surviving
/// `❯` marker; the active model is annotated so re-picking it reads as a
/// deliberate no-op.
fn draw_model_picker(frame: &mut Frame, app: &App, anchor: Rect, theme: Theme) {
    let Some(picker) = &app.model_picker else {
        return;
    };
    let rows = picker.options.len().min(MENU_MAX_ROWS) as u16;
    let height = rows.saturating_add(2).min(anchor.y); // borders; clipped on tiny frames
    if height < 3 {
        return;
    }
    let area = Rect::new(anchor.x, anchor.y - height, anchor.width, height);
    let lines: Vec<Line> = picker
        .options
        .iter()
        .take(rows as usize)
        .enumerate()
        .map(|(index, name)| {
            let mut spans = if index == picker.selected {
                vec![
                    Span::styled("❯ ", theme.accent_strong()),
                    Span::styled(name.clone(), theme.accent_strong()),
                ]
            } else {
                vec![Span::raw("  "), Span::raw(name.clone())]
            };
            if *name == app.model_name {
                spans.push(Span::styled("  (current)", theme.muted()));
            }
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::new()
                .borders(Borders::ALL)
                .title(
                    Line::from(" Model — Enter to switch, Esc to cancel ")
                        .style(theme.accent_strong()),
                )
                .border_style(theme.divider()),
        ),
        area,
    );
}

/// Floats the `/resume` session picker above the composer, in the same spot
/// as the other dialogs (they never show together). Unlike the model picker's
/// handful of options, a session listing can far exceed the panel, so the
/// drawn rows window around the highlight to keep it always visible; the
/// session this process is running is annotated so re-picking it reads as a
/// deliberate no-op.
fn draw_session_picker(frame: &mut Frame, app: &App, anchor: Rect, theme: Theme) {
    let Some(picker) = &app.session_picker else {
        return;
    };
    let rows = picker.sessions.len().min(MENU_MAX_ROWS);
    let height = (rows as u16).saturating_add(2).min(anchor.y); // borders; clipped on tiny frames
    if height < 3 {
        return;
    }
    let area = Rect::new(anchor.x, anchor.y - height, anchor.width, height);
    // Stateless window derived from the selection alone: top-anchored while
    // the highlight fits, then the highlight rides the bottom row.
    let start = picker.selected.saturating_sub(rows.saturating_sub(1));
    let lines: Vec<Line> = picker
        .sessions
        .iter()
        .enumerate()
        .skip(start)
        .take(rows)
        .map(|(index, session)| {
            let label = crate::resume::session_label(session);
            let mut spans = if index == picker.selected {
                vec![
                    Span::styled("❯ ", theme.accent_strong()),
                    Span::styled(label, theme.accent_strong()),
                ]
            } else {
                vec![Span::raw("  "), Span::raw(label)]
            };
            if picker.current == Some(index) {
                spans.push(Span::styled("  (current)", theme.muted()));
            }
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::new()
                .borders(Borders::ALL)
                .title(
                    Line::from(" Resume session — Enter to load, Esc to cancel ")
                        .style(theme.accent_strong()),
                )
                .border_style(theme.divider()),
        ),
        area,
    );
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
        Status::Idle => Line::from(vec![
            Span::styled("●", theme.success()),
            Span::raw(" Ready"),
        ]),
        Status::Running => Line::from(vec![
            Span::styled(app.activity_glyph(), theme.accent()),
            Span::raw(" Working"),
        ]),
        Status::Compacting => Line::from(vec![
            Span::styled(app.activity_glyph(), theme.warning()),
            Span::raw(" Compacting"),
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
    // Only the top border is drawn, so rows get the panel's full width.
    let inner_width = area.width.max(1);
    let rows: Vec<Line> = visible
        .into_iter()
        .map(|task| plan_item_line(task, inner_width, theme))
        .collect();
    let title = format!(" Plan {completed}/{} ", app.plan.len());
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
    // The caret may sit one row past the wrapped text (exact-width fill); give
    // that row real height so the composer never scrolls the text out to show it.
    let (caret_row, _) = caret_position(&app.input[..app.cursor], content_width);
    rows.max(caret_row.saturating_add(1))
        .clamp(1, INPUT_MAX_ROWS)
        .saturating_add(2)
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
    let title = match app.status {
        Status::Idle => " Prompt ",
        Status::Running => " Working ",
        Status::Compacting => " Compacting ",
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
            Status::Idle => "Describe what you want to get done",
            Status::Running => "Waiting for the current turn to finish",
            Status::Compacting => "Compacting session context",
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
///
/// A caret that lands exactly on `inner_width` advances to the start of the
/// next visual row — that is where the next typed char will render, and it
/// keeps the cursor from covering the row's last cell. [`input_height`] and the
/// composer's scroll both count that row via `caret_position`, so it is always
/// on screen even when it is past the wrapped text.
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
    if col >= inner_width {
        row = row.saturating_add(1);
        col = 0;
    }
    (row, col)
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mode_style = if app.mode == PermissionMode::BypassPermissions {
        theme.warning()
    } else {
        theme.muted()
    };

    // The scroll hint takes the left end, so the metadata is fitted to whatever
    // is actually left for it rather than to the whole row. The hint's slot is
    // measured from the hint itself: a fixed width silently clipped it to
    // "↑ earlier outp".
    let hint_width = display_width(SCROLL_HINT);
    let metadata_area = if !app.follow && area.width >= hint_width + MIN_METADATA_WIDTH {
        let [left, right] =
            Layout::horizontal([Constraint::Length(hint_width + 1), Constraint::Min(0)])
                .areas(area);
        frame.render_widget(
            Paragraph::new(Line::from(SCROLL_HINT).style(theme.warning())),
            left,
        );
        right
    } else {
        area
    };
    let metadata = footer_metadata(app, metadata_area.width.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(Line::from(metadata).style(mode_style)).alignment(Alignment::Right),
        metadata_area,
    );
}

/// Right-hand status text, fitted to `width` by dropping whole segments instead
/// of truncating mid-label — a clipped `cach…` reads as a broken number.
///
/// Segments run least to most important, and dropping starts from the left, so
/// a narrow terminal keeps the model and mode the run is actually using and a
/// wide one also shows what the context is costing.
fn footer_metadata(app: &App, width: u16) -> String {
    let mut segments = usage_segments(&app.session_usage);
    segments.push(app.model_name.clone());
    segments.push(mode_label(app.mode).to_string());

    while segments.len() > 1 && display_width(&segments.join(" · ")) > width {
        segments.remove(0);
    }
    // One segment may still overflow (a long model name on a narrow frame);
    // only then is a truncation the lesser evil.
    truncate_display(&segments.join(" · "), width)
}

/// Token counters worth showing, or nothing at all before the first response.
///
/// The cache share is the point of the whole prefix-stability design, so it is
/// listed last: it is the segment that survives longest as the frame narrows.
fn usage_segments(usage: &Usage) -> Vec<String> {
    if usage.input_tokens == 0 && usage.output_tokens == 0 {
        return Vec::new();
    }
    let mut segments = vec![
        format!("in {}", format_tokens(usage.input_tokens)),
        format!("out {}", format_tokens(usage.output_tokens)),
    ];
    // Providers report cached tokens as a subset of the input count, so the
    // share is meaningful only once input has been counted.
    if usage.input_tokens > 0 && usage.cached_input_tokens > 0 {
        let percent = usage.cached_input_tokens.saturating_mul(100) / usage.input_tokens;
        segments.push(format!("cache {percent}%"));
    }
    segments
}

/// Abbreviates a token count to at most four columns so the footer's width
/// stays predictable as the numbers grow. Precision drops with magnitude: a
/// status line is for noticing trends, not for accounting.
fn format_tokens(tokens: u64) -> String {
    match tokens {
        0..1_000 => tokens.to_string(),
        1_000..10_000 => one_decimal(tokens, 1_000, 'k'),
        10_000..1_000_000 => format!("{}k", tokens / 1_000),
        1_000_000..10_000_000 => one_decimal(tokens, 1_000_000, 'M'),
        10_000_000..1_000_000_000 => format!("{}M", tokens / 1_000_000),
        _ => one_decimal(tokens, 1_000_000_000, 'G'),
    }
}

/// One decimal place, floored rather than rounded: `{:.1}` would turn 9_999
/// into `10.0k` and silently widen the field the bands exist to bound.
fn one_decimal(tokens: u64, unit: u64, suffix: char) -> String {
    let tenths = tokens / (unit / 10);
    format!("{}.{}{suffix}", tenths / 10, tenths % 10)
}

fn approval_lines(approval: &ApprovalRequest, width: u16, theme: Theme) -> Vec<Line<'static>> {
    let detail_rows = if width >= 46 { 2 } else { 1 };
    let summary = truncate_display(
        &format!("Action  {}", approval.summary),
        width.saturating_mul(detail_rows),
    );
    let scope = truncate_display(
        &format!("Scope  {}", approval.persistence_label()),
        width.saturating_mul(detail_rows),
    );
    let mut lines = wrap_lines(
        vec![
            Line::from(summary).style(theme.warning()),
            Line::from(scope).style(theme.muted()),
        ],
        width,
    );

    let mut actions = vec![("y", "allow once")];
    if approval.allow_session.is_some() {
        actions.push(("a", "allow for session"));
    }
    actions.push(("n", "deny once"));
    if approval.deny_session.is_some() {
        actions.push(("d", "deny for session"));
    }
    actions.push(("Esc", "cancel turn"));
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
            .title(Line::from(" Approval required ").style(theme.warning()))
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
    fn command_menu_pops_up_while_typing_a_command_name() {
        let mut app = App::new("m", PermissionMode::Default);
        app.set_input("/".to_string());
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).expect("test terminal");

        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");

        let rendered = format!("{}", terminal.backend());
        assert!(rendered.contains("Commands"), "menu panel:\n{rendered}");
        assert!(
            rendered.contains("❯ /help"),
            "first row starts highlighted:\n{rendered}"
        );
        assert!(rendered.contains("/quit"));

        // Narrowing the prefix filters rows; the highlight follows the clamp.
        app.set_input("/q".to_string());
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
        let rendered = format!("{}", terminal.backend());
        assert!(rendered.contains("❯ /quit"));
        assert!(
            !rendered.contains("/help"),
            "help filtered out:\n{rendered}"
        );

        // Leaving the command-name position hides the menu entirely.
        app.set_input("hello".to_string());
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
        assert!(!format!("{}", terminal.backend()).contains("Commands"));
    }

    #[test]
    fn model_picker_renders_options_with_the_current_model_annotated() {
        let mut app = App::new("deepseek-v4-flash", PermissionMode::Default);
        app.available_models = vec![
            "deepseek-v4-pro".to_string(),
            "deepseek-v4-flash".to_string(),
        ];
        app.open_model_picker();
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).expect("test terminal");

        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");

        let rendered = format!("{}", terminal.backend());
        assert!(rendered.contains("Model"), "picker panel:\n{rendered}");
        assert!(
            rendered.contains("❯ deepseek-v4-flash"),
            "the active model starts highlighted:\n{rendered}"
        );
        assert!(
            rendered.contains("(current)"),
            "the active model is annotated:\n{rendered}"
        );
        assert!(rendered.contains("deepseek-v4-pro"));

        // Closing the picker removes the panel.
        app.model_picker = None;
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
        assert!(!format!("{}", terminal.backend()).contains("(current)"));
    }

    #[test]
    fn session_picker_renders_and_windows_around_the_selection() {
        use kuncode_agent::session_store::{SessionId, SessionSummary};

        let mut app = App::new("m", PermissionMode::Default);
        let sessions: Vec<SessionSummary> = (0..12u64)
            .map(|index| SessionSummary {
                id: SessionId::new(format!("session-{index:02}")),
                title: None,
                created_at: "2026-08-10T00:00:00.000Z".to_string(),
                updated_at: "2026-08-10T00:00:00.000Z".to_string(),
                message_count: index,
                preview: Some(format!("task number {index:02}")),
            })
            .collect();
        let active = SessionId::new("session-00");
        app.open_session_picker(sessions, Some(&active));
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("test terminal");

        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");

        let rendered = format!("{}", terminal.backend());
        assert!(
            rendered.contains("Resume session"),
            "picker panel:\n{rendered}"
        );
        assert!(
            rendered.contains("❯ ") && rendered.contains("task number 00"),
            "the active session starts highlighted:\n{rendered}"
        );
        assert!(
            rendered.contains("(current)"),
            "the active session is annotated:\n{rendered}"
        );
        assert!(
            !rendered.contains("task number 11"),
            "rows beyond the window stay hidden:\n{rendered}"
        );

        // Moving the highlight past the window slides later rows into view.
        app.session_picker.as_mut().expect("open").selected = 11;
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
        let rendered = format!("{}", terminal.backend());
        assert!(rendered.contains("task number 11"));
        assert!(!rendered.contains("task number 00"));

        // Closing the picker removes the panel.
        app.session_picker = None;
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
        assert!(!format!("{}", terminal.backend()).contains("Resume session"));
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
        assert!(rendered.contains("Plan 1/2"), "plan progress is shown");
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
            !rendered.contains("Plan 2/2"),
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

        assert!(rendered.contains("Plan 8/10"));
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
            assert!(rendered.contains("Approval required"));
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
        // Exact fill advances to the next visual row — where the next char will
        // render — instead of parking the cursor on the last cell.
        assert_eq!(caret_position("abcd", 4), (1, 0));
        // Overflow advances a row.
        assert_eq!(caret_position("abcde", 4), (1, 1));
        // Spaces don't get special word-break treatment: char-wrap, same as render.
        assert_eq!(caret_position("word word", 4), (2, 1));
        // Wide (CJK) glyphs: width 3 fits one per row.
        assert_eq!(caret_position("你你你", 3), (2, 2));
        // Explicit newline starts a fresh row.
        assert_eq!(caret_position("ab\nc", 4), (1, 1));
        // An exact fill followed by '\n' must not double-advance: the newline
        // itself is the row break.
        assert_eq!(caret_position("abcd\n", 4), (1, 0));
    }

    #[test]
    fn caret_row_stays_within_rows_the_composer_reserves() {
        // The caret may exceed the wrapped line count by at most one phantom row
        // (exact-width fill, caret at column 0). `input_height` reserves that row,
        // so anything beyond it would scroll the box blank.
        for input in ["", "abcd", "abcde", "word word", "你你你", "a\nbb\nccc"] {
            let (row, col) = caret_position(input, 4);
            let logical: Vec<Line> = input
                .split('\n')
                .map(|s| Line::raw(s.to_string()))
                .collect();
            let rendered = wrap_lines(logical, 4).len() as u16;
            assert!(
                row < rendered || (row == rendered && col == 0),
                "{input:?}: caret ({row}, {col}) outside {rendered} rendered rows + 1"
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
    fn exact_width_input_puts_the_cursor_past_the_last_char_not_on_it() {
        // Terminal width 10 → composer text width is 10 - 2 (border) - 2 (prompt)
        // = 6, so "abcdef" fills its first visual row exactly.
        let mut app = App::new("m", PermissionMode::Default);
        for c in "abcdef".chars() {
            app.insert_char(c);
        }

        let mut terminal = Terminal::new(TestBackend::new(10, 12)).expect("terminal");
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");

        let cursor = terminal.get_cursor_position().expect("cursor position");
        let buffer = terminal.backend().buffer();
        let f_cell = (0..12u16)
            .flat_map(|y| (0..10u16).map(move |x| (x, y)))
            .find(|&(x, y)| buffer.cell((x, y)).expect("cell").symbol() == "f")
            .expect("'f' should be rendered");
        assert_eq!(
            (cursor.x, cursor.y),
            (f_cell.0 - 5, f_cell.1 + 1),
            "caret starts the next visual row instead of covering 'f'"
        );
    }

    #[test]
    fn plan_rows_use_the_full_panel_width() {
        // The plan panel draws only a top border, so a row of exactly
        // panel-width cells (" ▸ " prefix + 21 chars on a 24-wide frame) must
        // render untruncated. The old `width - 2` maths would ellipsize it.
        let mut app = App::new("m", PermissionMode::Default);
        let task = "ABCDEFGHIJKLMNOPQRSTU";
        app.plan = vec![TodoItem {
            content: task.to_string(),
            active_form: task.to_string(),
            status: TodoStatus::InProgress,
        }];

        let mut terminal = Terminal::new(TestBackend::new(24, 14)).expect("terminal");
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
        let rendered = format!("{}", terminal.backend());
        assert!(
            rendered.contains(task),
            "an exactly-full-width plan row renders untruncated"
        );
    }

    fn usage(input: u64, output: u64, cached: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cached_input_tokens: cached,
            ..Usage::default()
        }
    }

    #[test]
    fn a_fresh_session_shows_only_the_model_and_mode() {
        let app = App::new("deepseek-v4-flash", PermissionMode::Default);
        assert_eq!(footer_metadata(&app, 80), "deepseek-v4-flash · default");
    }

    #[test]
    fn footer_reports_tokens_and_the_cache_share() {
        let mut app = App::new("deepseek-v4-flash", PermissionMode::Default);
        app.add_usage(usage(12_345, 2_100, 11_600));

        assert_eq!(
            footer_metadata(&app, 80),
            "in 12k · out 2.1k · cache 93% · deepseek-v4-flash · default",
        );
    }

    #[test]
    fn a_provider_without_cache_reporting_omits_the_share() {
        let mut app = App::new("gpt-test", PermissionMode::Default);
        app.add_usage(usage(900, 120, 0));

        assert_eq!(
            footer_metadata(&app, 80),
            "in 900 · out 120 · gpt-test · default"
        );
    }

    #[test]
    fn a_narrow_footer_drops_whole_segments_least_important_first() {
        let mut app = App::new("deepseek-v4-flash", PermissionMode::Default);
        app.add_usage(usage(12_345, 2_100, 11_600));

        // Wide enough for the cache share but not the raw counts.
        assert_eq!(
            footer_metadata(&app, 45),
            "cache 93% · deepseek-v4-flash · default",
        );
        // Only the identity survives; no label is cut in half.
        assert_eq!(footer_metadata(&app, 30), "deepseek-v4-flash · default");
        assert_eq!(footer_metadata(&app, 12), "default");
    }

    #[test]
    fn one_oversized_segment_is_truncated_rather_than_dropped() {
        let app = App::new("a-very-long-model-identifier", PermissionMode::Default);
        let metadata = footer_metadata(&app, 5);

        assert_eq!(display_width(&metadata), 5, "{metadata}");
        assert!(metadata.ends_with('…'), "{metadata}");
    }

    #[test]
    fn the_scroll_hint_and_the_metadata_share_the_footer_row() {
        let mut app = App::new("m", PermissionMode::Default);
        app.add_usage(usage(12_345, 2_100, 11_600));
        // The hint only claims the left end once there is history to be off
        // the tail of; an empty transcript re-follows on the next draw.
        for line in 0..20 {
            app.push_user(format!("line {line}"));
        }
        app.scroll_up(50);

        let mut terminal = Terminal::new(TestBackend::new(60, 12)).expect("test terminal");
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");

        let rendered = format!("{}", terminal.backend());
        assert!(rendered.contains("↑ earlier output"), "{rendered}");
        assert!(rendered.contains("cache 93%"), "{rendered}");
    }

    #[test]
    fn token_counts_abbreviate_within_four_columns() {
        // Band edges included: rounding at one of these is what widens a field.
        for tokens in [
            0,
            999,
            1_000,
            9_999,
            10_000,
            999_999,
            1_000_000,
            9_999_999,
            10_000_000,
            999_999_999,
        ] {
            let formatted = format_tokens(tokens);
            assert!(
                display_width(&formatted) <= 4,
                "{tokens} rendered as {formatted}",
            );
        }
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1.0k");
        assert_eq!(
            format_tokens(9_999),
            "9.9k",
            "floored, not rounded to 10.0k"
        );
        assert_eq!(format_tokens(12_345), "12k");
        assert_eq!(format_tokens(999_999), "999k");
        assert_eq!(format_tokens(1_500_000), "1.5M");
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
        assert!(rendered.contains("Compacting"));
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
        assert!(rendered.contains("Context compacted"));
        assert!(!rendered.contains("98765"));
        assert!(!rendered.contains("12345"));
    }
}
