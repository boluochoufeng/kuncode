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

#[cfg(test)]
use self::conversation::{USER_BG, display_tool_name};
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
mod tests;
