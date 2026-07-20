//! Authorization pipeline for prepared tool calls.
//!
//! Tools prepare and validate a call once, profiles produce typed checks, and
//! policy, Hooks, modes, and challenge-bound approval resolve those checks before
//! the retained payload can execute.

pub mod approval;
pub mod check;
pub mod contribution;
pub mod engine;
pub mod policy;
pub mod profile;
pub mod receipt;
pub mod request;
pub mod rule;
pub mod state;
pub mod target;

#[cfg(test)]
pub(crate) use approval::tests_support::ScriptedApprovalResolver;
pub use approval::{
    ApprovalBroker, ApprovalBrokerError, ApprovalChallenge, ApprovalChallengeId, ApprovalError,
    ApprovalResolution, ApprovalResolver, PendingApprovalCheck, PolicyMutationTemplate,
    PolicyMutationTemplateId, PolicyScope, PolicyScopeSet, RejectUnavailable, SafeRequestSnapshot,
};
pub use check::{PermissionCheck, PermissionCheckId, PermissionCheckSpec, ProfileDefault};
pub use contribution::{
    AuthorizationEffect, AuthorizationResolution, CheckResolution, PermissionCauseId,
    PolicyContribution, PolicyEffect, PolicyOrigin, ResolutionBasis, SafeExplanation,
    resolve_authorization, resolve_check,
};
pub use engine::{
    AuthorizationEngine, AuthorizationError, AuthorizationOutcome, ExecutedToolCall,
    ExecutionOutcome, PendingToolCall as PendingAuthorizationCall, RejectedToolCall,
};
pub use policy::{PolicySet, PolicySetError, PolicySetRevision, WorkspaceTrust};
pub use profile::{ToolPermissionProfile, ToolProfileError, ToolProfileRevision};
pub use receipt::{
    ApprovalReceipt, AuthorizationContextRevision, AuthorizedToolCall, CheckResolutionReceipt,
    ExecutionReceipt,
};
pub use request::{
    AuthorizationRequest, AuthorizationRequestError, CanonicalToolInput, InputFingerprint,
    RequestFingerprint, ToolDisplay, ToolIdentity,
};
pub use rule::{PermissionRule, PermissionRuleError, RuleCompileContext, compile_permission_rule};
pub use state::{PermissionMode, SessionOverlayRevision, SessionPolicyOverlay};
pub use target::{
    CanonicalCommand, CanonicalOrigin, CanonicalPath, CommandKind, McpSelector, PathSelector,
    PermissionNamespace, PermissionTarget, PermissionTargetError,
};
