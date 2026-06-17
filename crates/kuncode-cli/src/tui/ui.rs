//! ratatui rendering: conversation log, input box, status line, approval modal.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Style, Stylize},
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Wrap},
};

use super::app::{App, Item, Status, ToolState, mode_label};
use super::bridge::ApprovalRequest;

/// Draws one frame: conversation body, input box (height grows with the buffer),
/// status line, and — when an approval is pending — a centered modal on top.
pub fn draw(frame: &mut Frame, app: &App) {
    let input_lines = app.input.split('\n').count().max(1) as u16;
    // Border (2) + content, capped so a long paste can't swallow the screen.
    let input_height = (input_lines + 2).min(8);

    let [body, input_area, status_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_conversation(frame, app, body);
    draw_input(frame, app, input_area);
    draw_status(frame, app, status_area);

    if let Some(approval) = &app.approval {
        draw_approval(frame, approval, frame.area());
    }
}

fn draw_conversation(frame: &mut Frame, app: &App, area: Rect) {
    let lines = conversation_lines(app);

    // Auto-follow the tail: scroll so the last line sits at the bottom. The wrap
    // count is estimated from each line's display width (`Line::width`, which is
    // unicode-aware) divided by the body's inner width, so long answers don't
    // push the latest reply off-screen. Word-wrap can use a row or two more than
    // this hard-division estimate; manual scroll-back lands in a later step.
    let inner_width = area.width.saturating_sub(2).max(1);
    let inner_height = area.height.saturating_sub(2);
    let total: u16 = lines
        .iter()
        .map(|line| (line.width() as u16).div_ceil(inner_width).max(1))
        .sum();
    let scroll = total.saturating_sub(inner_height);

    let para = Paragraph::new(Text::from(lines))
        .block(Block::bordered().title("kuncode"))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(para, area);
}

/// Flattens the conversation log into styled lines. Multi-line user/assistant
/// text is split so each physical line is its own [`Line`].
fn conversation_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    for item in &app.conversation {
        match item {
            Item::User(text) => {
                for (i, raw) in text.split('\n').enumerate() {
                    let prefix = if i == 0 { "› " } else { "  " };
                    lines.push(Line::from(format!("{prefix}{raw}")).bold());
                }
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
                lines.push(Line::from(format!("⏺ {name}  {summary}")).cyan());
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

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let title = if app.status == Status::Running {
        "input (运行中)"
    } else {
        "input"
    };
    let para = Paragraph::new(app.input.as_str())
        .block(Block::bordered().title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);

    // Show the cursor only when the user can type (idle, no modal).
    if app.status == Status::Idle && app.approval.is_none() {
        let rows = app.input.split('\n').count() as u16;
        let last = app.input.split('\n').next_back().unwrap_or("");
        let cursor_x = area.x + 1 + last.chars().count() as u16;
        let cursor_y = area.y + rows;
        frame.set_cursor_position(Position::new(cursor_x, cursor_y));
    }
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let (state, hint) = if app.status == Status::Running {
        ("运行中", "^C 取消")
    } else {
        ("就绪", "⏎ 发送 · ^C 退出")
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

fn draw_approval(frame: &mut Frame, approval: &ApprovalRequest, area: Rect) {
    let lines = vec![
        Line::from(format!("⚠ 需要授权: {}", approval.summary)),
        Line::from(""),
        Line::from(format!("记住规则: {}", approval.scope_rule())),
        Line::from(""),
        Line::from("[y] 允许一次  [a] 总是  [n] 否  [d] 永久拒绝  [c] 取消"),
    ];
    let width = 64.min(area.width.saturating_sub(4));
    let height = (lines.len() as u16 + 2).min(area.height);
    let rect = centered(width, height, area);

    let modal = Paragraph::new(Text::from(lines))
        .block(
            Block::bordered()
                .title("权限")
                .border_style(Style::new().yellow()),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(Clear, rect);
    frame.render_widget(modal, rect);
}

/// A `width`×`height` rectangle centered in `area`, clamped to fit.
fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}
