//! Permission system: rules, policy, session state, and approval.
//!
//! Layering (see `docs/s03/permission-system.md`):
//!
//! - [`PermissionPolicy`] — static rules, owned read-only by the runner.
//! - [`PermissionSessionState`] — per-session grants + mode, lives in
//!   [`AgentSession`](crate::session::AgentSession) (already `&mut`).
//! - [`Approver`] — the side-effecting human prompt, owned by the runner.
//! - [`evaluate`] — the pure decision function over the three above.
//!
//! The runner sets the gate *before* dispatch via [`PermissionGate`]: `prepare`
//! a [`PermissionRequest`] from the tool, then `decide` it (`evaluate`, and only
//! on `Ask` consult the [`Approver`]).

pub mod approver;
pub mod gate;
pub mod policy;
pub mod request;
pub mod rule;
pub mod state;

#[cfg(test)]
pub use approver::ScriptedApprover;
pub use approver::{Approver, AutoApprove, DenyAll};
pub use gate::{Decision, PermissionGate, Prepared};
pub use policy::{PermissionPolicy, evaluate};
pub use request::{ApprovalOutcome, DenyReason, PermissionAction, PermissionRequest, Verdict};
pub use rule::{
    Rule, RuleOrigin, RuleParseError, first_match, matches_any, parse_rule, suggest_scope,
};
pub use state::{PermissionMode, PermissionSessionState};
