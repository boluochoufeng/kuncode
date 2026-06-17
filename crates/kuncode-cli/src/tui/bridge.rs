//! Bridges the agent's `Observer`/`Approver` callbacks to the TUI event loop.
//!
//! The runner invokes these on its own task; each forwards over a channel so the
//! single-owner event loop renders them. Events go one-way (`on_event` is sync
//! and non-blocking); an approval is a request/response pair — the async
//! `request` parks on a `oneshot` until the user answers the modal.

use async_trait::async_trait;
use kuncode_agent::observer::{AgentEvent, AgentObserver};
use kuncode_agent::permission::{
    ApprovalOutcome, Approver, PermissionRequest, Rule, suggest_scope,
};
use tokio::sync::{mpsc::UnboundedSender, oneshot};

/// Forwards every agent event to the event loop. `on_event` must not block; an
/// unbounded send never does, and a dropped receiver (TUI gone) is ignored.
pub struct TuiObserver {
    tx: UnboundedSender<AgentEvent>,
}

impl TuiObserver {
    pub fn new(tx: UnboundedSender<AgentEvent>) -> Self {
        Self { tx }
    }
}

impl AgentObserver for TuiObserver {
    fn on_event(&self, event: &AgentEvent) {
        let _ = self.tx.send(event.clone());
    }
}

/// A pending approval handed to the event loop: what to show, the rule an
/// "always" choice would persist, and the channel to answer on.
pub struct ApprovalRequest {
    pub summary: String,
    /// Rule an "always allow/deny" choice persists — broader than the single
    /// call for bash (a command prefix), so the modal surfaces it.
    pub scope: Rule,
    pub respond: oneshot::Sender<ApprovalOutcome>,
}

impl ApprovalRequest {
    /// The rule text an "always" choice will remember.
    pub fn scope_rule(&self) -> &str {
        &self.scope.raw
    }
}

/// Routes `Ask` decisions to the modal. If the event loop is gone (receiver or
/// responder dropped) it falls back to a safe one-off deny rather than hanging
/// the turn.
pub struct TuiApprover {
    tx: UnboundedSender<ApprovalRequest>,
}

impl TuiApprover {
    pub fn new(tx: UnboundedSender<ApprovalRequest>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl Approver for TuiApprover {
    async fn request(&self, req: &PermissionRequest) -> ApprovalOutcome {
        let (respond, rx) = oneshot::channel();
        let message = ApprovalRequest {
            summary: req.summary.clone(),
            scope: suggest_scope(req),
            respond,
        };
        if self.tx.send(message).is_err() {
            return ApprovalOutcome::DenyOnce;
        }
        rx.await.unwrap_or(ApprovalOutcome::DenyOnce)
    }
}
