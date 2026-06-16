//! Terminal approval prompt.

use std::io::{IsTerminal, Write};

use async_trait::async_trait;
use kuncode_agent::permission::{ApprovalOutcome, Approver, PermissionRequest, suggest_scope};

/// Asks the user to approve a tool call on the terminal.
///
/// When stdin/stdout is not a TTY there is no one to ask, so an `Ask` becomes a
/// safe `DenyOnce` (no-TTY + Ask → Deny) rather than hanging a pipeline.
pub struct TerminalApprover;

#[async_trait]
impl Approver for TerminalApprover {
    async fn request(&self, req: &PermissionRequest) -> ApprovalOutcome {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return ApprovalOutcome::DenyOnce;
        }

        let summary = req.summary.clone();
        // The suggested "Always" scope is curbed (command prefix / specific
        // file), never a blanket `tool(*)` — but for bash it is still BROADER
        // than the single call (`cargo build` → `bash(cargo*)`, which also
        // waves through `cargo publish`). Surface the exact rule in the prompt
        // so the user sees what an always/deny-always choice will persist.
        let scope = suggest_scope(req);
        let scope_rule = scope.raw.clone();

        // Reading a line blocks; keep it off the async runtime.
        //
        // Known limitation: a blocking stdin read can't be cancelled portably,
        // so if the runner's gate races this against a Ctrl-C and the token
        // wins, this task is orphaned and keeps holding stdin until the user
        // hits Enter — the next REPL read can then lose a line. Tolerated for
        // now (only triggers on Ctrl-C *during* a prompt); it dissolves once
        // the CLI grows a single-owner input loop / line editor.
        let answer = tokio::task::spawn_blocking(move || prompt(&summary, &scope_rule))
            .await
            .unwrap_or_else(|_| "n".to_string());

        match answer.as_str() {
            "y" | "yes" => ApprovalOutcome::AllowOnce,
            "a" | "always" => ApprovalOutcome::AllowAlways(scope),
            "d" => ApprovalOutcome::DenyAlways(scope),
            "c" | "cancel" => ApprovalOutcome::Abort,
            // Anything else (incl. plain "no" and EOF) is a one-off deny.
            _ => ApprovalOutcome::DenyOnce,
        }
    }
}

fn prompt(summary: &str, scope_rule: &str) -> String {
    let mut out = std::io::stdout();
    let _ = write!(out, "{}", prompt_text(summary, scope_rule));
    let _ = out.flush();

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        return "n".to_string(); // EOF → safe deny.
    }
    line.trim().to_lowercase()
}

/// Renders the approval prompt. Pure and split out so the rule scope that an
/// "always" / "deny always" choice will persist is unit-testable without a TTY.
/// The scope line matters because it can be broader than the call being
/// approved (a bash prefix), so the user must see it before remembering it.
fn prompt_text(summary: &str, scope_rule: &str) -> String {
    format!(
        "\n\u{26a0}  Permission required: {summary}\n  \
         'always' / 'deny always' will remember the rule: {scope_rule}\n  \
         [y] allow once  [a] always  [n] no  [d] deny always  [c] cancel > "
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuncode_agent::permission::PermissionAction;

    #[tokio::test]
    async fn no_tty_becomes_deny_once() {
        // The test harness has no terminal, so an Ask cannot be answered and is
        // resolved to a safe one-off deny rather than hanging.
        let req = PermissionRequest::new(
            "bash",
            PermissionAction::Execute,
            Some("rm notes.txt".to_string()),
            "Run shell command: rm notes.txt",
        );
        let outcome = TerminalApprover.request(&req).await;
        assert!(matches!(outcome, ApprovalOutcome::DenyOnce));
    }

    #[test]
    fn prompt_shows_the_rule_an_always_choice_persists() {
        let req = PermissionRequest::new(
            "bash",
            PermissionAction::Execute,
            Some("cargo build".to_string()),
            "Run shell command: cargo build",
        );
        // `cargo build` is granted as the broader `bash(cargo*)`; the prompt
        // must surface that scope so the user knows a later `cargo publish`
        // would be waved through.
        let scope = suggest_scope(&req);
        assert_eq!(scope.raw, "bash(cargo*)");
        let text = prompt_text(&req.summary, &scope.raw);
        assert!(
            text.contains("bash(cargo*)"),
            "prompt hides the scope: {text}"
        );
        assert!(text.contains("Run shell command: cargo build"));
    }
}
