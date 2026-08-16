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
use kuncode_agent::runner::{AgentRunner, ManualCompaction};
use kuncode_agent::session::AgentSession;
use kuncode_agent::session_store::SessionId;
use kuncode_core::completion::{CompletionModel, RetryModel};
use kuncode_core::providers::any_chat::AnyChatCompletionModel;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio_util::sync::CancellationToken;

use self::app::{App, Status, mode_label, next_mode};
use self::bridge::{ApprovalRequest, TuiApprover, TuiObserver};
use crate::runtime::{CliRuntime, ModelSwitcher, SessionResumer};

/// The one model type the TUI runs. Concrete (unlike [`CliRuntime`]) because a
/// `/model` switch rebuilds the turn and summary models with two different
/// retry policies, which a generic `M::make` cannot express.
type CliModel = RetryModel<AnyChatCompletionModel>;

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

/// Upper bound on sessions offered by the `/resume` picker. Smaller than the
/// startup picker's limit: the panel is walked row-by-row with arrow keys, so
/// a longer listing is unnavigable anyway.
const SESSION_LISTING_LIMIT: usize = 50;

/// Runs the interactive TUI until the user quits.
///
/// Wraps the assembled runner pieces with the TUI's own observer + approver,
/// then enters raw mode + the alternate screen via [`ratatui::init()`] (which also
/// installs a panic hook that restores the terminal before unwinding) and
/// guarantees [`ratatui::restore`] on every exit path. Mouse capture and
/// bracketed paste ride a [`TerminalFeatures`] guard so a panic can't leave
/// them enabled in the user's shell.
pub async fn run(runtime: CliRuntime<CliModel>) -> io::Result<()> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (approval_tx, mut approval_rx) = mpsc::unbounded_channel();

    // Read the frontend-facing bits before `into_runner` consumes the runtime.
    let model_name = runtime.model_name().to_string();
    let mode = runtime.mode();
    let switcher = runtime.model_switcher();
    let resumer = runtime.session_resumer();
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
    app.available_models = switcher.known_models();
    seed_transcript(&mut app, &session);

    let mut terminal = ratatui::init();
    let features = TerminalFeatures::enable();
    let result = event_loop(
        &mut terminal,
        runner,
        &switcher,
        &resumer,
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
/// hand off to [`run_one_turn`] for the duration of the turn, or apply a model
/// switch between turns.
///
/// Owns the runner (nothing else references it) so a `/model` switch can
/// replace its model pair and config in place, keeping the approval broker's
/// session-scoped grants.
#[allow(clippy::too_many_arguments)]
async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    mut runner: AgentRunner<CliModel>,
    switcher: &ModelSwitcher,
    resumer: &SessionResumer,
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
                match handle_idle_key(app, key) {
                    Some(Submission::Prompt(input)) => {
                        app.push_user(input.clone());
                        app.status = Status::Running;
                        run_one_turn(
                            terminal,
                            &runner,
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
                    // Between turns by construction: the idle loop only reaches
                    // here while no turn is running. All-or-nothing — a rejected
                    // switch leaves the runner untouched.
                    Some(Submission::SwitchModel(name)) => match switcher.switch(&name) {
                        Ok(switch) => {
                            runner = runner
                                .with_model(switch.model)
                                .with_summary_model(switch.summary_model)
                                .with_agent_config(switch.config);
                            app.model_name = switch.model_name.clone();
                            app.push_notice(format!("model switched to {}", switch.model_name));
                            tracing::info!(
                                target: "kuncode::runtime",
                                model = %switch.model_name,
                                "model switched",
                            );
                        }
                        Err(error) => app.push_error(error.to_string()),
                    },
                    // Between turns like a model switch. The listing await
                    // blocks the loop, but the store is a local database —
                    // the stall is well under a frame.
                    Some(Submission::PickSession) => {
                        match resumer.list(SESSION_LISTING_LIMIT).await {
                            Ok(sessions) => app.open_session_picker(sessions, session.session_id()),
                            Err(error) => app.push_error(error.to_string()),
                        }
                    }
                    Some(Submission::Compact) => {
                        run_compaction(terminal, &runner, session, app, &mut events, event_rx)
                            .await?;
                    }
                    // Between turns, so the mode a turn was authorized under
                    // cannot change underneath it. The session overlay is the
                    // authority; `app.mode` only mirrors it for the footer.
                    Some(Submission::CycleMode) => {
                        let mode = next_mode(app.mode);
                        session.permissions_mut().set_mode(mode);
                        app.mode = mode;
                        app.push_notice(format!("permission mode: {}", mode_label(mode)));
                        tracing::info!(
                            target: "kuncode::authorization",
                            permission_mode = ?mode,
                            "permission mode switched",
                        );
                    }
                    Some(Submission::ResumeSession(id)) => {
                        match resumer.resume(id.clone()).await {
                            Ok(resumed) => {
                                *session = resumed;
                                // The resumed session is built with the mode
                                // this process *started* in, so re-apply the
                                // live one: a mid-session Shift+Tab choice
                                // survives `/resume` the way `/model` does.
                                session.permissions_mut().set_mode(app.mode);
                                // The transcript now belongs to the resumed
                                // session: rebuild it from that history, like
                                // a fresh `--resume` start. Process-scoped
                                // usage keeps accumulating for the exit report.
                                app.conversation.clear();
                                app.plan.clear();
                                app.follow_tail();
                                seed_transcript(app, session);
                                app.push_notice(format!("resumed session {}", id.as_str()));
                            }
                            Err(error) => app.push_error(error.to_string()),
                        }
                    }
                    None => {}
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

/// Drives a `/compact` request, rendering the same live event stream a turn
/// does — the compaction pipeline reports through the observer, so the
/// transcript already narrates what happened.
///
/// Unlike a turn it takes no approvals and produces no assistant message; only
/// the outcomes the events *don't* cover land as a notice. Ctrl-C cancels.
async fn run_compaction<M: CompletionModel + 'static>(
    terminal: &mut ratatui::DefaultTerminal,
    runner: &AgentRunner<M>,
    session: &mut AgentSession,
    app: &mut App,
    events: &mut EventStream,
    event_rx: &mut UnboundedReceiver<AgentEvent>,
) -> io::Result<()> {
    let cancel = CancellationToken::new();
    let mut outcome = None;
    let mut events_closed = false;
    let mut frame = tokio::time::interval(FRAME_INTERVAL);
    frame.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    app.status = Status::Compacting;
    {
        let mut task = Box::pin(runner.compact_now(session, &cancel));
        io_stage(
            "compaction_initial_draw",
            terminal.draw(|frame| ui::draw(frame, app)),
        )?;
        while outcome.is_none() {
            tokio::select! {
                result = &mut task => outcome = Some(result),
                _ = frame.tick() => {
                    app.advance_animation();
                    io_stage(
                        "compaction_draw",
                        terminal.draw(|frame| ui::draw(frame, app)),
                    )?;
                }
                Some(event) = event_rx.recv() => app.apply_event(event.kind),
                maybe = events.next(), if !events_closed => {
                    match maybe {
                        Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                            handle_running_key(app, key, &cancel);
                        }
                        Some(Ok(Event::Mouse(mouse))) => handle_scroll(app, mouse),
                        Some(Err(error)) => {
                            return Err(log_tui_io("compaction_input", &error, true));
                        }
                        None => events_closed = true,
                        _ => {}
                    }
                }
            }
        }
    }

    // Same reason as a turn: the final poll can enqueue events that `select!`
    // never consumed, and the idle loop does not drain them.
    while let Ok(event) = event_rx.try_recv() {
        app.apply_event(event.kind);
    }
    app.status = Status::Idle;

    match outcome.expect("loop exits only once outcome is set") {
        // The CompactionCompleted event already rendered the result.
        Ok(ManualCompaction::Compacted) => {}
        Ok(ManualCompaction::NotNeeded) => {
            app.push_notice("nothing to compact".to_string());
        }
        Ok(ManualCompaction::Unavailable { reason }) => app.push_notice(reason.to_string()),
        Err(AgentError::Cancelled) => app.push_error("compaction cancelled".to_string()),
        Err(error) => app.push_error(error.to_string()),
    }
    Ok(())
}

/// Drives one turn to completion, rendering the live event stream and servicing
/// approval modals and Ctrl-C cancel meanwhile.
///
/// The turn future borrows `session` mutably, so it is scoped to an inner block;
/// only after it is dropped is `session` free again to read the final answer.
#[allow(clippy::too_many_arguments)]
async fn run_one_turn<M: CompletionModel + 'static>(
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

/// What an idle-state submission asks the event loop to do.
#[derive(Debug, Eq, PartialEq)]
enum Submission {
    /// Run a model turn with this prompt.
    Prompt(String),
    /// Switch the completion model to this name, between turns.
    SwitchModel(String),
    /// List stored sessions and open the `/resume` picker.
    PickSession,
    /// Replace the live session with this stored one, between turns.
    ResumeSession(SessionId),
    /// Advance the permission mode one step, between turns.
    CycleMode,
    /// Compact the context now, between turns.
    Compact,
}

/// Handles a key in the idle state. Returns `Some` when Enter submits work for
/// the event loop; otherwise edits the buffer (or sets `should_quit`).
fn handle_idle_key(app: &mut App, key: KeyEvent) -> Option<Submission> {
    if app.model_picker.is_some() {
        return handle_picker_key(app, key);
    }
    if app.session_picker.is_some() {
        return handle_session_picker_key(app, key);
    }
    if let Some(submitted) = handle_menu_key(app, key) {
        return submitted;
    }
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
        // Shift+Tab, which terminals report as BackTab (with or without the
        // modifier bit, depending on the terminal).
        (_, KeyCode::BackTab) => Some(Submission::CycleMode),
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
                    command::Dispatch::Prompt => Some(Submission::Prompt(submitted)),
                    command::Dispatch::SwitchModel(name) => Some(Submission::SwitchModel(name)),
                    command::Dispatch::PickSession => Some(Submission::PickSession),
                    command::Dispatch::Compact => Some(Submission::Compact),
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

/// Handles a key while the model picker dialog is open. The dialog is modal:
/// Up/Down move the highlight, Enter picks the highlighted model (re-picking
/// the active model just closes — no pointless switch), Esc cancels, Ctrl+C
/// still quits, and every other key is swallowed instead of reaching the
/// composer.
fn handle_picker_key(app: &mut App, key: KeyEvent) -> Option<Submission> {
    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return None;
    }
    if !key.modifiers.is_empty() {
        return None;
    }
    let picker = app.model_picker.as_mut()?; // caller guards is_some
    match key.code {
        KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
        KeyCode::Down => {
            picker.selected = (picker.selected + 1).min(picker.options.len().saturating_sub(1));
        }
        KeyCode::Enter => {
            let picker = app.model_picker.take()?;
            let chosen = picker
                .options
                .into_iter()
                .nth(picker.selected)
                .expect("selected stays in bounds for the picker's lifetime");
            if chosen != app.model_name {
                return Some(Submission::SwitchModel(chosen));
            }
        }
        KeyCode::Esc => app.model_picker = None,
        _ => {}
    }
    None
}

/// Handles a key while the `/resume` session picker is open. Modal exactly
/// like [`handle_picker_key`]: Up/Down move the highlight, Enter resumes the
/// highlighted session (re-picking the current one just closes — a reload
/// would drop session-scoped grants for nothing), Esc cancels, Ctrl+C still
/// quits, and every other key is swallowed instead of reaching the composer.
fn handle_session_picker_key(app: &mut App, key: KeyEvent) -> Option<Submission> {
    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return None;
    }
    if !key.modifiers.is_empty() {
        return None;
    }
    let picker = app.session_picker.as_mut()?; // caller guards is_some
    match key.code {
        KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
        KeyCode::Down => {
            picker.selected = (picker.selected + 1).min(picker.sessions.len().saturating_sub(1));
        }
        KeyCode::Enter => {
            let picker = app.session_picker.take()?;
            if picker.current == Some(picker.selected) {
                return None;
            }
            let chosen = picker
                .sessions
                .into_iter()
                .nth(picker.selected)
                .expect("selected stays in bounds for the picker's lifetime");
            return Some(Submission::ResumeSession(chosen.id));
        }
        KeyCode::Esc => app.session_picker = None,
        _ => {}
    }
    None
}

/// Intercepts a key while the slash-command completion menu is open (the
/// composer holds a command name in progress with at least one match). The
/// menu owns Up/Down (selection), Tab (complete into the composer), and Enter
/// (run the highlighted command — what the user sees selected, not the raw
/// prefix); every other key falls through to ordinary editing, which
/// re-derives the menu from the buffer on the next frame.
///
/// `None` = not consumed; `Some(inner)` = consumed, with `inner` as
/// [`handle_idle_key`]'s return value — `None` for navigation/completion,
/// `Some(Submission)` when running the highlighted command yields work for the
/// event loop (menu Enter always dispatches a bare command name: bare `/model`
/// opens its picker dialog directly, while bare `/resume` yields
/// [`Submission::PickSession`] because listing sessions needs the event loop).
fn handle_menu_key(app: &mut App, key: KeyEvent) -> Option<Option<Submission>> {
    if !key.modifiers.is_empty() {
        return None;
    }
    let menu = command::completions(&app.input)?;
    let last = menu.len().checked_sub(1)?; // empty menu: nothing to navigate or run
    let selected = app.menu_selection.min(last);
    match key.code {
        KeyCode::Up => app.menu_selection = selected.saturating_sub(1),
        KeyCode::Down => app.menu_selection = (selected + 1).min(last),
        KeyCode::Tab => {
            // Trailing space: the finished name closes the menu and starts the
            // (future) argument position.
            app.set_input(format!("/{} ", menu[selected].name));
            app.menu_selection = 0;
        }
        KeyCode::Enter => {
            app.follow_tail();
            app.take_input();
            app.menu_selection = 0;
            return Some(
                match command::dispatch(app, &format!("/{}", menu[selected].name)) {
                    command::Dispatch::Handled => None,
                    // Unreachable today (a bare name is never a prompt), kept
                    // for the compiler to police as commands grow payloads.
                    command::Dispatch::Prompt => None,
                    command::Dispatch::SwitchModel(name) => Some(Submission::SwitchModel(name)),
                    command::Dispatch::PickSession => Some(Submission::PickSession),
                    command::Dispatch::Compact => Some(Submission::Compact),
                },
            );
        }
        _ => return None,
    }
    Some(None)
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
    fn shift_tab_asks_the_event_loop_to_cycle_the_mode() {
        let mut app = App::new("m", PermissionMode::Default);
        typing(&mut app, "half a prompt");
        let key = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);

        assert_eq!(handle_idle_key(&mut app, key), Some(Submission::CycleMode));
        assert_eq!(
            app.input, "half a prompt",
            "cycling the mode must not disturb the composer"
        );
    }

    #[test]
    fn shift_tab_is_recognized_without_the_modifier_bit() {
        // Terminals disagree on whether BackTab carries SHIFT.
        let mut app = App::new("m", PermissionMode::Default);
        let key = KeyEvent::new(KeyCode::BackTab, KeyModifiers::empty());

        assert_eq!(handle_idle_key(&mut app, key), Some(Submission::CycleMode));
    }

    #[test]
    fn the_mode_ring_skips_the_unattended_modes() {
        let mut mode = PermissionMode::Default;
        let mut seen = Vec::new();
        for _ in 0..4 {
            mode = next_mode(mode);
            seen.push(mode);
        }
        assert_eq!(
            seen,
            vec![
                PermissionMode::AcceptEdits,
                PermissionMode::Plan,
                PermissionMode::Default,
                PermissionMode::AcceptEdits,
            ],
        );
        // Starting outside the ring lands on the strictest entry, never on
        // another unattended mode.
        assert_eq!(
            next_mode(PermissionMode::BypassPermissions),
            PermissionMode::Default,
        );
        assert_eq!(next_mode(PermissionMode::DontAsk), PermissionMode::Default);
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
            handle_idle_key(&mut app, enter()),
            Some(Submission::Prompt("exit now".to_string()))
        );
        assert!(!app.should_quit, "a prompt containing exit must not quit");
    }

    #[test]
    fn slash_model_with_a_name_submits_a_switch() {
        let mut app = App::new("m", PermissionMode::Default);
        // The space after the name closes the completion menu, so this takes
        // the plain Enter path.
        typing(&mut app, "/model deepseek-v4-pro");
        assert_eq!(
            handle_idle_key(&mut app, enter()),
            Some(Submission::SwitchModel("deepseek-v4-pro".to_string()))
        );
        assert!(app.input.is_empty(), "the submission clears the composer");
    }

    #[test]
    fn slash_model_without_args_opens_the_picker() {
        let mut app = App::new("m", PermissionMode::Default);
        app.available_models = vec!["m".to_string(), "other".to_string()];
        // Bare `/model` matches the completion menu, so Enter runs the
        // highlighted command with no arguments.
        typing(&mut app, "/model");
        assert!(handle_idle_key(&mut app, enter()).is_none());
        assert!(
            app.model_picker.is_some(),
            "bare /model should open the model picker"
        );
        assert!(app.input.is_empty(), "the submission clears the composer");
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

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn menu_enter_runs_the_highlighted_command_not_the_raw_prefix() {
        let mut app = App::new("m", PermissionMode::Default);
        typing(&mut app, "/q"); // menu: [quit], highlighted
        assert!(handle_idle_key(&mut app, enter()).is_none());
        assert!(app.should_quit, "the highlighted /quit should run");
        assert!(
            app.conversation
                .iter()
                .any(|item| matches!(item, app::Item::Notice(text) if text == "/quit")),
            "the echo shows the completed command, not the prefix"
        );
    }

    #[test]
    fn menu_navigation_selects_and_clamps() {
        let mut app = App::new("m", PermissionMode::Default);
        typing(&mut app, "/"); // menu: [help, model, resume, quit]
        // One Down per row past the first, plus an extra that must clamp at
        // the last row instead of running past it.
        for _ in 0..4 {
            assert!(handle_idle_key(&mut app, key(KeyCode::Down)).is_none());
        }
        assert!(handle_idle_key(&mut app, enter()).is_none());
        assert!(app.should_quit, "Down should have selected /quit");
    }

    #[test]
    fn menu_tab_completes_the_name_without_running_it() {
        let mut app = App::new("m", PermissionMode::Default);
        typing(&mut app, "/q");
        assert!(handle_idle_key(&mut app, key(KeyCode::Tab)).is_none());
        assert_eq!(app.input, "/quit ");
        assert_eq!(app.cursor, app.input.len());
        assert!(!app.should_quit, "Tab completes; only Enter runs");
        assert!(
            app.conversation.is_empty(),
            "completion is not an execution"
        );
    }

    /// An app with the picker open over ["m", "other"], "m" active + selected.
    fn picker_app() -> App {
        let mut app = App::new("m", PermissionMode::Default);
        app.available_models = vec!["m".to_string(), "other".to_string()];
        app.open_model_picker();
        app
    }

    #[test]
    fn picker_enter_on_another_model_submits_a_switch() {
        let mut app = picker_app();
        assert!(handle_idle_key(&mut app, key(KeyCode::Down)).is_none());
        assert_eq!(
            handle_idle_key(&mut app, enter()),
            Some(Submission::SwitchModel("other".to_string()))
        );
        assert!(app.model_picker.is_none(), "picking closes the dialog");
    }

    #[test]
    fn picker_enter_on_the_current_model_closes_without_switching() {
        let mut app = picker_app();
        assert!(handle_idle_key(&mut app, enter()).is_none());
        assert!(app.model_picker.is_none());
    }

    #[test]
    fn picker_navigation_clamps_at_both_ends() {
        let mut app = picker_app();
        assert!(handle_idle_key(&mut app, key(KeyCode::Up)).is_none());
        assert_eq!(app.model_picker.as_ref().unwrap().selected, 0);
        for _ in 0..3 {
            assert!(handle_idle_key(&mut app, key(KeyCode::Down)).is_none());
        }
        assert_eq!(app.model_picker.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn picker_esc_cancels_without_switching() {
        let mut app = picker_app();
        assert!(handle_idle_key(&mut app, key(KeyCode::Esc)).is_none());
        assert!(app.model_picker.is_none());
        assert!(!app.should_quit);
    }

    #[test]
    fn picker_swallows_ordinary_typing() {
        let mut app = picker_app();
        assert!(handle_idle_key(&mut app, key(KeyCode::Char('x'))).is_none());
        assert!(
            app.input.is_empty(),
            "the dialog is modal; typing must not reach the composer"
        );
        assert!(app.model_picker.is_some());
    }

    #[test]
    fn picker_ctrl_c_still_quits() {
        let mut app = picker_app();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(handle_idle_key(&mut app, ctrl_c).is_none());
        assert!(app.should_quit);
    }

    fn stored_session(id: &str) -> kuncode_agent::session_store::SessionSummary {
        kuncode_agent::session_store::SessionSummary {
            id: SessionId::new(id),
            title: None,
            created_at: "2026-08-10T00:00:00.000Z".to_string(),
            updated_at: "2026-08-10T00:00:00.000Z".to_string(),
            message_count: 0,
            preview: None,
        }
    }

    /// An app with the session picker open over ["a", "b"], "a" current + selected.
    fn session_picker_app() -> App {
        let mut app = App::new("m", PermissionMode::Default);
        let active = SessionId::new("a");
        app.open_session_picker(
            vec![stored_session("a"), stored_session("b")],
            Some(&active),
        );
        app
    }

    #[test]
    fn typing_slash_resume_then_enter_asks_for_the_listing() {
        let mut app = App::new("m", PermissionMode::Default);
        // Bare `/resume` matches the completion menu, so Enter runs the
        // highlighted command; the listing itself happens in the event loop.
        typing(&mut app, "/resume");
        assert_eq!(
            handle_idle_key(&mut app, enter()),
            Some(Submission::PickSession)
        );
        assert!(app.input.is_empty(), "the submission clears the composer");
    }

    #[test]
    fn session_picker_enter_on_another_session_submits_a_resume() {
        let mut app = session_picker_app();
        assert!(handle_idle_key(&mut app, key(KeyCode::Down)).is_none());
        assert_eq!(
            handle_idle_key(&mut app, enter()),
            Some(Submission::ResumeSession(SessionId::new("b")))
        );
        assert!(app.session_picker.is_none(), "picking closes the dialog");
    }

    #[test]
    fn session_picker_enter_on_the_current_session_closes_without_resuming() {
        let mut app = session_picker_app();
        assert!(handle_idle_key(&mut app, enter()).is_none());
        assert!(app.session_picker.is_none());
    }

    #[test]
    fn session_picker_esc_cancels_and_typing_is_swallowed() {
        let mut app = session_picker_app();
        assert!(handle_idle_key(&mut app, key(KeyCode::Char('x'))).is_none());
        assert!(
            app.input.is_empty(),
            "the dialog is modal; typing must not reach the composer"
        );
        assert!(handle_idle_key(&mut app, key(KeyCode::Esc)).is_none());
        assert!(app.session_picker.is_none());
        assert!(!app.should_quit);
    }

    #[test]
    fn session_picker_ctrl_c_still_quits() {
        let mut app = session_picker_app();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(handle_idle_key(&mut app, ctrl_c).is_none());
        assert!(app.should_quit);
    }

    #[test]
    fn menu_does_not_capture_keys_without_matches() {
        let mut app = App::new("m", PermissionMode::Default);
        typing(&mut app, "/frobnicate");
        // No matches: Up/Down stay cursor motion, Enter submits the attempt.
        assert!(handle_idle_key(&mut app, key(KeyCode::Up)).is_none());
        assert_eq!(app.input, "/frobnicate", "the buffer must survive Up");
        assert!(handle_idle_key(&mut app, enter()).is_none());
        assert!(
            app.conversation.iter().any(
                |item| matches!(item, app::Item::Notice(text) if text.contains("unknown command"))
            ),
            "Enter still reaches unknown-command dispatch"
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
