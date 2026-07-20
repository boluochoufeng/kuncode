//! Bridges the agent's observer and approval callbacks to the TUI event loop.
//!
//! The runner invokes these on its own task; each forwards over a channel so the
//! single-owner event loop renders them. Events go one-way (`on_event` is sync
//! and non-blocking); an approval is a request/response pair — the async
//! `request` parks on a `oneshot` until the user answers the modal.

use async_trait::async_trait;
use kuncode_agent::observer::{AgentEvent, AgentObserver};
use kuncode_agent::permission::{
    ApprovalChallenge, ApprovalResolution, ApprovalResolver, PolicyEffect,
    PolicyMutationTemplateId, PolicyScope,
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

/// A pending approval handed to the event loop.
///
/// Persistence choices are opaque IDs produced by the engine. The TUI can
/// select one but cannot construct or widen a policy rule.
pub struct ApprovalRequest {
    pub summary: String,
    pub targets: Vec<String>,
    pub allow_session: Option<PolicyMutationTemplateId>,
    pub deny_session: Option<PolicyMutationTemplateId>,
    pub respond: oneshot::Sender<ApprovalResolution>,
}

impl ApprovalRequest {
    /// Summarizes the exact targets whose decision can be remembered.
    pub fn persistence_label(&self) -> String {
        match self.targets.as_slice() {
            [] => "无".to_string(),
            [target] => target.clone(),
            targets => format!("{} 个精确目标", targets.len()),
        }
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
impl ApprovalResolver for TuiApprover {
    async fn resolve(&self, challenge: &ApprovalChallenge) -> ApprovalResolution {
        let (respond, rx) = oneshot::channel();
        let targets = challenge
            .pending_checks()
            .iter()
            .map(|check| check.target().to_string())
            .collect();
        let message = ApprovalRequest {
            summary: challenge.request_snapshot().display().summary().to_string(),
            targets,
            allow_session: mutation_id(challenge, PolicyEffect::Allow),
            deny_session: mutation_id(challenge, PolicyEffect::Deny),
            respond,
        };
        if self.tx.send(message).is_err() {
            return ApprovalResolution::Deny { persistence: None };
        }
        rx.await
            .unwrap_or(ApprovalResolution::Deny { persistence: None })
    }
}

fn mutation_id(
    challenge: &ApprovalChallenge,
    effect: PolicyEffect,
) -> Option<PolicyMutationTemplateId> {
    let mut matches = challenge
        .mutation_options()
        .iter()
        .filter(|option| option.effect() == effect && option.scope() == PolicyScope::Session);
    let selected = matches.next()?;
    matches.next().is_none().then(|| selected.id().clone())
}
