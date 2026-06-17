//! Full-screen terminal UI built on ratatui + crossterm.
//!
//! Mirrors the existing `Observer`/`Approver` split: this is just another
//! frontend wired to the same agent runner. [`run`] owns the terminal lifecycle
//! (raw mode, alternate screen, panic-safe restore) and the single event loop
//! that folds the keyboard, the agent's event stream, and approval requests into
//! one `select!`.

mod app;
mod bridge;
mod ui;

use std::io;
use std::sync::Arc;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use futures_util::StreamExt;
use kuncode_agent::error::AgentError;
use kuncode_agent::observer::AgentEvent;
use kuncode_agent::permission::{PermissionMode, PermissionPolicy};
use kuncode_agent::registry::ToolRegistry;
use kuncode_agent::runner::{AgentConfig, AgentRunner};
use kuncode_agent::session::AgentSession;
use kuncode_core::completion::CompletionModel;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio_util::sync::CancellationToken;

use self::app::{App, Status};
use self::bridge::{ApprovalRequest, TuiApprover, TuiObserver};

/// Rows scrolled per PageUp/PageDown.
const SCROLL_STEP: u16 = 10;

/// Rows scrolled per mouse-wheel notch.
const MOUSE_SCROLL_STEP: u16 = 3;

/// Runs the interactive TUI until the user quits.
///
/// Wraps the assembled runner pieces with the TUI's own observer + approver,
/// then enters raw mode + the alternate screen via [`ratatui::init()`] (which also
/// installs a panic hook that restores the terminal before unwinding) and
/// guarantees [`ratatui::restore`] on every exit path.
pub async fn run<M>(
    model: M,
    registry: ToolRegistry,
    config: AgentConfig,
    policy: PermissionPolicy,
    mode: PermissionMode,
    model_name: String,
) -> io::Result<()>
where
    M: CompletionModel,
{
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (approval_tx, mut approval_rx) = mpsc::unbounded_channel();

    let runner = AgentRunner::with_config(model, registry, config)
        .with_policy(policy)
        .with_approver(Arc::new(TuiApprover::new(approval_tx)))
        .with_observer(Arc::new(TuiObserver::new(event_tx)));
    let mut session = AgentSession::with_mode(mode);
    let mut app = App::new(model_name, mode);

    let mut terminal = ratatui::init();
    // Capture the mouse so the wheel scrolls the conversation instead of the
    // terminal's own scrollback. Best-effort: a terminal that refuses it just
    // loses wheel scrolling, and PageUp/PageDown still work.
    let _ = execute!(io::stdout(), EnableMouseCapture);
    let result = event_loop(
        &mut terminal,
        &runner,
        &mut session,
        &mut app,
        &mut event_rx,
        &mut approval_rx,
    )
    .await;
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Idle loop: render, read a key, and either edit the input box or — on submit —
/// hand off to [`run_one_turn`] for the duration of the turn.
async fn event_loop<M: CompletionModel>(
    terminal: &mut ratatui::DefaultTerminal,
    runner: &AgentRunner<M>,
    session: &mut AgentSession,
    app: &mut App,
    event_rx: &mut UnboundedReceiver<AgentEvent>,
    approval_rx: &mut UnboundedReceiver<ApprovalRequest>,
) -> io::Result<()> {
    let mut events = EventStream::new();

    while !app.should_quit {
        terminal.draw(|frame| ui::draw(frame, app))?;

        match events.next().await {
            Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                if let Some(input) = handle_idle_key(app, key) {
                    app.push_user(input.clone());
                    app.status = Status::Running;
                    run_one_turn(
                        terminal,
                        runner,
                        session,
                        app,
                        input,
                        &mut events,
                        event_rx,
                        approval_rx,
                    )
                    .await?;
                    app.status = Status::Idle;
                }
            }
            Some(Ok(Event::Mouse(mouse))) => handle_scroll(app, mouse),
            Some(Ok(_)) => {} // resize / non-press keys
            Some(Err(err)) => return Err(err),
            None => break, // stdin closed
        }
    }

    Ok(())
}

