//! Presentation-neutral interpretation of the agent event stream.
//!
//! Both frontends — the line-by-line [`CliObserver`](crate::observer) and the
//! [`tui`](crate::tui) — must decide the *same* things from an [`EventKind`]:
//! which events are visible, when narration accompanies a tool call, and how a
//! `ToolEnd` envelope reads (ok / denied / failed). That interpretation lives
//! here once; each frontend only differs in *how* it draws the result (glyphs
//! for the plain renderer, styled rows for the TUI).

use kuncode_agent::observer::{EventKind, ToolFailure};
use kuncode_agent::todo::TodoItem;
use kuncode_agent::tool::ToolErrorKind;

/// How a finished tool call reads, independent of how it is drawn.
#[derive(Debug, PartialEq, Eq)]
pub enum ToolOutcome {
    /// Succeeded; `truncated` if the output envelope was capped.
    Ok { truncated: bool },
    /// Blocked by a permission rule or a hook — an expected outcome, shown apart
    /// from a genuine failure. Carries the human-facing reason.
    Denied(String),
    /// Failed for any other reason; carries a `kind: message` summary (or a bare
    /// `failed` when the envelope had no detail).
    Failed(String),
}

/// What one event means for any human-facing frontend.
#[derive(Debug, PartialEq, Eq)]
pub enum ViewEffect {
    /// Intermediate narration that accompanies tool calls. The call-free final
    /// answer is printed by the turn driver, not from the event stream.
    Narration(String),
    /// A tool call opened: a row should appear, pending its outcome.
    ToolOpened {
        id: String,
        tool: String,
        summary: String,
    },
    /// A tool call closed with `outcome`; updates the row opened for `id`.
    ToolClosed {
        id: String,
        tool: String,
        outcome: ToolOutcome,
    },
    /// The task plan changed; carries the full snapshot (empty = cleared).
    Plan(Vec<TodoItem>),
    /// A non-fatal harness degradation (e.g. session persistence stopped
    /// working). Shown once — the emitter already de-duplicates — and the turn
    /// continues, so it renders as a notice, not an error.
    Warning(String),
}

/// Interprets one event into its visible effect, or `None` when it has none.
///
/// `ModelStart` (no spinner yet) and the turn-terminal `Error` (rendered by the
/// turn driver / `main`, not the event stream) produce nothing, as does a
/// call-free or empty `Assistant`. The streaming `TextDelta`/`ReasoningDelta`
/// are also `None` here: live incremental rendering is the TUI's concern (it
/// intercepts them before this reducer), and the plain CLI does not stream.
pub fn view(kind: EventKind) -> Option<ViewEffect> {
    match kind {
        // Narration shows only when it accompanies tool calls; the call-free
        // final answer is the turn driver's to print, so this won't double it.
        EventKind::Assistant { text, tool_calls } if !text.is_empty() && !tool_calls.is_empty() => {
            Some(ViewEffect::Narration(text))
        }
        EventKind::ToolStart {
            tool_call_id,
            tool,
            summary,
        } => Some(ViewEffect::ToolOpened {
            id: tool_call_id,
            tool,
            summary,
        }),
        EventKind::ToolEnd {
            tool_call_id,
            tool,
            ok,
            truncated,
            error,
        } => Some(ViewEffect::ToolClosed {
            id: tool_call_id,
            tool,
            outcome: tool_outcome(ok, truncated, error),
        }),
        // Always surfaced, even when empty: the TUI needs the empty snapshot to
        // clear its plan panel (the plain renderer just prints nothing).
        EventKind::TodoUpdate { todos } => Some(ViewEffect::Plan(todos)),
        EventKind::Warning { message } => Some(ViewEffect::Warning(message)),
        EventKind::ModelStart
        | EventKind::TextDelta { .. }
        | EventKind::ReasoningDelta { .. }
        | EventKind::Assistant { .. }
        | EventKind::Error { .. }
        | EventKind::CompactionStarted { .. }
        | EventKind::CompactionCompleted { .. }
        | EventKind::CompactionSkipped { .. }
        | EventKind::CompactionObserved { .. }
        | EventKind::CompactionFailed { .. } => None,
    }
}

