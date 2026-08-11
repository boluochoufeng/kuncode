//! Full-screen terminal UI built on ratatui + crossterm.
//!
//! Mirrors the observer/approval-resolver split: this is just another
//! frontend wired to the same agent runner. [`run`] owns the terminal lifecycle
//! (raw mode, alternate screen, panic-safe restore) and the single event loop
//! that folds the keyboard, the agent's event stream, and approval requests into
//! one `select!`.

mod app;
mod bridge;
mod command;
mod ui;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use futures_util::StreamExt;
use kuncode_agent::error::AgentError;
use kuncode_agent::observer::AgentEvent;
use kuncode_agent::runner::AgentRunner;
use kuncode_agent::session::AgentSession;
use kuncode_core::completion::CompletionModel;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio_util::sync::CancellationToken;

use self::app::{App, Status};
use self::bridge::{ApprovalRequest, TuiApprover, TuiObserver};
use crate::runtime::CliRuntime;

/// Rows scrolled per PageUp/PageDown.
const SCROLL_STEP: u16 = 10;

/// Rows scrolled per mouse-wheel notch.
const MOUSE_SCROLL_STEP: u16 = 3;

/// Redraw cadence while a turn streams (~30fps). This is the *only* redraw path
/// during a turn, so the screen refreshes at a fixed rate instead of once per
/// streamed token (the model pushes deltas far faster); it also paces the
/// typewriter via [`App::advance_reveal`](app::App::advance_reveal).
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// Typewriter reveal speed for streamed output, in chars/second.
const REVEAL_CPS: u32 = 80;

/// Cap on how long a turn keeps typing out a buffered tail after the model has
/// finished, before snapping to the full answer — so a long fast burst can't
/// delay the commit by more than this.
const MAX_DRAIN: Duration = Duration::from_millis(3000);

