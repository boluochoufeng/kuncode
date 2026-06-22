//! TUI application state and the agent-event reducer.

use crossterm::event::{KeyCode, KeyEvent};
use kuncode_agent::observer::EventKind;
use kuncode_agent::permission::{ApprovalOutcome, PermissionMode};
use kuncode_agent::todo::TodoItem;

use super::bridge::ApprovalRequest;
use crate::view::{ToolOutcome, ViewEffect, view};

/// Whether a turn is in flight. Gates input submission (one turn at a time) and
/// whether the input box / cursor is shown.
#[derive(PartialEq, Eq)]
pub enum Status {
    Idle,
    Running,
}

/// Lifecycle of one tool call as the event stream reports it.
pub enum ToolState {
    Running,
    Ok { truncated: bool },
    Failed(String),
    Denied(String),
}

/// One rendered entry in the conversation log.
///
/// Built from the agent's event stream plus the turn's final answer. Tool output
/// bodies are intentionally absent — events are thin notifications, and the
/// bodies live in the transcript; the log shows a one-line summary + final state
/// per call. Inline expansion of full bodies is deferred.
pub enum Item {
    User(String),
    Assistant(String),
    Tool {
        id: String,
        name: String,
        summary: String,
        state: ToolState,
    },
    Error(String),
}

/// Mutable state driving the terminal UI.
pub struct App {
    pub model_name: String,
    pub mode: PermissionMode,
    pub conversation: Vec<Item>,
    /// Current session task plan, rendered as a sticky panel above the input —
    /// *not* a log entry. A linear log can't keep one item pinned to the bottom
    /// (later tool calls append below it), so the live checklist lives in its own
    /// fixed pane instead. Empty means no plan (panel hidden). Updated wholesale
    /// from each [`TodoUpdate`](EventKind::TodoUpdate).
    pub plan: Vec<TodoItem>,
    pub input: String,
    pub status: Status,
    /// Set while a `PreToolUse` approval is pending; renders as a panel in the
    /// input box's place that captures keys until the user answers.
    pub approval: Option<ApprovalRequest>,
    /// Vertical scroll offset of the conversation, in rows.
    pub scroll: u16,
    /// When true, the view sticks to the bottom (latest output). Manual
    /// scroll-up clears it; scrolling back to the bottom restores it.
    pub follow: bool,
    pub should_quit: bool,
}

impl App {
    pub fn new(model_name: impl Into<String>, mode: PermissionMode) -> Self {
        Self {
            model_name: model_name.into(),
            mode,
            conversation: Vec::new(),
            plan: Vec::new(),
            input: String::new(),
            status: Status::Idle,
            approval: None,
            scroll: 0,
            follow: true,
            should_quit: false,
        }
    }

    // --- Input editing (idle) -------------------------------------------------

    pub fn insert_char(&mut self, c: char) {
        self.input.push(c);
    }

    pub fn insert_newline(&mut self) {
        self.input.push('\n');
    }

    pub fn backspace(&mut self) {
        self.input.pop();
    }

    /// Takes the current input, leaving the box empty.
    pub fn take_input(&mut self) -> String {
        std::mem::take(&mut self.input)
    }

    // --- Scrolling ------------------------------------------------------------

    /// Scrolls up by `lines`, dropping auto-follow so new output won't yank the
    /// view back to the bottom.
    pub fn scroll_up(&mut self, lines: u16) {
        self.follow = false;
        self.scroll = self.scroll.saturating_sub(lines);
    }

    /// Scrolls down by `lines`. Re-enabling follow at the bottom is left to the
    /// renderer, which alone knows the max offset for the current terminal size.
    pub fn scroll_down(&mut self, lines: u16) {
        self.scroll = self.scroll.saturating_add(lines);
    }

    /// Snaps back to following the latest output (e.g. on submit).
    pub fn follow_tail(&mut self) {
        self.follow = true;
    }

    // --- Conversation log -----------------------------------------------------

    pub fn push_user(&mut self, text: String) {
        self.conversation.push(Item::User(text));
    }

    /// Pushes the turn's final answer. Empty answers (e.g. a cancelled turn)
    /// leave no entry.
    pub fn push_assistant(&mut self, text: String) {
        if !text.trim().is_empty() {
            self.conversation.push(Item::Assistant(text));
        }
    }

    pub fn push_error(&mut self, text: String) {
        self.conversation.push(Item::Error(text));
    }