/// Classifies a `ToolEnd` envelope. A `permission_denied` failure reads apart
/// from a genuine error. The `kind == "permission_denied"` string match lives
/// here once — both frontends consume the resulting [`ToolOutcome`] instead of
/// re-comparing the string (a typed failure kind is deferred).
fn tool_outcome(ok: bool, truncated: bool, error: Option<ToolFailure>) -> ToolOutcome {
    if ok {
        return ToolOutcome::Ok { truncated };
    }
    match error {
        Some(f) if f.kind == ToolErrorKind::PermissionDenied => ToolOutcome::Denied(f.message),
        Some(f) => ToolOutcome::Failed(format!("{}: {}", f.kind, f.message)),
        None => ToolOutcome::Failed("failed".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuncode_agent::todo::{TodoItem, TodoStatus};

    #[test]
    fn narration_only_when_it_accompanies_tool_calls() {
        // Call-free answer: the turn driver prints it, the stream must not.
        assert_eq!(
            view(EventKind::Assistant {
                text: "done".to_string(),
                tool_calls: vec![],
            }),
            None
        );
        // Empty narration alongside a call: nothing to show.
        assert_eq!(
            view(EventKind::Assistant {
                text: String::new(),
                tool_calls: vec!["1".to_string()],
            }),
            None
        );
        // Real narration alongside a call: surfaced.
        assert_eq!(
            view(EventKind::Assistant {
                text: "let me check".to_string(),
                tool_calls: vec!["1".to_string()],
            }),
            Some(ViewEffect::Narration("let me check".to_string()))
        );
    }

    #[test]
    fn tool_end_classifies_denied_apart_from_failed() {
        let denied = view(EventKind::ToolEnd {
            tool_call_id: "1".to_string(),
            tool: "bash".to_string(),
            ok: false,
            truncated: false,
            error: Some(ToolFailure {
                kind: ToolErrorKind::PermissionDenied,
                message: "blocked by Bash(curl*)".to_string(),
            }),
        });
        assert_eq!(
            denied,
            Some(ViewEffect::ToolClosed {
                id: "1".to_string(),
                tool: "bash".to_string(),
                outcome: ToolOutcome::Denied("blocked by Bash(curl*)".to_string()),
            })
        );

        // Any other failure kind reads as a `kind: message` summary.
        let failed = view(EventKind::ToolEnd {
            tool_call_id: "2".to_string(),
            tool: "bash".to_string(),
            ok: false,
            truncated: false,
            error: Some(ToolFailure {
                kind: ToolErrorKind::Other("non_zero_exit".to_string()),
                message: "exit 1".to_string(),
            }),
        });
        assert!(matches!(
            failed,
            Some(ViewEffect::ToolClosed { outcome: ToolOutcome::Failed(m), .. }) if m == "non_zero_exit: exit 1"
        ));

        // No detail at all falls back to a bare "failed".
        let bare = view(EventKind::ToolEnd {
            tool_call_id: "3".to_string(),
            tool: "bash".to_string(),
            ok: false,
            truncated: false,
            error: None,
        });
        assert!(matches!(
            bare,
            Some(ViewEffect::ToolClosed { outcome: ToolOutcome::Failed(m), .. }) if m == "failed"
        ));
    }

    #[test]
    fn tool_end_ok_carries_truncation() {
        let ok = view(EventKind::ToolEnd {
            tool_call_id: "1".to_string(),
            tool: "read_file".to_string(),
            ok: true,
            truncated: true,
            error: None,
        });
        assert!(matches!(
            ok,
            Some(ViewEffect::ToolClosed {
                outcome: ToolOutcome::Ok { truncated: true },
                ..
            })
        ));
    }

    #[test]
    fn model_start_and_terminal_error_have_no_visible_effect() {
        assert_eq!(view(EventKind::ModelStart), None);
        assert_eq!(
            view(EventKind::Error {
                kind: "completion".to_string(),
                message: "boom".to_string(),
            }),
            None
        );
    }

    #[test]
    fn streaming_deltas_have_no_view_effect() {
        // Live incremental rendering is the TUI's own concern; the shared reducer
        // (and thus the plain CLI) ignores deltas.
        assert_eq!(
            view(EventKind::TextDelta {
                text: "Hel".to_string(),
            }),
            None
        );
        assert_eq!(
            view(EventKind::ReasoningDelta {
                text: "think".to_string(),
            }),
            None
        );
    }

    #[test]
    fn warning_passes_through() {
        assert_eq!(
            view(EventKind::Warning {
                message: "session persistence failed: disk full".to_string(),
            }),
            Some(ViewEffect::Warning(
                "session persistence failed: disk full".to_string()
            ))
        );
    }

    #[test]
    fn todo_update_passes_through_even_when_empty() {
        // The empty snapshot must survive so the TUI can clear its plan panel.
        assert_eq!(
            view(EventKind::TodoUpdate { todos: vec![] }),
            Some(ViewEffect::Plan(vec![]))
        );
        let item = TodoItem {
            content: "a".to_string(),
            active_form: "doing a".to_string(),
            status: TodoStatus::InProgress,
        };
        assert_eq!(
            view(EventKind::TodoUpdate {
                todos: vec![item.clone()],
            }),
            Some(ViewEffect::Plan(vec![item]))
        );
    }
}
