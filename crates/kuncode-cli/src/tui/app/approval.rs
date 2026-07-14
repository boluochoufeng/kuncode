//! Permission approval interactions for the TUI application state.

use crossterm::event::{KeyCode, KeyEvent};
use kuncode_agent::permission::ApprovalOutcome;

use super::App;
use crate::tui::bridge::ApprovalRequest;

impl App {
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
            Choice::AllowOnce => ApprovalOutcome::AllowOnce,
            Choice::AllowAlways => ApprovalOutcome::AllowAlways(req.scope),
            Choice::DenyOnce => ApprovalOutcome::DenyOnce,
            Choice::DenyAlways => ApprovalOutcome::DenyAlways(req.scope),
            Choice::Abort => ApprovalOutcome::Abort,
        };
        let _ = req.respond.send(outcome);
    }
}

enum Choice {
    AllowOnce,
    AllowAlways,
    DenyOnce,
    DenyAlways,
    Abort,
}
