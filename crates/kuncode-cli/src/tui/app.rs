//! TUI application state and the agent-event reducer.

mod input;

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent};
use kuncode_agent::observer::EventKind;
use kuncode_agent::permission::{ApprovalResolution, PermissionMode};
use kuncode_agent::todo::TodoItem;

use super::bridge::ApprovalRequest;
use crate::view::{ToolOutcome, ViewEffect, view};

/// Whether a turn is in flight. Gates input submission (one turn at a time) and
/// whether the input box / cursor is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Idle,
    Running,
    Compacting,
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
    /// A presentation-only marker; it never enters the agent transcript.
    Compaction,
    /// A non-fatal harness notice (e.g. session persistence degraded);
    /// rendered apart from [`Error`](Self::Error) — the turn kept going.
    Warning(String),
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
    ///
    /// Only the [`answer_revealed`](Self::answer_revealed) prefix is drawn: the
    /// model streams faster than is comfortable to read, so a typewriter advances
    /// the reveal at a fixed rate while the full text accumulates here.
    pub stream_answer: String,
    /// Byte offset (char boundary) up to which [`stream_answer`](Self::stream_answer)
    /// is currently shown. Advanced by [`advance_reveal`](Self::advance_reveal).
    pub answer_revealed: usize,
    /// Live reasoning text accumulated from
    /// [`ReasoningDelta`](EventKind::ReasoningDelta), rendered in a dimmed channel
    /// separate from [`stream_answer`](Self::stream_answer). Cleared with it.
    pub stream_reasoning: String,
    /// Reveal offset for [`stream_reasoning`](Self::stream_reasoning), the dual of
    /// [`answer_revealed`](Self::answer_revealed).
    pub reasoning_revealed: usize,
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
            answer_revealed: 0,
            stream_reasoning: String::new(),
            reasoning_revealed: 0,
            approval: None,
            scroll: 0,
            follow: true,
            should_quit: false,
        }
    }

    pub fn set_approval(&mut self, req: ApprovalRequest) {
        self.approval = Some(req);
    }

    /// Resolves a pending approval and leaves unrelated keys untouched.
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
            Choice::AllowOnce => ApprovalResolution::Approve { persistence: None },
            Choice::AllowAlways => ApprovalResolution::Approve {
                persistence: req.allow_session,
            },
            Choice::DenyOnce => ApprovalResolution::Deny { persistence: None },
            Choice::DenyAlways => ApprovalResolution::Deny {
                persistence: req.deny_session,
            },
            Choice::Abort => ApprovalResolution::Cancel,
        };
        let _ = req.respond.send(outcome);
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
        if self.status == Status::Compacting {
            self.status = Status::Running;
        }
        self.conversation.push(Item::Error(text));
    }

    fn clear_stream_preview(&mut self) {
        self.stream_answer.clear();
        self.stream_reasoning.clear();
        self.answer_revealed = 0;
        self.reasoning_revealed = 0;
    }

    /// Whether either preview channel still has un-revealed text — i.e. the
    /// typewriter has more to type. Drives both whether a tick needs to redraw
    /// and whether the turn should keep ticking before committing the answer.
    pub fn has_pending_reveal(&self) -> bool {
        self.answer_revealed < self.stream_answer.len()
            || self.reasoning_revealed < self.stream_reasoning.len()
    }

    /// Advances the typewriter by up to `cps` chars/second over `elapsed`,
    /// revealing reasoning first (it streams before the answer) then the answer.
    /// Returns whether anything was revealed, so the caller can skip an
    /// unnecessary redraw.
    ///
    /// When the model streams slower than `cps` the reveal simply catches up to
    /// what has arrived and idles, so a slow stream is shown as-is rather than
    /// being held back; only a faster-than-`cps` burst is paced.
    pub fn advance_reveal(&mut self, elapsed: Duration, cps: u32) -> bool {
        // At least one char per tick so progress never stalls on rounding.
        let mut budget = ((cps as f64) * elapsed.as_secs_f64()).round().max(1.0) as usize;
        let mut revealed = false;
        for (text, cursor) in [
            (&self.stream_reasoning, &mut self.reasoning_revealed),
            (&self.stream_answer, &mut self.answer_revealed),
        ] {
            if budget == 0 || *cursor >= text.len() {
                continue;
            }
            let stepped = advance_by_chars(text, *cursor, budget);
            let consumed = text[*cursor..stepped].chars().count();
            if stepped != *cursor {
                *cursor = stepped;
                revealed = true;
            }
            budget = budget.saturating_sub(consumed);
        }
        revealed
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
                if self.status == Status::Compacting {
                    self.status = Status::Running;
                }
                self.clear_stream_preview();
                return;
            }
            EventKind::CompactionStarted { .. } => {
                self.status = Status::Compacting;
                return;
            }
            EventKind::CompactionCompleted { .. } => {
                self.status = Status::Running;
                self.conversation.push(Item::Compaction);
                return;
            }
            EventKind::CompactionFailed { .. } => {
                self.status = Status::Running;
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
            ViewEffect::Warning(text) => self.conversation.push(Item::Warning(text)),
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
}