/// Drives one turn to completion, rendering the live event stream and servicing
/// approval modals and Ctrl-C cancel meanwhile.
///
/// The turn future borrows `session` mutably, so it is scoped to an inner block;
/// only after it is dropped is `session` free again to read the final answer.
#[allow(clippy::too_many_arguments)]
async fn run_one_turn<M: CompletionModel>(
    terminal: &mut ratatui::DefaultTerminal,
    runner: &AgentRunner<M>,
    session: &mut AgentSession,
    app: &mut App,
    input: String,
    events: &mut EventStream,
    event_rx: &mut UnboundedReceiver<AgentEvent>,
    approval_rx: &mut UnboundedReceiver<ApprovalRequest>,
) -> io::Result<()> {
    let cancel = CancellationToken::new();
    let mut outcome = None;

    {
        let mut turn = Box::pin(runner.run_turn_with(session, input, cancel.clone()));
        while outcome.is_none() {
            terminal.draw(|frame| ui::draw(frame, app))?;

            tokio::select! {
                result = &mut turn => outcome = Some(result),
                Some(event) = event_rx.recv() => app.apply_event(event.kind),
                Some(req) = approval_rx.recv() => app.set_approval(req),
                maybe = events.next() => {
                    match maybe {
                        Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                            handle_running_key(app, key, &cancel);
                        }
                        Some(Ok(Event::Mouse(mouse))) => handle_scroll(app, mouse),
                        _ => {}
                    }
                }
            }
        }
    }

    match outcome.expect("loop exits only once outcome is set") {
        Ok(turn) => {
            let text = turn.final_text(session);
            app.push_assistant(text);
        }
        Err(AgentError::Cancelled) => app.push_error("已取消".to_string()),
        Err(err) => app.push_error(err.to_string()),
    }

    Ok(())
}

/// Handles a key in the idle state. Returns `Some(input)` when Enter submits a
/// non-empty buffer; otherwise edits the buffer (or sets `should_quit`).
fn handle_idle_key(app: &mut App, key: KeyEvent) -> Option<String> {
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            app.should_quit = true;
            None
        }
        (_, KeyCode::PageUp) => {
            app.scroll_up(SCROLL_STEP);
            None
        }
        (_, KeyCode::PageDown) => {
            app.scroll_down(SCROLL_STEP);
            None
        }
        // Bare Enter submits; a modified Enter (Shift/Alt, where the terminal
        // reports it) inserts a newline for multi-line input.
        (m, KeyCode::Enter) if m.is_empty() => {
            let trimmed = app.input.trim();
            if trimmed.is_empty() {
                None
            } else if trimmed == "exit" {
                // `exit` is a REPL command, not a prompt: quit instead of sending
                // it to the agent.
                app.should_quit = true;
                None
            } else {
                app.follow_tail();
                Some(app.take_input())
            }
        }
        (_, KeyCode::Enter) => {
            app.insert_newline();
            None
        }
        (_, KeyCode::Backspace) => {
            app.backspace();
            None
        }
        (_, KeyCode::Char(c)) => {
            app.insert_char(c);
            None
        }
        _ => None,
    }
}

/// Handles a key while a turn runs: answer the approval modal if one is open,
/// else let Ctrl-C cancel the turn.
fn handle_running_key(app: &mut App, key: KeyEvent, cancel: &CancellationToken) {
    if app.approval.is_some() {
        app.handle_approval_key(key);
        return;
    }
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => cancel.cancel(),
        (_, KeyCode::PageUp) => app.scroll_up(SCROLL_STEP),
        (_, KeyCode::PageDown) => app.scroll_down(SCROLL_STEP),
        _ => {}
    }
}

/// Maps a mouse-wheel event to a conversation scroll. Effective only with mouse
/// capture enabled; otherwise the terminal handles the wheel itself.
fn handle_scroll(app: &mut App, mouse: MouseEvent) {
    match mouse.kind {
        MouseEventKind::ScrollUp => app.scroll_up(MOUSE_SCROLL_STEP),
        MouseEventKind::ScrollDown => app.scroll_down(MOUSE_SCROLL_STEP),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typing(app: &mut App, text: &str) {
        for c in text.chars() {
            app.insert_char(c);
        }
    }

    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty())
    }

    #[test]
    fn typing_exit_then_enter_quits_without_submitting() {
        let mut app = App::new("m", PermissionMode::Default);
        typing(&mut app, "  exit  "); // surrounding whitespace still counts
        assert!(handle_idle_key(&mut app, enter()).is_none());
        assert!(app.should_quit, "`exit` should quit the TUI");
    }

    #[test]
    fn enter_submits_a_normal_prompt() {
        let mut app = App::new("m", PermissionMode::Default);
        typing(&mut app, "exit now");
        assert_eq!(
            handle_idle_key(&mut app, enter()).as_deref(),
            Some("exit now")
        );
        assert!(!app.should_quit, "a prompt containing exit must not quit");
    }
}