    /// Folds one agent event into the conversation log. The event's *meaning* is
    /// decided once in [`view`], shared with `CliObserver`;
    /// this only maps that meaning onto the TUI's display model. Events with no
    /// visible effect (`ModelStart`, the turn-terminal `Error` rendered via
    /// [`push_error`](Self::push_error)) yield `None`.
    pub fn apply_event(&mut self, kind: EventKind) {
        let Some(effect) = view(kind) else {
            return;
        };
        match effect {
            ViewEffect::Narration(text) => self.conversation.push(Item::Assistant(text)),
            ViewEffect::ToolOpened { id, tool, summary } => {
                self.conversation.push(Item::Tool {
                    id,
                    name: tool,
                    summary,
                    state: ToolState::Running,
                });
            }
            ViewEffect::ToolClosed { id, tool, outcome } => {
                let state = match outcome {
                    ToolOutcome::Ok { truncated } => ToolState::Ok { truncated },
                    ToolOutcome::Denied(message) => ToolState::Denied(message),
                    ToolOutcome::Failed(message) => ToolState::Failed(message),
                };
                if let Some(existing) = self.tool_state_mut(&id) {
                    *existing = state;
                } else {
                    // A `ToolClosed` with no preceding `ToolOpened`: the runner
                    // rejects an unknown tool / bad arguments before a row is
                    // opened. Add its own entry so the failure isn't dropped.
                    self.conversation.push(Item::Tool {
                        id,
                        name: tool,
                        summary: String::new(),
                        state,
                    });
                }
            }
            ViewEffect::Plan(todos) => {
                // The plan is a sticky panel, not a log entry, so a wholesale
                // replace keeps it pinned below the latest activity regardless of
                // what else streams in. An empty plan hides the panel.
                self.plan = todos;
            }
        }
    }

    /// Most recent tool entry with `id`, searched newest-first so a re-used id
    /// would resolve to the live call.
    fn tool_state_mut(&mut self, id: &str) -> Option<&mut ToolState> {
        self.conversation
            .iter_mut()
            .rev()
            .find_map(|item| match item {
                Item::Tool { id: tid, state, .. } if tid == id => Some(state),
                _ => None,
            })
    }

    // --- Approval modal -------------------------------------------------------

    pub fn set_approval(&mut self, req: ApprovalRequest) {
        self.approval = Some(req);
    }

    /// Resolves a pending approval from a key press, sending the outcome back to
    /// the waiting `TuiApprover`. No-op for keys that aren't a choice.
    pub fn handle_approval_key(&mut self, key: KeyEvent) {
        let choice = match key.code {
            KeyCode::Char('y') => Choice::AllowOnce,
            KeyCode::Char('a') => Choice::AllowAlways,
            KeyCode::Char('n') => Choice::DenyOnce,
            KeyCode::Char('d') => Choice::DenyAlways,
            KeyCode::Char('c') | KeyCode::Esc => Choice::Abort,
            _ => return,
        };
        let Some(req) = self.approval.take() else {
            return;
        };
        let outcome = match choice {
            Choice::AllowOnce => ApprovalOutcome::AllowOnce,
            Choice::AllowAlways => ApprovalOutcome::AllowAlways(req.scope),
            Choice::DenyOnce => ApprovalOutcome::DenyOnce,
            Choice::DenyAlways => ApprovalOutcome::DenyAlways(req.scope),
            Choice::Abort => ApprovalOutcome::Abort,
        };
        let _ = req.respond.send(outcome);
    }
}

/// Approval choice decoded from a key, before the (consuming) outcome is built.
enum Choice {
    AllowOnce,
    AllowAlways,
    DenyOnce,
    DenyAlways,
    Abort,
}

