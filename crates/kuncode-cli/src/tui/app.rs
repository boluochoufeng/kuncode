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
    /// Byte offset of the edit cursor within [`input`](Self::input), always on a
    /// char boundary (`0..=input.len()`). Edits and movement keep it on a
    /// boundary so `insert`/`remove`/slicing never split a multi-byte char.
    pub cursor: usize,
    pub status: Status,
    /// Live answer text accumulated from [`TextDelta`](EventKind::TextDelta)
    /// while the model streams, rendered below the log as an in-progress bubble.
    /// Ephemeral: cleared once the iteration's text is committed (narration via
    /// the `Assistant` event, the final answer via [`push_assistant`](Self::push_assistant)),
    /// so the streamed preview is never double-shown alongside the committed item.
    pub stream_answer: String,
    /// Live reasoning text accumulated from
    /// [`ReasoningDelta`](EventKind::ReasoningDelta), rendered in a dimmed channel
    /// separate from [`stream_answer`](Self::stream_answer). Cleared with it.
    pub stream_reasoning: String,
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
            cursor: 0,
            status: Status::Idle,
            stream_answer: String::new(),
            stream_reasoning: String::new(),
            approval: None,
            scroll: 0,
            follow: true,
            should_quit: false,
        }
    }

    // --- Input editing (idle) -------------------------------------------------
    //
    // All edits happen at [`cursor`](Self::cursor); movement keeps it on a char
    // boundary. Up/Down move by *logical* line (not wrapped row) so this state
    // needs no knowledge of the render width; a step recomputes the column, so
    // passing through a short line clamps the column rather than remembering a
    // goal column.

    pub fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Deletes the char before the cursor (Backspace). No-op at the start.
    pub fn backspace(&mut self) {
        if let Some(prev) = self.prev_boundary() {
            self.input.remove(prev);
            self.cursor = prev;
        }
    }

    /// Deletes the char at the cursor (Delete). No-op at the end.
    pub fn delete(&mut self) {
        if self.cursor < self.input.len() {
            self.input.remove(self.cursor);
        }
    }

    pub fn move_left(&mut self) {
        if let Some(prev) = self.prev_boundary() {
            self.cursor = prev;
        }
    }

    pub fn move_right(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.cursor = next;
        }
    }

    /// Moves to the start of the current logical line (after the preceding `\n`).
    pub fn move_home(&mut self) {
        self.cursor = self.current_line().0;
    }

    /// Moves to the end of the current logical line (before the next `\n`).
    pub fn move_end(&mut self) {
        self.cursor = self.current_line().1;
    }

    /// Moves to the previous logical line, keeping the column. No-op on the first
    /// line; a shorter target line clamps the cursor to its end.
    pub fn move_up(&mut self) {
        let (start, _) = self.current_line();
        if start == 0 {
            return;
        }
        let col = self.input[start..self.cursor].chars().count();
        let prev_end = start - 1; // the '\n' joining the two lines
        let prev_start = self.input[..prev_end].rfind('\n').map_or(0, |i| i + 1);
        self.cursor = self.byte_at_column(prev_start, prev_end, col);
    }

    /// Moves to the next logical line, keeping the column. No-op on the last line.
    pub fn move_down(&mut self) {
        let (start, end) = self.current_line();
        if end == self.input.len() {
            return;
        }
        let col = self.input[start..self.cursor].chars().count();
        let next_start = end + 1; // skip the '\n'
        let next_end = self.input[next_start..]
            .find('\n')
            .map_or(self.input.len(), |rel| next_start + rel);
        self.cursor = self.byte_at_column(next_start, next_end, col);
    }

    /// Takes the current input, leaving the box empty and the cursor at the start.
    pub fn take_input(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.input)
    }

    /// Byte offset of the char boundary before the cursor, or `None` at the start.
    fn prev_boundary(&self) -> Option<usize> {
        self.input[..self.cursor]
            .chars()
            .next_back()
            .map(|c| self.cursor - c.len_utf8())
    }

    /// Byte offset of the char boundary after the cursor, or `None` at the end.
    fn next_boundary(&self) -> Option<usize> {
        self.input[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
    }

    /// Byte range `[start, end)` of the logical line holding the cursor: from
    /// just after the preceding `\n` to just before the next one (or input end).
    fn current_line(&self) -> (usize, usize) {
        let start = self.input[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        let end = self.input[self.cursor..]
            .find('\n')
            .map_or(self.input.len(), |rel| self.cursor + rel);
        (start, end)
    }

    /// Byte offset of the `col`-th char within line `[start, end)`, clamped to
    /// `end` when the line has fewer than `col` chars.
    fn byte_at_column(&self, start: usize, end: usize, col: usize) -> usize {
        self.input[start..end]
            .char_indices()
            .nth(col)
            .map_or(end, |(off, _)| start + off)
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

    /// Pushes the turn's final answer, committing (and clearing) any streamed
    /// preview. Empty answers (e.g. a cancelled turn) leave no entry.
    pub fn push_assistant(&mut self, text: String) {
        self.clear_stream_preview();
        if !text.trim().is_empty() {
            self.conversation.push(Item::Assistant(text));
        }
    }

    pub fn push_error(&mut self, text: String) {
        // A turn that errored/cancelled mid-stream drops its live preview.
        self.clear_stream_preview();
        self.conversation.push(Item::Error(text));
    }

    fn clear_stream_preview(&mut self) {
        self.stream_answer.clear();
        self.stream_reasoning.clear();
    }

    /// Folds one agent event into the conversation log. The event's *meaning* is
    /// decided once in [`view`], shared with `CliObserver`;
    /// this only maps that meaning onto the TUI's display model. Events with no
    /// visible effect (`ModelStart`, the turn-terminal `Error` rendered via
    /// [`push_error`](Self::push_error)) yield `None`.
    pub fn apply_event(&mut self, kind: EventKind) {
        // Streaming deltas are TUI-only live rendering, intercepted before the
        // shared `view` reducer (which treats them as no-ops). They accumulate
        // into the ephemeral preview buffers; commit paths clear them.
        match &kind {
            EventKind::ModelStart => {
                // A new model call: drop any leftover preview from the last one.
                self.clear_stream_preview();
                return;
            }
            EventKind::TextDelta { text } => {
                self.stream_answer.push_str(text);
                return;
            }
            EventKind::ReasoningDelta { text } => {
                self.stream_reasoning.push_str(text);
                return;
            }
            // Narration (text alongside tool calls) commits as an `Assistant`
            // item via `view` below, so retire the preview now. The call-free
            // final answer is committed instead by the turn driver via
            // `push_assistant`, which clears the preview then — leave it until, to
            // avoid a blank frame between here and that commit.
            EventKind::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                self.clear_stream_preview();
            }
            _ => {}
        }

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
    fn streamed_deltas_preview_then_finalize_without_duplication() {
        let mut app = app();
        app.apply_event(EventKind::ModelStart);
        app.apply_event(EventKind::TextDelta {
            text: "Hel".to_string(),
        });
        app.apply_event(EventKind::TextDelta {
            text: "lo".to_string(),
        });
        // Live preview accumulates; nothing committed to the log yet.
        assert_eq!(app.stream_answer, "Hello");
        assert!(app.conversation.is_empty());

        // The call-free `Assistant` event is ignored (preview kept to avoid a
        // blank frame); the turn driver commits the final answer.
        app.apply_event(EventKind::Assistant {
            text: "Hello".to_string(),
            tool_calls: vec![],
        });
        assert_eq!(app.stream_answer, "Hello", "preview survives until commit");

        app.push_assistant("Hello".to_string());
        assert!(app.stream_answer.is_empty(), "commit clears the preview");
        match app.conversation.as_slice() {
            [Item::Assistant(text)] => assert_eq!(text, "Hello"),
            _ => panic!("final answer should be the single committed item"),
        }
    }

    #[test]
    fn reasoning_streams_into_its_own_buffer() {
        let mut app = app();
        app.apply_event(EventKind::ReasoningDelta {
            text: "think ".to_string(),
        });
        app.apply_event(EventKind::ReasoningDelta {
            text: "hard".to_string(),
        });
        assert_eq!(app.stream_reasoning, "think hard");
        assert!(
            app.stream_answer.is_empty(),
            "reasoning is a separate channel"
        );
    }

    #[test]
    fn narration_event_clears_the_streamed_preview() {
        let mut app = app();
        app.apply_event(EventKind::TextDelta {
            text: "let me check".to_string(),
        });
        app.apply_event(EventKind::Assistant {
            text: "let me check".to_string(),
            tool_calls: vec!["1".to_string()],
        });
        // Narration commits as one item; the preview is gone (not double-shown).
        assert!(app.stream_answer.is_empty());
        match app.conversation.as_slice() {
            [Item::Assistant(text)] => assert_eq!(text, "let me check"),
            _ => panic!("narration not committed exactly once"),
        }
    }

    #[test]
    fn model_start_clears_a_stale_preview() {
        let mut app = app();
        app.stream_answer = "leftover".to_string();
        app.stream_reasoning = "stale".to_string();
        app.apply_event(EventKind::ModelStart);
        assert!(app.stream_answer.is_empty() && app.stream_reasoning.is_empty());
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
        assert_eq!(app.cursor, 0, "take_input resets the cursor");
    }

    fn typed(text: &str) -> App {
        let mut app = app();
        for c in text.chars() {
            app.insert_char(c);
        }
        app
    }

    #[test]
    fn insert_and_delete_happen_at_the_cursor() {
        let mut app = typed("helo"); // cursor at end
        app.move_left();
        app.move_left();
        app.insert_char('l'); // "hello", between the two spots
        assert_eq!(app.input, "hello");

        app.move_home();
        app.delete(); // forward-delete 'h'
        assert_eq!(app.input, "ello");
        assert_eq!(app.cursor, 0);

        app.move_end();
        app.backspace(); // delete-before 'o'
        assert_eq!(app.input, "ell");
    }

    #[test]
    fn movement_stops_at_bounds_and_respects_utf8() {
        let mut app = app();
        app.move_left(); // at start: no-op, no panic
        app.delete(); // at end of empty: no-op
        let mut app = typed("你好"); // 3 bytes each
        app.move_left();
        assert_eq!(app.cursor, 3, "left lands on a char boundary, not mid-byte");
        app.move_right();
        app.move_right(); // already at end: no-op
        assert_eq!(app.cursor, 6);
        app.backspace();
        assert_eq!(app.input, "你");
    }

    #[test]
    fn home_end_act_on_the_current_logical_line() {
        let mut app = typed("ab\ncd"); // cursor at end, on line "cd"
        app.move_home();
        assert_eq!(app.cursor, 3, "home → start of the current line");
        app.insert_char('Z');
        assert_eq!(app.input, "ab\nZcd");
        app.move_end();
        assert_eq!(app.cursor, app.input.len(), "end → end of the current line");
    }

    #[test]
    fn up_down_move_by_logical_line_clamping_the_column() {
        let mut app = typed("abcd\nxy\nwxyz"); // cursor at end of "wxyz" (col 4)
        app.move_up(); // "xy" is shorter → clamp to its end (col 2)
        assert_eq!(&app.input[..app.cursor], "abcd\nxy");
        app.move_up(); // column is now 2 → col 2 of "abcd"
        assert_eq!(&app.input[..app.cursor], "ab");
        app.move_down(); // back down to "xy", col 2 → its end
        assert_eq!(&app.input[..app.cursor], "abcd\nxy");
        app.move_up();
        app.move_up(); // already on the first line: no-op
        assert_eq!(&app.input[..app.cursor], "ab");
    }
}