/// Runs the interactive TUI until the user quits.
///
/// Wraps the assembled runner pieces with the TUI's own observer + approver,
/// then enters raw mode + the alternate screen via [`ratatui::init()`] (which also
/// installs a panic hook that restores the terminal before unwinding) and
/// guarantees [`ratatui::restore`] on every exit path. Mouse capture and
/// bracketed paste ride a [`TerminalFeatures`] guard so a panic can't leave
/// them enabled in the user's shell.
pub async fn run<M>(runtime: CliRuntime<M>) -> io::Result<()>
where
    M: CompletionModel,
{
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (approval_tx, mut approval_rx) = mpsc::unbounded_channel();

    // Read the frontend-facing bits before `into_runner` consumes the runtime.
    let model_name = runtime.model_name().to_string();
    let mode = runtime.mode();
    let mut session = runtime
        .session()
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    let runner = runtime
        .into_runner(
            Arc::new(TuiApprover::new(approval_tx)),
            Arc::new(TuiObserver::new(event_tx)),
        )
        .map_err(io::Error::other)?;
    let mut app = App::new(model_name, mode);
    seed_transcript(&mut app, &session);

    let mut terminal = ratatui::init();
    let features = TerminalFeatures::enable();
    let result = event_loop(
        &mut terminal,
        &runner,
        &mut session,
        &mut app,
        &mut event_rx,
        &mut approval_rx,
    )
    .await;
    drop(features);
    let restore_result = ratatui::try_restore();
    if let Err(error) = &restore_result {
        log_tui_io("restore_terminal", error, true);
    }
    // The alternate screen is gone with everything it displayed; this print is
    // what survives in the user's scrollback.
    print_exit_report(&app, &session);
    match (result, restore_result) {
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// Prints the session's token usage and the command that resumes it, Codex-style.
///
/// Silent when the run neither consumed tokens nor has any history to resume,
/// so `kuncode` + immediate quit stays clean.
fn print_exit_report(app: &App, session: &AgentSession) {
    let usage = app.session_usage;
    let consumed = usage.total_tokens > 0 || usage.input_tokens > 0 || usage.output_tokens > 0;
    if consumed {
        let cached = if usage.cached_input_tokens > 0 {
            format!(" (cached {})", usage.cached_input_tokens)
        } else {
            String::new()
        };
        println!(
            "Token usage: input {}{cached} · output {} · total {}",
            usage.input_tokens, usage.output_tokens, usage.total_tokens,
        );
    }
    if let Some(id) = session.session_id()
        && !session.messages().is_empty()
    {
        println!(
            "To resume this session, run kuncode --resume={}",
            id.as_str()
        );
    }
}

/// Replays a resumed session's dialog into the transcript so the user sees
/// what they are continuing.
///
/// Only user and assistant text is replayed: tool exchanges were already
/// rendered live when they happened, and the compacted-context envelope is
/// collapsed to a marker line instead of its JSON payload. This is purely a
/// display decision — the envelope's shape grants it no authority.
fn seed_transcript(app: &mut App, session: &AgentSession) {
    use kuncode_core::completion::{AssistantContent, Message, UserContent};

    if session.messages().is_empty() {
        return;
    }
    for message in session.messages() {
        match message {
            Message::User { content } => {
                if kuncode_agent::compaction::summary::is_compacted_context_message(message) {
                    app.push_assistant(
                        "(earlier conversation compacted into a summary)".to_string(),
                    );
                    continue;
                }
                for block in content.iter() {
                    if let UserContent::Text(text) = block {
                        app.push_user(text.text_ref().to_string());
                    }
                }
            }
            Message::Assistant { content, .. } => {
                for block in content.iter() {
                    if let AssistantContent::Text(text) = block {
                        app.push_assistant(text.text_ref().to_string());
                    }
                }
            }
            Message::System { .. } => {}
        }
    }
}

/// RAII for the optional terminal features (mouse capture + bracketed paste):
/// enabled on construction, disabled on drop, so *every* exit path restores
/// them — including a panic unwinding through [`run`]. The ratatui panic hook
/// only restores raw mode and the alternate screen; without this guard a panic
/// would leave the shell emitting `ESC[200~` paste markers and mouse sequences.
///
/// Best-effort on both sides: a terminal that refuses mouse capture just loses
/// wheel scrolling (PageUp/PageDown still work), and disable failures are
/// logged rather than escalated because drop runs on the error path too.
struct TerminalFeatures;

impl TerminalFeatures {
    fn enable() -> Self {
        if let Err(error) = execute!(io::stdout(), EnableMouseCapture) {
            log_tui_io("enable_mouse_capture", &error, false);
        }
        if let Err(error) = execute!(io::stdout(), EnableBracketedPaste) {
            log_tui_io("enable_bracketed_paste", &error, false);
        }
        Self
    }
}

impl Drop for TerminalFeatures {
    fn drop(&mut self) {
        if let Err(error) = execute!(io::stdout(), DisableBracketedPaste) {
            log_tui_io("disable_bracketed_paste", &error, false);
        }
        if let Err(error) = execute!(io::stdout(), DisableMouseCapture) {
            log_tui_io("disable_mouse_capture", &error, false);
        }
    }
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
        io_stage("idle_draw", terminal.draw(|frame| ui::draw(frame, app)))?;

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
            Some(Ok(Event::Paste(text))) => app.insert_paste(&text),
            Some(Ok(_)) => {} // resize / non-press keys
            Some(Err(error)) => return Err(log_tui_io("idle_input", &error, true)),
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
    crate::logging::log_prompt_preview(&input);
    let cancel = CancellationToken::new();
    let mut outcome = None;
    // Once the input stream ends, stop selecting on it so a perpetually-ready
    // `None` can't busy-spin the loop until the turn finishes.
    let mut events_closed = false;

    // A steady frame clock owns redraws for the whole turn (loop + final drain),
    // decoupling the screen refresh rate from the much faster delta arrival.
    let mut frame = tokio::time::interval(FRAME_INTERVAL);
    frame.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    {
        let mut turn = Box::pin(runner.run_turn_with(session, input, cancel.clone()));
        // Paint the running state immediately; subsequent redraws ride the clock.
        io_stage(
            "turn_initial_draw",
            terminal.draw(|frame| ui::draw(frame, app)),
        )?;
        while outcome.is_none() {
            tokio::select! {
                result = &mut turn => outcome = Some(result),
                // The frame tick is the only redraw path: deltas merely accumulate
                // into the preview, and the typewriter + repaint happen here at a
                // fixed cadence rather than once per streamed token.
                _ = frame.tick() => {
                    app.advance_animation();
                    app.advance_reveal(FRAME_INTERVAL, REVEAL_CPS);
                    io_stage(
                        "turn_stream_draw",
                        terminal.draw(|frame| ui::draw(frame, app)),
                    )?;
                }
                Some(event) = event_rx.recv() => app.apply_event(event.kind),
                Some(req) = approval_rx.recv() => app.set_approval(req),
                maybe = events.next(), if !events_closed => {
                    match maybe {
                        Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                            handle_running_key(app, key, &cancel);
                        }
                        Some(Ok(Event::Mouse(mouse))) => handle_scroll(app, mouse),
                        // Mirror the idle loop: a stream error means the terminal
                        // IO broke, so unwind to the shared restore-and-exit path
                        // rather than swallowing it (and risking a busy redraw on a
                        // persistently-ready error).
                        Some(Err(error)) => {
                            return Err(log_tui_io("turn_input", &error, true));
                        }
                        None => events_closed = true,
                        _ => {}
                    }
                }
            }
        }
    }

    // The turn's final poll may have enqueued tool/assistant events that `select!`
    // never consumed before the `result` branch fired. The idle loop doesn't drain
    // `event_rx`, so flush them here — otherwise the last rows of a fast turn would
    // leak into the next one (or never render).
    while let Ok(event) = event_rx.try_recv() {
        app.apply_event(event.kind);
    }

    match outcome.expect("loop exits only once outcome is set") {
        Ok(turn) => {
            app.add_usage(turn.usage);
            let text = turn.final_text(session);
            // Keep typing out whatever the typewriter hasn't shown yet, so a fast
            // stream finishes at the reading pace instead of snapping to the full
            // answer — but cap the wait so a long tail can't stall the commit.
            let mut waited = Duration::ZERO;
            while app.has_pending_reveal() && waited < MAX_DRAIN {
                frame.tick().await;
                app.advance_reveal(FRAME_INTERVAL, REVEAL_CPS);
                io_stage(
                    "turn_drain_draw",
                    terminal.draw(|frame| ui::draw(frame, app)),
                )?;
                waited += FRAME_INTERVAL;
            }
            app.push_assistant(text);
        }
        Err(AgentError::Cancelled) => app.push_error("cancelled".to_string()),
        Err(err) => app.push_error(err.to_string()),
    }

    Ok(())
}

fn io_stage<T>(stage: &str, result: io::Result<T>) -> io::Result<T> {
    result.map_err(|error| log_tui_io(stage, &error, true))
}

fn log_tui_io(stage: &str, error: &io::Error, fatal: bool) -> io::Error {
    if fatal {
        tracing::error!(
            target: "kuncode::runtime",
            component = "tui",
            stage,
            io_kind = ?error.kind(),
            diagnostic_chars = error.to_string().chars().count(),
            "terminal I/O failed",
        );
    } else {
        tracing::warn!(
            target: "kuncode::runtime",
            component = "tui",
            stage,
            io_kind = ?error.kind(),
            diagnostic_chars = error.to_string().chars().count(),
            "optional terminal feature unavailable",
        );
    }
    io::Error::new(error.kind(), error.to_string())
}

/// Handles a key in the idle state. Returns `Some(input)` when Enter submits a
/// non-empty buffer; otherwise edits the buffer (or sets `should_quit`).
fn handle_idle_key(app: &mut App, key: KeyEvent) -> Option<String> {
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            app.should_quit = true;
            None
        }
        // Emacs-style line motion, the muscle-memory aliases for Home/End.
        (KeyModifiers::CONTROL, KeyCode::Char('a')) => {
            app.move_home();
            None
        }
        (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
            app.move_end();
            None
        }
        (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
            app.insert_newline();
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
        // Cursor movement within the input box. Up/Down move by logical line;
        // PageUp/PageDown (above) stay reserved for scrolling the conversation.
        (_, KeyCode::Left) => {
            app.move_left();
            None
        }
        (_, KeyCode::Right) => {
            app.move_right();
            None
        }
        (_, KeyCode::Up) => {
            app.move_up();
            None
        }
        (_, KeyCode::Down) => {
            app.move_down();
            None
        }
        (_, KeyCode::Home) => {
            app.move_home();
            None
        }
        (_, KeyCode::End) => {
            app.move_end();
            None
        }
        (_, KeyCode::Delete) => {
            app.delete();
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
                // it to the agent. Kept as an alias of `/quit`.
                app.should_quit = true;
                None
            } else {
                app.follow_tail();
                // Take the buffer before dispatch: a command submission clears
                // the composer exactly like a prompt submission does.
                let submitted = app.take_input();
                match command::dispatch(app, &submitted) {
                    command::Dispatch::Handled => None,
                    command::Dispatch::Prompt => Some(submitted),
                }
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
        // Ctrl-chords the app doesn't bind must not leak their letter into the
        // buffer (Ctrl+K typing a literal 'k'). Shift/Alt still insert: many
        // terminals report them alongside ordinary composed characters. And
        // CONTROL+ALT passes through as well — Windows reports AltGr as
        // LEFT_CTRL+RIGHT_ALT, so European layouts type @ { [ € with exactly
        // that pairing; blocking it would make those chars unenterable.
        (m, KeyCode::Char(c))
            if !m.contains(KeyModifiers::CONTROL) || m.contains(KeyModifiers::ALT) =>
        {
            app.insert_char(c);
            None
        }
        _ => None,
    }
}

/// Handles a key while a turn runs: answer the approval modal if one is open,
/// else let Ctrl-C cancel the turn. Scrolling stays available either way —
/// deciding an approval often means checking the conversation above it, and
/// the mouse wheel already scrolls during a modal.
fn handle_running_key(app: &mut App, key: KeyEvent, cancel: &CancellationToken) {
    match key.code {
        KeyCode::PageUp => return app.scroll_up(SCROLL_STEP),
        KeyCode::PageDown => return app.scroll_down(SCROLL_STEP),
        _ => {}
    }
    if app.approval.is_some() {
        app.handle_approval_key(key);
        return;
    }
    if (key.modifiers, key.code) == (KeyModifiers::CONTROL, KeyCode::Char('c')) {
        cancel.cancel();
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
    use kuncode_agent::permission::PermissionMode;

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

    #[test]
    fn typing_slash_quit_then_enter_quits_without_submitting() {
        let mut app = App::new("m", PermissionMode::Default);
        typing(&mut app, "/quit");
        assert!(handle_idle_key(&mut app, enter()).is_none());
        assert!(app.should_quit, "/quit should quit the TUI");
    }

    #[test]
    fn typing_slash_help_then_enter_shows_help_without_submitting() {
        let mut app = App::new("m", PermissionMode::Default);
        typing(&mut app, "/help");
        assert!(handle_idle_key(&mut app, enter()).is_none());
        assert!(!app.should_quit);
        assert!(
            app.input.is_empty(),
            "a command submission clears the composer like a prompt"
        );
        assert!(
            app.conversation
                .iter()
                .any(|item| matches!(item, app::Item::Notice(_))),
            "help output should land in the transcript"
        );
    }

    #[test]
    fn unknown_slash_command_notices_instead_of_submitting() {
        let mut app = App::new("m", PermissionMode::Default);
        typing(&mut app, "/frobnicate");
        assert!(handle_idle_key(&mut app, enter()).is_none());
        assert!(!app.should_quit);
        assert!(
            app.conversation.iter().any(
                |item| matches!(item, app::Item::Notice(text) if text.contains("unknown command"))
            ),
            "an unknown command should push a notice instead of running a turn"
        );
    }

    #[test]
    fn page_keys_scroll_even_while_an_approval_is_pending() {
        let mut app = App::new("m", PermissionMode::Default);
        let (respond, _rx) = tokio::sync::oneshot::channel();
        app.set_approval(crate::tui::bridge::ApprovalRequest {
            summary: "run command".to_string(),
            targets: vec!["Bash(date)".to_string()],
            allow_session: None,
            deny_session: None,
            respond,
        });
        let cancel = CancellationToken::new();

        handle_running_key(
            &mut app,
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::empty()),
            &cancel,
        );

        assert!(!app.follow, "PageUp scrolls instead of being swallowed");
        assert!(app.approval.is_some(), "the modal stays pending");
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn unbound_control_chords_do_not_insert_their_letter() {
        let mut app = App::new("m", PermissionMode::Default);
        assert!(
            handle_idle_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            )
            .is_none()
        );
        assert!(app.input.is_empty(), "Ctrl+K must not type a literal 'k'");
    }

    #[test]
    fn altgr_composed_characters_still_insert() {
        // Windows reports AltGr as CONTROL|ALT; the composed char must insert
        // or European layouts lose @ { [ € entirely.
        let mut app = App::new("m", PermissionMode::Default);
        assert!(
            handle_idle_key(
                &mut app,
                KeyEvent::new(
                    KeyCode::Char('@'),
                    KeyModifiers::CONTROL | KeyModifiers::ALT,
                ),
            )
            .is_none()
        );
        assert_eq!(app.input, "@", "AltGr-composed '@' must be typed");
    }

    #[test]
    fn control_j_inserts_a_reliable_newline() {
        let mut app = App::new("m", PermissionMode::Default);
        typing(&mut app, "first");

        assert!(
            handle_idle_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            )
            .is_none()
        );
        typing(&mut app, "second");

        assert_eq!(app.input, "first\nsecond");
    }
}
