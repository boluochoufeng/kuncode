//! Authorization context versions and single-use execution receipts.

use kuncode_core::non_empty_vec::NonEmptyVec;

use crate::{hook::HookRegistryRevision, registry::ToolRegistryRevision, tool::PreparedInvocation};

use super::{
    AuthorizationRequest, CheckResolution, InputFingerprint, PermissionCauseId, PermissionCheckId,
    PolicyEffect, PolicySetRevision, RequestFingerprint, SessionOverlayRevision,
};

/// Version vector covering every input that can change an authorization result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizationContextRevision {
    policy_set: PolicySetRevision,
    session_overlay: SessionOverlayRevision,
    hook_registry: HookRegistryRevision,
    tool_registry: ToolRegistryRevision,
}

impl AuthorizationContextRevision {
    /// Captures one coherent authorization context snapshot.
    pub const fn new(
        policy_set: PolicySetRevision,
        session_overlay: SessionOverlayRevision,
        hook_registry: HookRegistryRevision,
        tool_registry: ToolRegistryRevision,
    ) -> Self {
        Self {
            policy_set,
            session_overlay,
            hook_registry,
            tool_registry,
        }
    }

    /// Returns the immutable policy revision.
    pub const fn policy_set(&self) -> PolicySetRevision {
        self.policy_set
    }

    /// Returns the mutable session-overlay revision.
    pub const fn session_overlay(&self) -> SessionOverlayRevision {
        self.session_overlay
    }

    /// Returns the Hook registration revision.
    pub const fn hook_registry(&self) -> HookRegistryRevision {
        self.hook_registry
    }

    /// Returns the tool registry revision.
    pub const fn tool_registry(&self) -> ToolRegistryRevision {
        self.tool_registry
    }
}

/// Stable policy evidence retained for one resolved check.
#[derive(Clone, Debug)]
pub struct CheckResolutionReceipt {
    check_id: PermissionCheckId,
    effect: PolicyEffect,
    deciding_causes: Vec<PermissionCauseId>,
}

impl CheckResolutionReceipt {
    pub(crate) fn from_resolution(resolution: &CheckResolution) -> Self {
        Self {
            check_id: resolution.check().id().clone(),
            effect: resolution.effect(),
            deciding_causes: resolution.deciding_causes().to_vec(),
        }
    }

    /// Returns the check covered by this evidence.
    pub fn check_id(&self) -> &PermissionCheckId {
        &self.check_id
    }

    /// Returns the final effect at receipt issuance.
    pub const fn effect(&self) -> PolicyEffect {
        self.effect
    }

    /// Returns the decisive stable causes.
    pub fn deciding_causes(&self) -> &[PermissionCauseId] {
        &self.deciding_causes
    }
}

/// Evidence that a challenge participated in this authorization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalReceipt {
    challenge_id: String,
}

impl ApprovalReceipt {
    pub(crate) fn new(challenge_id: impl Into<String>) -> Self {
        Self {
            challenge_id: challenge_id.into(),
        }
    }

    /// Returns the consumed challenge identity.
    pub fn challenge_id(&self) -> &str {
        &self.challenge_id
    }
}

/// Non-serializable evidence authorizing one exact prepared invocation.
#[derive(Clone, Debug)]
pub struct ExecutionReceipt {
    call_id: String,
    generation: u8,
    input_fingerprint: InputFingerprint,
    request_fingerprint: RequestFingerprint,
    context_revision: AuthorizationContextRevision,
    check_resolutions: NonEmptyVec<CheckResolutionReceipt>,
    approval: Option<ApprovalReceipt>,
    rewrite_count: u8,
}

impl ExecutionReceipt {
    pub(crate) fn new(
        request: &AuthorizationRequest,
        context_revision: AuthorizationContextRevision,
        resolutions: &NonEmptyVec<CheckResolution>,
        approval: Option<ApprovalReceipt>,
        rewrite_count: u8,
    ) -> Self {
        let first = CheckResolutionReceipt::from_resolution(resolutions.first());
        let rest = resolutions
            .iter()
            .skip(1)
            .map(CheckResolutionReceipt::from_resolution)
            .collect();
        Self {
            call_id: request.call_id().to_string(),
            generation: request.generation(),
            input_fingerprint: request.input_fingerprint().clone(),
            request_fingerprint: request.request_fingerprint().clone(),
            context_revision,
            check_resolutions: NonEmptyVec::from_first_rest(first, rest),
            approval,
            rewrite_count,
        }
    }

    /// Returns the provider call identity.
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// Returns the final rewrite generation.
    pub const fn generation(&self) -> u8 {
        self.generation
    }

    /// Returns the final canonical-input identity.
    pub fn input_fingerprint(&self) -> &InputFingerprint {
        &self.input_fingerprint
    }

    /// Returns the exact request identity authorized for execution.
    pub fn request_fingerprint(&self) -> &RequestFingerprint {
        &self.request_fingerprint
    }

    /// Returns the context snapshot that must still be current at execution.
    pub fn context_revision(&self) -> &AuthorizationContextRevision {
        &self.context_revision
    }

    /// Returns per-check decision evidence.
    pub fn check_resolutions(&self) -> &NonEmptyVec<CheckResolutionReceipt> {
        &self.check_resolutions
    }

    /// Returns optional approval evidence.
    pub const fn approval(&self) -> Option<&ApprovalReceipt> {
        self.approval.as_ref()
    }

    /// Returns the number of effective input rewrites.
    pub const fn rewrite_count(&self) -> u8 {
        self.rewrite_count
    }

    pub(crate) fn matches_request(&self, request: &AuthorizationRequest) -> bool {
        self.call_id == request.call_id()
            && self.generation == request.generation()
            && &self.input_fingerprint == request.input_fingerprint()
            && &self.request_fingerprint == request.request_fingerprint()
            && self.check_resolutions.len() == request.checks().len()
            && self
                .check_resolutions
                .iter()
                .zip(request.checks().iter())
                .all(|(receipt, check)| {
                    receipt.check_id() == check.id()
                        && receipt.effect() != PolicyEffect::Deny
                        && (receipt.effect() != PolicyEffect::RequireApproval
                            || self.approval.is_some())
                })
    }
}

/// Opaque, non-cloneable capability consumed by the execution dispatcher.
pub struct AuthorizedToolCall {
    request: AuthorizationRequest,
    invocation: Box<dyn PreparedInvocation>,
    receipt: ExecutionReceipt,
}

impl AuthorizedToolCall {
    pub(crate) fn new(
        request: AuthorizationRequest,
        invocation: Box<dyn PreparedInvocation>,
        receipt: ExecutionReceipt,
    ) -> Self {
        Self {
            request,
            invocation,
            receipt,
        }
    }

    /// Returns the final request for events and diagnostics.
    pub fn request(&self) -> &AuthorizationRequest {
        &self.request
    }

    /// Returns the one-time receipt without exposing the executable payload.
    pub fn receipt(&self) -> &ExecutionReceipt {
        &self.receipt
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        AuthorizationRequest,
        Box<dyn PreparedInvocation>,
        ExecutionReceipt,
    ) {
        (self.request, self.invocation, self.receipt)
    }
}