/// Short label for the status line. Mirrors [`PermissionMode::parse`]'s short
/// spellings so what the user sees matches what `--mode` accepts.
pub fn mode_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "accept-edits",
        PermissionMode::BypassPermissions => "bypass",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuncode_agent::observer::ToolFailure;
    use kuncode_agent::tool::ToolErrorKind;

    fn app() -> App {
        App::new("model", PermissionMode::Default)
    }

    fn tool_start(id: &str) -> EventKind {
        EventKind::ToolStart {
            tool_call_id: id.to_string(),
            tool: "bash".to_string(),
            summary: "run ls".to_string(),
        }
    }

    #[test]
    fn tool_start_then_ok_updates_the_same_entry() {
        let mut app = app();
        app.apply_event(tool_start("1"));
        app.apply_event(EventKind::ToolEnd {
            tool_call_id: "1".to_string(),
            tool: "bash".to_string(),
            ok: true,
            truncated: true,
            error: None,
        });
        // One entry (not two), flipped to Ok with the truncation flag carried.
        match app.conversation.as_slice() {
            [
                Item::Tool {
                    state: ToolState::Ok { truncated: true },
                    ..
                },
            ] => {}
            other => panic!("unexpected log: {} items", other.len()),
        }
    }

    #[test]
    fn denial_reads_apart_from_a_failure() {
        let mut app = app();
        app.apply_event(tool_start("1"));
        app.apply_event(EventKind::ToolEnd {
            tool_call_id: "1".to_string(),
            tool: "bash".to_string(),
            ok: false,
            truncated: false,
            error: Some(ToolFailure {
                kind: ToolErrorKind::PermissionDenied,
                message: "blocked".to_string(),
            }),
        });
        assert!(matches!(
            app.conversation.as_slice(),
            [Item::Tool {
                state: ToolState::Denied(_),
                ..
            }]
        ));
    }

    #[test]
    fn tool_end_without_a_start_still_surfaces() {
        // The runner reports an unknown tool / bad arguments as a `ToolEnd` with no
        // preceding `ToolStart`; it must still show up, not vanish.
        let mut app = app();
        app.apply_event(EventKind::ToolEnd {
            tool_call_id: "1".to_string(),
            tool: "mystery".to_string(),
            ok: false,
            truncated: false,
            error: Some(ToolFailure {
                kind: ToolErrorKind::UnknownTool,
                message: "no such tool".to_string(),
            }),
        });
        match app.conversation.as_slice() {
            [
                Item::Tool {
                    name,
                    state: ToolState::Failed(_),
                    ..
                },
            ] => assert_eq!(name, "mystery"),
            _ => panic!("orphan ToolEnd should surface as a tool entry"),
        }
    }

    fn todo(content: &str, status: kuncode_agent::todo::TodoStatus) -> TodoItem {
        TodoItem {
            content: content.to_string(),
            active_form: format!("{content}…"),
            status,
        }
    }

    #[test]
    fn todo_update_replaces_the_live_plan_without_touching_the_log() {
        use kuncode_agent::todo::TodoStatus;
        let mut app = app();
        app.apply_event(EventKind::TodoUpdate {
            todos: vec![todo("a", TodoStatus::InProgress)],
        });
        // Intervening log activity must not move or duplicate the plan: it is a
        // sticky panel, not a conversation entry.
        app.push_user("keep going".to_string());
        app.apply_event(EventKind::TodoUpdate {
            todos: vec![
                todo("a", TodoStatus::Completed),
                todo("b", TodoStatus::InProgress),
            ],
        });
        // The plan field holds the latest snapshot wholesale.
        assert_eq!(app.plan.len(), 2);
        assert_eq!(app.plan[0].status, TodoStatus::Completed);
        assert_eq!(app.plan[1].content, "b");
        // The log only has the user message — no plan entry leaked into it.
        assert!(matches!(app.conversation.as_slice(), [Item::User(_)]));
    }

    #[test]
    fn clearing_the_plan_empties_the_panel() {
        use kuncode_agent::todo::TodoStatus;
        let mut app = app();
        app.apply_event(EventKind::TodoUpdate {
            todos: vec![todo("a", TodoStatus::InProgress)],
        });
        // An empty plan clears it: the panel is hidden, not left as a stale list.
        app.apply_event(EventKind::TodoUpdate { todos: vec![] });
        assert!(app.plan.is_empty());
    }

    #[test]
    fn call_free_assistant_event_is_not_logged() {
        // The final answer arrives via `push_assistant`; the reducer must ignore
        // the call-free `Assistant` event so it is not doubled.
        let mut app = app();
        app.apply_event(EventKind::Assistant {
            text: "done".to_string(),
            tool_calls: vec![],
        });
        assert!(app.conversation.is_empty());
    }

    #[test]
    fn narration_alongside_calls_is_logged() {
        let mut app = app();
        app.apply_event(EventKind::Assistant {
            text: "let me check".to_string(),
            tool_calls: vec!["1".to_string()],
        });
        match app.conversation.as_slice() {
            [Item::Assistant(text)] => assert_eq!(text, "let me check"),
            _ => panic!("narration not logged"),
        }
    }

    #[test]
    fn input_edits_then_take_clears() {
        let mut app = app();
        app.insert_char('h');
        app.insert_char('i');
        app.insert_newline();
        app.insert_char('x');
        app.backspace();
        assert_eq!(app.input, "hi\n");
        assert_eq!(app.take_input(), "hi\n");
        assert!(app.input.is_empty());
    }
}
