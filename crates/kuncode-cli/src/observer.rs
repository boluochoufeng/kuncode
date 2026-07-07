//! Terminal renderer for agent progress.
//!
//! Implements [`AgentObserver`] so `kuncode-agent` stays free of terminal IO:
//! the agent emits, this renders. Live progress only — the turn's *final* answer
//! and the cancel/error footer are printed by `main.rs`, the sole owner of the
//! turn's terminal line. Writes are light, so
//! running synchronously on the loop task is fine.

use kuncode_agent::observer::{AgentEvent, AgentObserver};
use kuncode_agent::todo::{TodoItem, TodoStatus};

use crate::view::{ToolOutcome, ViewEffect, view};

/// Renders intermediate narration, tool starts, and tool results to stdout.
pub struct CliObserver;

impl AgentObserver for CliObserver {
    fn on_event(&self, event: &AgentEvent) {
        // What each event *means* is decided once in `view`; this renderer only
        // draws the result. Events with no visible effect (`ModelStart`, the
        // turn-terminal `Error` whose footer `main.rs` owns) yield `None`.
        let Some(effect) = view(event.kind.clone()) else {
            return;
        };
        match effect {
            ViewEffect::Narration(text) => println!("{text}"),
            ViewEffect::ToolOpened { summary, .. } => println!("⏺ {summary}"),
            ViewEffect::ToolClosed { outcome, .. } => match outcome {
                ToolOutcome::Ok { truncated } => {
                    println!("  ⎿ ✓{}", if truncated { " (truncated)" } else { "" });
                }
                // A denial reads apart from a genuine failure: "blocked", not "broke".
                ToolOutcome::Denied(message) => println!("  ⎿ ⛔ {message}"),
                ToolOutcome::Failed(message) => println!("  ⎿ ✗ {message}"),
            },
            // A notice, not an error: the turn keeps going.
            ViewEffect::Warning(text) => println!("⚠ {text}"),
            // Reprint the whole checklist (it is small): no in-place cursor model
            // here, unlike the TUI. An empty plan prints nothing, so no lone header.
            ViewEffect::Plan(todos) => {
                if !todos.is_empty() {
                    println!("⏺ 任务计划");
                    for todo in &todos {
                        let (glyph, text) = todo_glyph_and_text(todo);
                        println!("  ⎿ {glyph} {text}");
                    }
                }
            }
        }
    }
}

/// Status → (glyph, text) for one plan item, shared by this plain renderer and
/// the TUI ([`crate::tui`]). `in_progress` shows the present-tense `active_form`;
/// the others show the imperative `content`. One source of truth so the two
/// renderers can't drift on the glyph or which text field to show.
pub(crate) fn todo_glyph_and_text(todo: &TodoItem) -> (&'static str, &str) {
    match todo.status {
        TodoStatus::Pending => ("☐", todo.content.as_str()),
        TodoStatus::InProgress => ("▸", todo.active_form.as_str()),
        TodoStatus::Completed => ("✓", todo.content.as_str()),
    }
}
