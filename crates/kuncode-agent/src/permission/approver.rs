//! The side-effecting approval layer.
//!
//! [`evaluate`](super::policy::evaluate) is pure; when it returns `Ask`, the
//! runner hands the request to an [`Approver`] for a human (or scripted)
//! decision. Keeping all interaction behind this trait lets the engine and its
//! tests stay free of any terminal IO.

#[cfg(test)]
use std::collections::VecDeque;
#[cfg(test)]
use std::sync::Mutex;

use async_trait::async_trait;

use super::request::{ApprovalOutcome, PermissionRequest};

/// Resolves an `Ask` verdict into a concrete decision. The terminal
/// implementation lives in the CLI; the engine only depends on this trait.
#[async_trait]
pub trait Approver: Send + Sync {
    /// Asks how to handle `req`. Implementations must not block the runtime;
    /// terminal prompts should run on a blocking task.
    async fn request(&self, req: &PermissionRequest) -> ApprovalOutcome;
}

/// Approves every prompt once. The default for tests and early non-interactive
/// runs where the gate should be a no-op.
#[derive(Clone, Copy, Debug, Default)]
pub struct AutoApprove;

#[async_trait]
impl Approver for AutoApprove {
    async fn request(&self, _req: &PermissionRequest) -> ApprovalOutcome {
        ApprovalOutcome::AllowOnce
    }
}

/// Denies every prompt once. The safety default when there is no TTY to ask
/// (no-TTY + Ask → Deny), so pipelines never hang on an unanswerable prompt.
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyAll;

#[async_trait]
impl Approver for DenyAll {
    async fn request(&self, _req: &PermissionRequest) -> ApprovalOutcome {
        ApprovalOutcome::DenyOnce
    }
}

/// Returns pre-scripted outcomes in order. A test scaffold for the approval
/// branches: it `expect`s on purpose (panicking if asked more times than it was
/// given outcomes), so it is gated to `#[cfg(test)]` to keep those panics out of
/// the shipped library — see the no-`expect`-in-library rule in AGENTS.md.
#[cfg(test)]
#[derive(Debug)]
pub struct ScriptedApprover {
    outcomes: Mutex<VecDeque<ApprovalOutcome>>,
}

#[cfg(test)]
impl ScriptedApprover {
    /// Builds an approver that yields `outcomes` in order.
    pub fn new(outcomes: impl IntoIterator<Item = ApprovalOutcome>) -> Self {
        Self {
            outcomes: Mutex::new(outcomes.into_iter().collect()),
        }
    }
}

#[cfg(test)]
#[async_trait]
impl Approver for ScriptedApprover {
    async fn request(&self, _req: &PermissionRequest) -> ApprovalOutcome {
        self.outcomes
            .lock()
            .expect("scripted approver lock")
            .pop_front()
            .expect("scripted approver ran out of outcomes")
    }
}
