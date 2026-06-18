//! What a tool call is asking permission to do, and how that request is
//! resolved.

use super::rule::{Rule, RuleOrigin};

/// The kind of operation a tool performs. Only the classes the current tools
/// need: no `Fetch` (there is no network tool yet) and no `Other` (we refuse a
/// vague catch-all). Add `Fetch` when a network tool appears.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionAction {
    /// Observe state without changing it. Defaults to `Allow`.
    Read,
    /// Mutate workspace state. Defaults to `Ask`.
    Write,
    /// Run an opaque subprocess. Defaults to `Ask`.
    Execute,
    /// Operate on the agent's own session state (e.g. the task plan) with no
    /// external side effect — no filesystem, network, or subprocess. Defaults to
    /// `Allow` and never auto-asks; an explicit deny rule can still block it.
    /// Distinct from [`Read`](Self::Read), which observes *external* state.
    Meta,
}

/// One parsed tool call awaiting a permission verdict. Produced by a tool's
/// `permission()` from its already-parsed arguments.
#[derive(Clone, Debug)]
pub struct PermissionRequest {
    /// Model-facing tool name, e.g. `"edit_file"` or `"bash"`.
    pub tool: String,
    /// Operation class, driving the per-action default and `AcceptEdits`.
    pub action: PermissionAction,
    /// What rules match against: a workspace-relative path for file tools, a
    /// lightly normalized command for `bash`. `None` for argument-less tools,
    /// which can then only be matched by a bare-tool rule.
    pub resource: Option<String>,
    /// Human-readable one-liner shown in the approval prompt.
    pub summary: String,
}

impl PermissionRequest {
    /// Builds a request. `resource` is `None` for tools with nothing worth
    /// scoping a rule to.
    pub fn new(
        tool: impl Into<String>,
        action: PermissionAction,
        resource: Option<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            tool: tool.into(),
            action,
            resource,
            summary: summary.into(),
        }
    }
}

/// Result of the pure [`evaluate`](super::policy::evaluate): allow outright,
/// deny outright, or escalate to a human approver.
#[derive(Clone, Debug)]
pub enum Verdict {
    Allow,
    Deny(DenyReason),
    Ask,
}

/// Why a request was denied, carrying the matched rule so the harness can
/// explain "you are blocked because of rule X (from source Y)".
#[derive(Clone, Debug)]
pub struct DenyReason {
    /// Where the matched rule came from.
    pub origin: RuleOrigin,
    /// The matched rule's original text, e.g. `"Bash(sudo*)"`.
    pub rule: String,
}

/// What a human approver decided for an `Ask` verdict.
#[derive(Clone, Debug)]
pub enum ApprovalOutcome {
    /// Allow this one call; do not remember.
    AllowOnce,
    /// Allow and remember as a session allow-grant.
    AllowAlways(Rule),
    /// Deny this one call; do not remember.
    DenyOnce,
    /// Deny and remember as a session deny-grant.
    DenyAlways(Rule),
    /// Abort the whole turn (user interrupt). Becomes `ToolError::Cancelled`.
    Abort,
}