enum Choice {
    AllowOnce,
    AllowAlways,
    DenyOnce,
    DenyAlways,
    Abort,
}

/// Byte offset reached by stepping `chars` chars forward from `from` in `text`,
/// or `text.len()` if fewer remain. Always lands on a char boundary so slicing
/// `text[..offset]` never splits a multi-byte character.
fn advance_by_chars(text: &str, from: usize, chars: usize) -> usize {
    match text[from..].char_indices().nth(chars) {
        Some((rel, _)) => from + rel,
        None => text.len(),
    }
}

/// Short label for the status line. Mirrors [`PermissionMode::parse`]'s short
/// spellings so what the user sees matches what `--mode` accepts.
pub fn mode_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "accept-edits",
        PermissionMode::Plan => "plan",
        PermissionMode::BypassPermissions => "bypass",
        PermissionMode::DontAsk => "dont-ask",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuncode_agent::compaction::budget::TokenCountPrecision;
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
                kind: ToolErrorKind::ToolNotFound,
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
    fn reveal_paces_at_the_rate_and_caps_at_received() {
        let mut app = app();
        app.apply_event(EventKind::TextDelta {
            text: "abcdef".to_string(),
        });
        // 120 cps over 33ms ≈ 4 chars per tick.
        assert!(app.advance_reveal(Duration::from_millis(33), 120));
        assert_eq!(&app.stream_answer[..app.answer_revealed], "abcd");
        assert!(app.advance_reveal(Duration::from_millis(33), 120));
        assert_eq!(&app.stream_answer[..app.answer_revealed], "abcdef");
        // Caught up: nothing left to reveal, so no redraw is requested.
        assert!(!app.advance_reveal(Duration::from_millis(33), 120));
        assert!(!app.has_pending_reveal());
    }

    #[test]
    fn reveal_spends_budget_on_reasoning_before_the_answer() {
        let mut app = app();
        app.apply_event(EventKind::ReasoningDelta {
            text: "rr".to_string(),
        });
        app.apply_event(EventKind::TextDelta {
            text: "aaaa".to_string(),
        });
        // Budget of 3: 2 chars finish reasoning, the remaining 1 starts the answer.
        app.advance_reveal(Duration::from_secs(1), 3);
        assert_eq!(&app.stream_reasoning[..app.reasoning_revealed], "rr");
        assert_eq!(&app.stream_answer[..app.answer_revealed], "a");
    }

    #[test]
    fn reveal_never_splits_a_multibyte_char() {
        let mut app = app();
        app.apply_event(EventKind::TextDelta {
            text: "héllo".to_string(), // 'é' is two bytes
        });
        // One char per tick, walking across the multi-byte boundary; the slice
        // must always stay valid UTF-8 (would panic otherwise).
        for _ in 0..6 {
            app.advance_reveal(Duration::from_millis(1), 1);
            let _shown = &app.stream_answer[..app.answer_revealed];
        }
        assert_eq!(&app.stream_answer[..app.answer_revealed], "héllo");
    }

    #[test]
    fn compaction_events_drive_tui_only_state_without_numeric_log_data() {
        // Given
        let mut app = app();
        app.status = Status::Running;

        // When
        app.apply_event(compaction_started());

        // Then
        assert_eq!(app.status, Status::Compacting);
        assert!(app.conversation.is_empty());

        // When
        app.apply_event(compaction_completed());

        // Then
        assert_eq!(app.status, Status::Running);
        assert!(matches!(app.conversation.as_slice(), [Item::Compaction]));
    }

    #[test]
    fn compaction_failure_and_turn_error_clear_the_transient_state() {
        // Given
        let mut app = app();
        app.status = Status::Running;
        app.apply_event(compaction_started());

        // When
        app.apply_event(EventKind::CompactionFailed {
            stage: "validation".to_string(),
            error: "no_safe_boundary".to_string(),
            recoverable: true,
            before_tokens: 42_000,
            summary_usage: None,
            latency_ms: 10,
        });

        // Then
        assert_eq!(app.status, Status::Running);
        assert!(app.conversation.is_empty());

        // When
        app.apply_event(compaction_started());
        app.push_error("已取消".to_string());

        // Then
        assert_eq!(app.status, Status::Running);
    }

    fn compaction_started() -> EventKind {
        EventKind::CompactionStarted {
            reason: "soft_threshold".to_string(),
            before_tokens: 42_000,
            precision: TokenCountPrecision::Exact,
        }
    }

    fn compaction_completed() -> EventKind {
        EventKind::CompactionCompleted {
            before_tokens: 42_000,
            after_tokens: 18_000,
            target_reached: true,
            passes: vec!["semantic_summary".to_string(), "atomic_commit".to_string()],
            source_seq_start: 1,
            source_seq_end: 10,
            checkpoint_seq: 11,
            artifact_count: 0,
            summary_usage: None,
            summary_latency_ms: Some(50),
            latency_ms: 80,
        }
    }
}
