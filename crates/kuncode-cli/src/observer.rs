//! Terminal renderer for agent progress.
//!
//! Implements [`AgentObserver`] so `kuncode-agent` stays free of terminal IO:
//! the agent emits, this renders. Live progress only — the turn's *final* answer
//! and the cancel/error footer are printed by `main.rs`, the sole owner of the
//! turn's terminal line. Writes are light, so
//! running synchronously on the loop task is fine.

use kuncode_agent::observer::{AgentEvent, AgentObserver, EventKind};

/// Renders intermediate narration, tool starts, and tool results to stdout.
pub struct CliObserver;

impl AgentObserver for CliObserver {
    fn on_event(&self, event: &AgentEvent) {
        match &event.kind {
            // No persistent line: a future spinner would start here and be
            // cleared by the next `Assistant`/`Error`.
            EventKind::ModelStart => {}
            // Print narration only when it accompanies tool calls — the final,
            // call-free answer is `main.rs`'s job, so this won't double it.
            EventKind::Assistant { text, tool_calls }
                if !text.is_empty() && !tool_calls.is_empty() =>
            {
                println!("{text}");
            }
            EventKind::ToolStart { summary, .. } => println!("⏺ {summary}"),
            EventKind::ToolEnd {
                ok: true,
                truncated,
                ..
            } => println!("  ⎿ ✓{}", if *truncated { " (truncated)" } else { "" }),
            // A denial is an expected outcome, not a crash; flag it apart from
            // genuine failures so the user reads it as "blocked", not "broke".
            EventKind::ToolEnd {
                error: Some(failure),
                ..
            } if failure.kind == "permission_denied" => {
                println!("  ⎿ ⛔ {}", failure.message);
            }
            EventKind::ToolEnd { error, .. } => println!(
                "  ⎿ ✗ {}",
                error
                    .as_ref()
                    .map(|f| format!("{}: {}", f.kind, f.message))
                    .unwrap_or_else(|| "failed".into())
            ),
            // Turn-terminal backstop: only clear any open progress state. The
            // cancel/error footer is `main.rs`'s (it also drives the exit code),
            // so printing here would duplicate its `^C cancelled` / `error: ..`.
            EventKind::Error { .. } => {}
            _ => {}
        }
    }
}
