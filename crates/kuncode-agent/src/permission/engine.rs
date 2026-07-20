//! Single authorization and execution path for prepared tool calls.

use std::{
    collections::BTreeSet,
    sync::atomic::{AtomicU64, Ordering},
};

use kuncode_core::completion::Message;
use serde::Serialize;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    hook::{AuthorizationHookResult, HookEffect, Hooks, PreToolCx},
    registry::ToolRegistry,
    tool::{
        ExecutedInvocation, PreparationContext, PreparedInvocation, PreparedInvocationState,
        ToolContext, ToolError, ToolErrorKind, ToolOutput,
    },
};

use super::approval::ApprovalSatisfaction;
use super::{
    ApprovalBroker, ApprovalBrokerError, ApprovalChallenge, ApprovalError, ApprovalReceipt,
    ApprovalResolution, AuthorizationContextRevision, AuthorizationEffect, AuthorizationRequest,
    AuthorizationRequestError, AuthorizationResolution, AuthorizedToolCall, ExecutionReceipt,
    PermissionCauseId, PolicyContribution, PolicyEffect, PolicyOrigin, PolicyScope, PolicySet,
    PolicySetError, SafeExplanation, SessionPolicyOverlay, ToolIdentity, ToolProfileError,
};

const MAX_REWRITES: u8 = 4;
const MAX_AUTHORIZATION_ITERATIONS: usize = 24;
static CHALLENGE_NONCE: AtomicU64 = AtomicU64::new(1);

/// Provider-issued call awaiting preparation and authorization.
#[derive(Clone, Debug)]
pub struct PendingToolCall {
    call_id: String,
    tool_name: String,
    raw_input: serde_json::Value,
}

impl PendingToolCall {
    /// Creates one pending call while retaining the provider call identity.
    pub fn new(
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        raw_input: serde_json::Value,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            raw_input,
        }
    }

    /// Returns the provider tool-call identity.
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// Returns the model-facing tool identity.
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// Returns the current raw or replacement input.
    pub fn raw_input(&self) -> &serde_json::Value {
        &self.raw_input
    }
}

/// Model-recoverable authorization rejection with optional stable request data.
pub struct RejectedToolCall {
    request: Option<AuthorizationRequest>,
    output: ToolOutput,
}

impl RejectedToolCall {
    fn before_preparation(output: ToolOutput) -> Self {
        Self {
            request: None,
            output,
        }
    }

    fn after_preparation(request: AuthorizationRequest, output: ToolOutput) -> Self {
        Self {
            request: Some(request),
            output,
        }
    }

    /// Returns the final stable request when preparation succeeded.
    pub const fn request(&self) -> Option<&AuthorizationRequest> {
        self.request.as_ref()
    }

    /// Splits event metadata from the model-visible result.
    pub fn into_parts(self) -> (Option<AuthorizationRequest>, ToolOutput) {
        (self.request, self.output)
    }
}

/// Result of the single authorization entrypoint.
pub enum AuthorizationOutcome {
    /// Contains the only capability accepted by execution.
    Authorized(AuthorizedToolCall),
    /// Fails closed while keeping the model loop recoverable.
    Rejected(RejectedToolCall),
    /// User or runner cancellation aborts the current operation.
    Cancelled,
}

/// Result of consuming an [`AuthorizedToolCall`].
pub enum ExecutionOutcome {
    /// The exact prepared invocation ran under a current receipt.
    Executed(Box<ExecutedToolCall>),
    /// Authorization context changed before dispatch; authorization must restart.
    Stale(PendingToolCall),
    /// Cancellation won before the invocation completed.
    Cancelled,
}

/// Delivered output paired with the consumed authorization receipt.
pub struct ExecutedToolCall {
    request: AuthorizationRequest,
    receipt: ExecutionReceipt,
    invocation: ExecutedInvocation,
}

impl ExecutedToolCall {
    /// Returns the request actually executed.
    pub fn request(&self) -> &AuthorizationRequest {
        &self.request
    }

    /// Returns the consumed authorization evidence.
    pub fn receipt(&self) -> &ExecutionReceipt {
        &self.receipt
    }

    /// Splits authorization metadata from the delivered tool result.
    pub fn into_parts(self) -> (AuthorizationRequest, ExecutionReceipt, ExecutedInvocation) {
        (self.request, self.receipt, self.invocation)
    }
}

/// Borrowed authorization service assembled by the runner.
pub struct AuthorizationEngine<'a> {
    registry: &'a ToolRegistry,
    policy: &'a PolicySet,
    hooks: &'a Hooks,
    approvals: &'a ApprovalBroker,
}

impl<'a> AuthorizationEngine<'a> {
    /// Binds the trusted registries and fail-closed approval broker.
    pub const fn new(
        registry: &'a ToolRegistry,
        policy: &'a PolicySet,
        hooks: &'a Hooks,
        approvals: &'a ApprovalBroker,
    ) -> Self {
        Self {
            registry,
            policy,
            hooks,
            approvals,
        }
    }

    /// Prepares, stabilizes, resolves, and optionally approves one tool call.
    ///
    /// # Errors
    /// Returns only trusted configuration or invariant failures. Model input,
    /// policy rejection, and unavailable approval remain recoverable outcomes.
    pub async fn authorize(
        &self,
        pending: PendingToolCall,
        overlay: &mut SessionPolicyOverlay,
        messages: &[Message],
        iteration: usize,
        cancel: &CancellationToken,
    ) -> Result<AuthorizationOutcome, AuthorizationError> {
        self.authorize_with_progress(pending, overlay, messages, iteration, cancel, |_| {})
            .await
    }

    pub(crate) async fn authorize_with_progress<F>(
        &self,
        pending: PendingToolCall,
        overlay: &mut SessionPolicyOverlay,
        messages: &[Message],
        iteration: usize,
        cancel: &CancellationToken,
        mut on_stable: F,
    ) -> Result<AuthorizationOutcome, AuthorizationError>
    where
        F: FnMut(&AuthorizationRequest),
    {
        let mut prepared = match self.prepare(&pending, 0).await? {
            PreparationResult::Ready(prepared) => prepared,
            PreparationResult::Rejected(output) => {
                return Ok(AuthorizationOutcome::Rejected(
                    RejectedToolCall::before_preparation(output),
                ));
            }
        };
        let mut rewrites = RewriteState {
            seen_inputs: BTreeSet::from([prepared
                .request
                .input_fingerprint()
                .as_str()
                .to_string()]),
            count: 0,
        };
        let mut approval_receipt = None;

        for _ in 0..MAX_AUTHORIZATION_ITERATIONS {
            let stabilized = match self
                .stabilize(
                    prepared,
                    &pending,
                    messages,
                    iteration,
                    cancel,
                    &mut rewrites,
                )
                .await?
            {
                StabilizationResult::Stable(stabilized) => stabilized,
                StabilizationResult::Rejected(rejected) => {
                    return Ok(AuthorizationOutcome::Rejected(rejected));
                }
                StabilizationResult::Cancelled => return Ok(AuthorizationOutcome::Cancelled),
            };

            on_stable(&stabilized.prepared.request);

            let context_revision = self.context_revision(overlay);
            let resolution = self.policy.resolve_with_overlay(
                &stabilized.prepared.request,
                overlay,
                &stabilized.hook_contributions,
            )?;
            audit_resolution(&stabilized.prepared.request, &resolution);
            match resolution.effect() {
                AuthorizationEffect::Deny => {
                    return Ok(AuthorizationOutcome::Rejected(
                        RejectedToolCall::after_preparation(
                            stabilized.prepared.request,
                            permission_denied("permission policy denied this tool call"),
                        ),
                    ));
                }
                AuthorizationEffect::Allow => {
                    return Ok(AuthorizationOutcome::Authorized(authorize_prepared(
                        stabilized.prepared,
                        resolution,
                        context_revision,
                        approval_receipt,
                        rewrites.count,
                    )));
                }
                AuthorizationEffect::RequireApproval => {}
            }

            if overlay.mode() == super::PermissionMode::DontAsk {
                return Ok(AuthorizationOutcome::Rejected(
                    RejectedToolCall::after_preparation(
                        stabilized.prepared.request,
                        permission_denied("approval is disabled for this session"),
                    ),
                ));
            }

            let challenge = ApprovalChallenge::new(
                CHALLENGE_NONCE.fetch_add(1, Ordering::Relaxed),
                &stabilized.prepared.request,
                &resolution,
                context_revision.clone(),
                None,
            )?;
            tracing::info!(
                target: "kuncode::authorization",
                call_id = stabilized.prepared.request.call_id(),
                tool = stabilized.prepared.request.tool().as_str(),
                generation = stabilized.prepared.request.generation(),
                request_fingerprint = stabilized
                    .prepared
                    .request
                    .request_fingerprint()
                    .as_str(),
                challenge_id = challenge.id().as_str(),
                pending_checks = challenge.pending_checks().len(),
                mutation_options = challenge.mutation_options().len(),
                policy_revision = challenge.context_revision().policy_set().get(),
                session_revision = challenge.context_revision().session_overlay().get(),
                hook_revision = challenge.context_revision().hook_registry().get(),
                tool_revision = challenge.context_revision().tool_registry().get(),
                "approval challenge created",
            );
            let approval = match self.approvals.claim(&challenge) {
                Err(_) => ApprovalResolution::Deny { persistence: None },
                Ok(()) => {
                    let hook_resolution = tokio::select! {
                        resolution = self.hooks.approval_request(&challenge) => resolution,
                        _ = cancel.cancelled() => return Ok(AuthorizationOutcome::Cancelled),
                    };
                    if matches!(hook_resolution, ApprovalResolution::Abstain) {
                        tokio::select! {
                            resolution = self.approvals.resolve_claimed(&challenge) => {
                                match resolution {
                                    Ok(resolution) => resolution,
                                    Err(_) => ApprovalResolution::Deny { persistence: None },
                                }
                            }
                            _ = cancel.cancelled() => return Ok(AuthorizationOutcome::Cancelled),
                        }
                    } else {
                        hook_resolution
                    }
                }
            };
            tracing::info!(
                target: "kuncode::authorization",
                challenge_id = challenge.id().as_str(),
                outcome = approval_resolution_name(&approval),
                "approval challenge resolved",
            );

            if self.context_revision(overlay) != *challenge.context_revision() {
                tracing::warn!(
                    target: "kuncode::authorization",
                    challenge_id = challenge.id().as_str(),
                    reason = "context_revision_changed",
                    "approval invalidated",
                );
                prepared = stabilized.prepared;
                continue;
            }

            match approval {
                ApprovalResolution::Abstain => {
                    return Ok(AuthorizationOutcome::Rejected(
                        RejectedToolCall::after_preparation(
                            stabilized.prepared.request,
                            permission_denied("no approval resolver accepted this request"),
                        ),
                    ));
                }
                ApprovalResolution::Cancel => return Ok(AuthorizationOutcome::Cancelled),
                ApprovalResolution::Deny { persistence } => {
                    if let Some(id) = persistence
                        && !apply_session_mutation(overlay, &challenge, &id, PolicyEffect::Deny)
                    {
                        return Ok(AuthorizationOutcome::Rejected(
                            RejectedToolCall::after_preparation(
                                stabilized.prepared.request,
                                permission_denied("invalid approval mutation selection"),
                            ),
                        ));
                    }
                    return Ok(AuthorizationOutcome::Rejected(
                        RejectedToolCall::after_preparation(
                            stabilized.prepared.request,
                            permission_denied("approval was denied"),
                        ),
                    ));
                }
                ApprovalResolution::ReplaceInput(input) => {
                    if rewrites.count >= MAX_REWRITES {
                        return Ok(AuthorizationOutcome::Rejected(
                            RejectedToolCall::after_preparation(
                                stabilized.prepared.request,
                                hook_rewrite_failed("approval input rewrite limit exceeded"),
                            ),
                        ));
                    }
                    let replacement_pending = PendingToolCall::new(
                        pending.call_id.clone(),
                        pending.tool_name.clone(),
                        input.as_value().clone(),
                    );
                    let replacement = match self
                        .prepare(
                            &replacement_pending,
                            stabilized.prepared.request.generation().saturating_add(1),
                        )
                        .await?
                    {
                        PreparationResult::Ready(replacement) => replacement,
                        PreparationResult::Rejected(_) => {
                            return Ok(AuthorizationOutcome::Rejected(
                                RejectedToolCall::after_preparation(
                                    stabilized.prepared.request,
                                    hook_rewrite_failed("approval replacement input is invalid"),
                                ),
                            ));
                        }
                    };
                    if replacement.request.input_fingerprint()
                        == stabilized.prepared.request.input_fingerprint()
                    {
                        prepared = stabilized.prepared;
                        continue;
                    }
                    if !rewrites
                        .seen_inputs
                        .insert(replacement.request.input_fingerprint().as_str().to_string())
                    {
                        return Ok(AuthorizationOutcome::Rejected(
                            RejectedToolCall::after_preparation(
                                stabilized.prepared.request,
                                hook_rewrite_failed("approval input rewrite cycle detected"),
                            ),
                        ));
                    }
                    rewrites.count = rewrites.count.saturating_add(1);
                    approval_receipt = None;
                    prepared = replacement;
                }
                ApprovalResolution::Approve { persistence } => {
                    if let Some(id) = persistence
                        && !apply_session_mutation(overlay, &challenge, &id, PolicyEffect::Allow)
                    {
                        return Ok(AuthorizationOutcome::Rejected(
                            RejectedToolCall::after_preparation(
                                stabilized.prepared.request,
                                permission_denied("invalid approval mutation selection"),
                            ),
                        ));
                    }

                    let current_revision = self.context_revision(overlay);
                    let satisfaction =
                        ApprovalSatisfaction::from_challenge(&challenge, current_revision.clone());
                    let refreshed = self.policy.resolve_with_overlay(
                        &stabilized.prepared.request,
                        overlay,
                        &stabilized.hook_contributions,
                    )?;
                    audit_resolution(&stabilized.prepared.request, &refreshed);
                    match refreshed.effect() {
                        AuthorizationEffect::Deny => {
                            return Ok(AuthorizationOutcome::Rejected(
                                RejectedToolCall::after_preparation(
                                    stabilized.prepared.request,
                                    permission_denied(
                                        "permission policy changed to deny after approval",
                                    ),
                                ),
                            ));
                        }
                        AuthorizationEffect::RequireApproval
                            if !satisfaction.covers(
                                &stabilized.prepared.request,
                                &current_revision,
                                &refreshed,
                            ) =>
                        {
                            prepared = stabilized.prepared;
                            continue;
                        }
                        AuthorizationEffect::Allow | AuthorizationEffect::RequireApproval => {
                            approval_receipt =
                                Some(ApprovalReceipt::new(satisfaction.challenge_id().as_str()));
                            return Ok(AuthorizationOutcome::Authorized(authorize_prepared(
                                stabilized.prepared,
                                refreshed,
                                current_revision,
                                approval_receipt,
                                rewrites.count,
                            )));
                        }
                    }
                }
            }
        }

        Err(AuthorizationError::IterationLimit)
    }

    /// Consumes a receipt only when its entire context snapshot is still current.
    ///
    /// # Errors
    /// Returns a harness-level tool failure after a valid invocation starts.
    pub async fn execute(
        &self,
        authorized: AuthorizedToolCall,
        overlay: &SessionPolicyOverlay,
        context: &ToolContext,
    ) -> Result<ExecutionOutcome, ToolError> {
        let current_revision = self.context_revision(overlay);
        let (request, mut invocation, receipt) = authorized.into_parts();
        if receipt.context_revision() != &current_revision || !receipt.matches_request(&request) {
            tracing::warn!(
                target: "kuncode::authorization",
                call_id = request.call_id(),
                tool = request.tool().as_str(),
                generation = request.generation(),
                request_fingerprint = request.request_fingerprint().as_str(),
                reason = "receipt_or_context_mismatch",
                "execution receipt rejected as stale",
            );
            return Ok(ExecutionOutcome::Stale(PendingToolCall::new(
                request.call_id(),
                request.tool().as_str(),
                request.canonical_input().as_value().clone(),
            )));
        }

        let validity = tokio::select! {
            result = invocation.revalidate(context) => result?,
            _ = context.cancel.cancelled() => return Ok(ExecutionOutcome::Cancelled),
        };
        if validity == PreparedInvocationState::Stale {
            tracing::warn!(
                target: "kuncode::authorization",
                call_id = request.call_id(),
                tool = request.tool().as_str(),
                generation = request.generation(),
                request_fingerprint = request.request_fingerprint().as_str(),
                reason = "prepared_resource_changed",
                "execution receipt rejected as stale",
            );
            return Ok(ExecutionOutcome::Stale(PendingToolCall::new(
                request.call_id(),
                request.tool().as_str(),
                request.canonical_input().as_value().clone(),
            )));
        }

        let executed = tokio::select! {
            result = invocation.execute(context) => result?,
            _ = context.cancel.cancelled() => return Ok(ExecutionOutcome::Cancelled),
        };
        tracing::info!(
            target: "kuncode::authorization",
            call_id = request.call_id(),
            tool = request.tool().as_str(),
            generation = request.generation(),
            request_fingerprint = request.request_fingerprint().as_str(),
            "execution receipt consumed",
        );
        Ok(ExecutionOutcome::Executed(Box::new(ExecutedToolCall {
            request,
            receipt,
            invocation: executed,
        })))
    }

    /// Captures the current version vector for diagnostics and execution checks.
    pub fn context_revision(&self, overlay: &SessionPolicyOverlay) -> AuthorizationContextRevision {
        AuthorizationContextRevision::new(
            self.policy.revision(),
            overlay.revision(),
            self.hooks.revision(),
            self.registry.revision(),
        )
    }

    async fn prepare(
        &self,
        pending: &PendingToolCall,
        generation: u8,
    ) -> Result<PreparationResult, AuthorizationError> {
        let Some(registered) = self.registry.registered(pending.tool_name()) else {
            tracing::warn!(
                target: "kuncode::authorization",
                call_id = pending.call_id(),
                tool_name_chars = pending.tool_name().chars().count(),
                outcome = "tool_not_found",
                "tool preparation rejected",
            );
            return Ok(PreparationResult::Rejected(ToolOutput::failure(
                ToolErrorKind::ToolNotFound,
                format!("tool `{}` is not registered", pending.tool_name()),
            )));
        };
        let tool = registered.tool().clone();
        let profile = registered.profile().clone();
        let preparation = match tool
            .prepare(pending.raw_input().clone(), &PreparationContext::new())
            .await
        {
            Ok(preparation) => preparation,
            Err(output) => {
                tracing::info!(
                    target: "kuncode::authorization",
                    call_id = pending.call_id(),
                    tool = pending.tool_name(),
                    generation,
                    error_kind = tool_output_error_kind(&output),
                    "tool preparation rejected",
                );
                return Ok(PreparationResult::Rejected(output));
            }
        };
        let (canonical_input, invocation, specs, display) = preparation.into_parts();
        let checks = match profile.validate(specs.into_vec()) {
            Ok(checks) => checks,
            Err(error) => {
                tracing::error!(
                    target: "kuncode::authorization",
                    call_id = pending.call_id(),
                    tool = pending.tool_name(),
                    generation,
                    error = %error,
                    "tool permission profile rejected preparation",
                );
                return Err(error.into());
            }
        };
        let request = AuthorizationRequest::new(
            pending.call_id(),
            generation,
            ToolIdentity::new(pending.tool_name())?,
            canonical_input,
            checks,
            display,
            profile.revision().clone(),
        )?;
        tracing::info!(
            target: "kuncode::authorization",
            call_id = request.call_id(),
            tool = request.tool().as_str(),
            generation = request.generation(),
            checks = request.checks().len(),
            input_fingerprint = request.input_fingerprint().as_str(),
            request_fingerprint = request.request_fingerprint().as_str(),
            profile_revision = request.profile_revision().as_str(),
            "tool preparation validated",
        );
        Ok(PreparationResult::Ready(PreparedCall {
            request,
            invocation,
        }))
    }

    async fn stabilize(
        &self,
        mut prepared: PreparedCall,
        pending: &PendingToolCall,
        messages: &[Message],
        iteration: usize,
        cancel: &CancellationToken,
        rewrites: &mut RewriteState,
    ) -> Result<StabilizationResult, AuthorizationError> {
        loop {
            let cx = PreToolCx {
                request: &prepared.request,
                messages,
                iteration,
            };
            let results = tokio::select! {
                results = self.hooks.pre_tool_use(&cx) => results,
                _ = cancel.cancelled() => return Ok(StabilizationResult::Cancelled),
            };
            let has_deny = results.iter().any(|result| {
                matches!(
                    result.outcome().effect.as_ref(),
                    Some(HookEffect::Deny { .. })
                )
            });
            let contributions = hook_contributions(&results, &prepared.request)?;
            if has_deny {
                return Ok(StabilizationResult::Stable(StabilizedCall {
                    prepared,
                    hook_contributions: contributions,
                }));
            }

            let mut replacements = Vec::new();
            for result in &results {
                let Some(input) = result.outcome().replacement_input.as_ref() else {
                    continue;
                };
                let replacement_pending =
                    PendingToolCall::new(pending.call_id(), pending.tool_name(), input.clone());
                let replacement = match self
                    .prepare(
                        &replacement_pending,
                        prepared.request.generation().saturating_add(1),
                    )
                    .await?
                {
                    PreparationResult::Ready(replacement) => replacement,
                    PreparationResult::Rejected(_) => {
                        return Ok(StabilizationResult::Rejected(
                            RejectedToolCall::after_preparation(
                                prepared.request,
                                hook_rewrite_failed("hook replacement input is invalid"),
                            ),
                        ));
                    }
                };
                replacements.push((result.name().to_string(), replacement));
            }
            if replacements.is_empty() {
                return Ok(StabilizationResult::Stable(StabilizedCall {
                    prepared,
                    hook_contributions: contributions,
                }));
            }

            let fingerprints = replacements
                .iter()
                .map(|(_, replacement)| {
                    replacement
                        .request
                        .request_fingerprint()
                        .as_str()
                        .to_string()
                })
                .collect::<BTreeSet<_>>();
            if fingerprints.len() != 1 {
                return Ok(StabilizationResult::Rejected(
                    RejectedToolCall::after_preparation(
                        prepared.request,
                        hook_rewrite_failed("hook input rewrites conflict"),
                    ),
                ));
            }
            replacements.sort_by(|left, right| left.0.cmp(&right.0));
            let (_, replacement) = replacements.remove(0);
            if replacement.request.input_fingerprint() == prepared.request.input_fingerprint() {
                return Ok(StabilizationResult::Stable(StabilizedCall {
                    prepared,
                    hook_contributions: contributions,
                }));
            }
            if rewrites.count >= MAX_REWRITES {
                return Ok(StabilizationResult::Rejected(
                    RejectedToolCall::after_preparation(
                        prepared.request,
                        hook_rewrite_failed("hook input rewrite limit exceeded"),
                    ),
                ));
            }
            if !rewrites
                .seen_inputs
                .insert(replacement.request.input_fingerprint().as_str().to_string())
            {
                return Ok(StabilizationResult::Rejected(
                    RejectedToolCall::after_preparation(
                        prepared.request,
                        hook_rewrite_failed("hook input rewrite cycle detected"),
                    ),
                ));
            }
            rewrites.count = rewrites.count.saturating_add(1);
            tracing::info!(
                target: "kuncode::authorization",
                call_id = replacement.request.call_id(),
                tool = replacement.request.tool().as_str(),
                generation = replacement.request.generation(),
                input_fingerprint = replacement.request.input_fingerprint().as_str(),
                rewrite_count = rewrites.count,
                "hook input rewrite accepted",
            );
            prepared = replacement;
        }
    }
}

struct PreparedCall {
    request: AuthorizationRequest,
    invocation: Box<dyn PreparedInvocation>,
}

struct StabilizedCall {
    prepared: PreparedCall,
    hook_contributions: Vec<PolicyContribution>,
}

struct RewriteState {
    seen_inputs: BTreeSet<String>,
    count: u8,
}

enum PreparationResult {
    Ready(PreparedCall),
    Rejected(ToolOutput),
}

enum StabilizationResult {
    Stable(StabilizedCall),
    Rejected(RejectedToolCall),
    Cancelled,
}

/// Trusted authorization configuration or invariant failure.
#[derive(Debug, Error)]
pub enum AuthorizationError {
    /// Tool-emitted checks violated the registry profile.
    #[error(transparent)]
    ToolProfile(#[from] ToolProfileError),
    /// Canonical request construction failed.
    #[error(transparent)]
    Request(#[from] AuthorizationRequestError),
    /// Static or session policy resolution failed.
    #[error(transparent)]
    Policy(#[from] PolicySetError),
    /// Challenge construction failed.
    #[error(transparent)]
    Approval(#[from] ApprovalError),
    /// Approval state could not be consumed safely.
    #[error(transparent)]
    ApprovalBroker(#[from] ApprovalBrokerError),
    /// Stable Hook cause data could not be encoded.
    #[error("failed to encode authorization Hook contribution: {0}")]
    HookEncoding(#[from] serde_json::Error),
    /// Dynamic policy or Hook state failed to converge.
    #[error("authorization did not converge within its iteration limit")]
    IterationLimit,
}

fn hook_contributions(
    results: &[AuthorizationHookResult],
    request: &AuthorizationRequest,
) -> Result<Vec<PolicyContribution>, serde_json::Error> {
    #[derive(Serialize)]
    struct HookCause<'a> {
        hook: &'a str,
        generation: u8,
        request_fingerprint: &'a str,
        effect: PolicyEffect,
    }

    results
        .iter()
        .filter_map(|result| {
            result.outcome().effect.as_ref().map(|effect| {
                let (effect, explanation) = match effect {
                    HookEffect::Allow => (
                        PolicyEffect::Allow,
                        SafeExplanation::new("authorization Hook allowed this generation"),
                    ),
                    HookEffect::Ask => (
                        PolicyEffect::RequireApproval,
                        SafeExplanation::new("authorization Hook requested approval"),
                    ),
                    HookEffect::Deny { reason } => (PolicyEffect::Deny, reason.clone()),
                };
                let cause_id = PermissionCauseId::derive(&HookCause {
                    hook: result.name(),
                    generation: request.generation(),
                    request_fingerprint: request.request_fingerprint().as_str(),
                    effect,
                })?;
                Ok(PolicyContribution::new(
                    effect,
                    PolicyOrigin::Hook(result.name().to_string()),
                    cause_id,
                    explanation,
                ))
            })
        })
        .collect()
}

fn apply_session_mutation(
    overlay: &mut SessionPolicyOverlay,
    challenge: &ApprovalChallenge,
    id: &super::PolicyMutationTemplateId,
    expected_effect: PolicyEffect,
) -> bool {
    let Some(template) = challenge.mutation(id) else {
        return false;
    };
    if template.effect() != expected_effect || template.scope() != PolicyScope::Session {
        return false;
    }
    tracing::info!(
        target: "kuncode::authorization",
        challenge_id = challenge.id().as_str(),
        mutation_id = template.id().as_str(),
        effect = ?template.effect(),
        scope = ?template.scope(),
        namespace = ?template.target().namespace(),
        rule_cause_id = template.rule().cause_id().as_str(),
        "approval policy mutation applied",
    );
    overlay.push(template.rule().clone());
    true
}

fn authorize_prepared(
    prepared: PreparedCall,
    resolution: AuthorizationResolution,
    context_revision: AuthorizationContextRevision,
    approval: Option<ApprovalReceipt>,
    rewrite_count: u8,
) -> AuthorizedToolCall {
    let receipt = ExecutionReceipt::new(
        &prepared.request,
        context_revision,
        resolution.checks(),
        approval,
        rewrite_count,
    );
    tracing::info!(
        target: "kuncode::authorization",
        call_id = prepared.request.call_id(),
        tool = prepared.request.tool().as_str(),
        generation = prepared.request.generation(),
        request_fingerprint = prepared.request.request_fingerprint().as_str(),
        checks = receipt.check_resolutions().len(),
        approved = receipt.approval().is_some(),
        rewrite_count,
        policy_revision = receipt.context_revision().policy_set().get(),
        session_revision = receipt.context_revision().session_overlay().get(),
        hook_revision = receipt.context_revision().hook_registry().get(),
        tool_revision = receipt.context_revision().tool_registry().get(),
        "execution receipt issued",
    );
    AuthorizedToolCall::new(prepared.request, prepared.invocation, receipt)
}

fn audit_resolution(request: &AuthorizationRequest, resolution: &AuthorizationResolution) {
    for check in resolution.checks().iter() {
        tracing::info!(
            target: "kuncode::authorization",
            call_id = request.call_id(),
            tool = request.tool().as_str(),
            generation = request.generation(),
            request_fingerprint = request.request_fingerprint().as_str(),
            check_id = check.check().id().as_str(),
            namespace = ?check.check().target().namespace(),
            effect = ?check.effect(),
            basis = ?check.basis(),
            deciding_causes = check.deciding_causes().len(),
            matching_contributions = check.contributions().len(),
            "permission check resolved",
        );
        for contribution in check.contributions() {
            tracing::debug!(
                target: "kuncode::authorization",
                call_id = request.call_id(),
                check_id = check.check().id().as_str(),
                cause_id = contribution.cause_id().as_str(),
                origin = ?contribution.origin(),
                effect = ?contribution.effect(),
                "matching permission contribution",
            );
        }
    }
}

fn approval_resolution_name(resolution: &ApprovalResolution) -> &'static str {
    match resolution {
        ApprovalResolution::Abstain => "abstain",
        ApprovalResolution::Approve { .. } => "approve",
        ApprovalResolution::Deny { .. } => "deny",
        ApprovalResolution::ReplaceInput(_) => "replace_input",
        ApprovalResolution::Cancel => "cancel",
    }
}

fn tool_output_error_kind(output: &ToolOutput) -> &str {
    output
        .error
        .as_ref()
        .map_or("none", |error| error.kind.as_str())
}

fn permission_denied(message: &str) -> ToolOutput {
    ToolOutput::failure(ToolErrorKind::PermissionDenied, message)
}

fn hook_rewrite_failed(message: &str) -> ToolOutput {
    ToolOutput::failure("hook_rewrite_failed", message)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use async_trait::async_trait;
    use kuncode_core::{completion::ToolDefinition, non_empty_vec::NonEmptyVec};

    use super::*;
    use crate::{
        hook::{AuthorizationHookFailure, Hook, HookCapabilities, HookEffect, PreToolOutcome},
        permission::{
            ApprovalChallenge, ApprovalResolution, ApprovalResolver, CanonicalPath,
            CanonicalToolInput, PermissionCheckSpec, PermissionMode, PermissionTarget,
            PolicyEffect, PolicyOrigin, PolicyScopeSet, PolicySet, ToolDisplay,
            approval::tests_support::ScriptedApprovalResolver,
        },
        test_support::TestDir,
        tool::{Tool, ToolPreparation},
    };

    struct CountingTool {
        definition: ToolDefinition,
        prepares: Arc<AtomicUsize>,
        executions: Arc<AtomicUsize>,
    }

    impl CountingTool {
        fn new(prepares: Arc<AtomicUsize>, executions: Arc<AtomicUsize>) -> Self {
            Self {
                definition: ToolDefinition {
                    name: "counting".to_string(),
                    description: "test".to_string(),
                    parameters: serde_json::json!({ "type": "object" }),
                },
                prepares,
                executions,
            }
        }
    }

    struct CountingInvocation {
        executions: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl PreparedInvocation for CountingInvocation {
        async fn execute(
            self: Box<Self>,
            _ctx: &ToolContext,
        ) -> Result<ExecutedInvocation, ToolError> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            Ok(ExecutedInvocation::new(
                ToolOutput::success(serde_json::json!({ "done": true })),
                crate::tool::ToolResultRetention::Verbatim,
            ))
        }
    }

    #[async_trait]
    impl Tool for CountingTool {
        fn definition(&self) -> &ToolDefinition {
            &self.definition
        }

        async fn prepare(
            self: Arc<Self>,
            args: serde_json::Value,
            _ctx: &PreparationContext,
        ) -> Result<ToolPreparation, ToolOutput> {
            self.prepares.fetch_add(1, Ordering::SeqCst);
            if !args.is_object() {
                return Err(ToolOutput::failure(
                    ToolErrorKind::InvalidArguments,
                    "counting input must be an object",
                ));
            }
            let target = PermissionTarget::exact_tool("counting")
                .map_err(|error| ToolOutput::failure("invalid_arguments", error.to_string()))?;
            Ok(ToolPreparation::new(
                CanonicalToolInput::new(args),
                Box::new(CountingInvocation {
                    executions: self.executions.clone(),
                }),
                NonEmptyVec::new(PermissionCheckSpec::new(target)),
                ToolDisplay::new("Run counting tool"),
            ))
        }
    }

    struct AllowHook;

    #[async_trait]
    impl Hook for AllowHook {
        async fn pre_tool_use(
            &self,
            _cx: &PreToolCx<'_>,
        ) -> Result<PreToolOutcome, AuthorizationHookFailure> {
            Ok(PreToolOutcome {
                effect: Some(HookEffect::Allow),
                replacement_input: None,
            })
        }
    }

    struct RewriteThenAbstain;

    #[async_trait]
    impl Hook for RewriteThenAbstain {
        async fn pre_tool_use(
            &self,
            cx: &PreToolCx<'_>,
        ) -> Result<PreToolOutcome, AuthorizationHookFailure> {
            if cx.request.canonical_input().as_value()["value"] == 1 {
                Ok(PreToolOutcome {
                    effect: Some(HookEffect::Allow),
                    replacement_input: Some(serde_json::json!({ "value": 2 })),
                })
            } else {
                Ok(PreToolOutcome::default())
            }
        }
    }

    struct RewriteValue {
        from: i64,
        to: serde_json::Value,
    }

    #[async_trait]
    impl Hook for RewriteValue {
        async fn pre_tool_use(
            &self,
            cx: &PreToolCx<'_>,
        ) -> Result<PreToolOutcome, AuthorizationHookFailure> {
            Ok(
                if cx.request.canonical_input().as_value()["value"] == self.from {
                    PreToolOutcome {
                        effect: None,
                        replacement_input: Some(self.to.clone()),
                    }
                } else {
                    PreToolOutcome::default()
                },
            )
        }
    }

    struct ToggleRewrite;

    #[async_trait]
    impl Hook for ToggleRewrite {
        async fn pre_tool_use(
            &self,
            cx: &PreToolCx<'_>,
        ) -> Result<PreToolOutcome, AuthorizationHookFailure> {
            let value = cx.request.canonical_input().as_value()["value"].as_i64();
            let replacement_input = match value {
                Some(1) => Some(serde_json::json!({ "value": 2 })),
                Some(2) => Some(serde_json::json!({ "value": 1 })),
                _ => None,
            };
            Ok(PreToolOutcome {
                effect: None,
                replacement_input,
            })
        }
    }

    struct IncrementRewrite;

    #[async_trait]
    impl Hook for IncrementRewrite {
        async fn pre_tool_use(
            &self,
            cx: &PreToolCx<'_>,
        ) -> Result<PreToolOutcome, AuthorizationHookFailure> {
            let value = cx.request.canonical_input().as_value()["value"]
                .as_i64()
                .unwrap_or_default();
            Ok(PreToolOutcome {
                effect: None,
                replacement_input: Some(serde_json::json!({ "value": value + 1 })),
            })
        }
    }

    struct DenyAndRewrite;

    #[async_trait]
    impl Hook for DenyAndRewrite {
        async fn pre_tool_use(
            &self,
            _cx: &PreToolCx<'_>,
        ) -> Result<PreToolOutcome, AuthorizationHookFailure> {
            Ok(PreToolOutcome {
                effect: Some(HookEffect::Deny {
                    reason: SafeExplanation::new("test deny"),
                }),
                replacement_input: Some(serde_json::json!({ "value": 2 })),
            })
        }
    }

    struct AskHook;

    #[async_trait]
    impl Hook for AskHook {
        async fn pre_tool_use(
            &self,
            _cx: &PreToolCx<'_>,
        ) -> Result<PreToolOutcome, AuthorizationHookFailure> {
            Ok(PreToolOutcome {
                effect: Some(HookEffect::Ask),
                replacement_input: None,
            })
        }
    }

    struct FailingHook;

    #[async_trait]
    impl Hook for FailingHook {
        async fn pre_tool_use(
            &self,
            _cx: &PreToolCx<'_>,
        ) -> Result<PreToolOutcome, AuthorizationHookFailure> {
            Err(AuthorizationHookFailure::new("test hook failed"))
        }
    }

    struct ApprovalHook {
        resolution: ApprovalHookResolution,
    }

    #[derive(Clone, Copy)]
    enum ApprovalHookResolution {
        Approve,
        ApprovePersisted,
        Replace,
    }

    #[async_trait]
    impl Hook for ApprovalHook {
        async fn approval_request(
            &self,
            challenge: &ApprovalChallenge,
        ) -> Result<ApprovalResolution, AuthorizationHookFailure> {
            Ok(match self.resolution {
                ApprovalHookResolution::Approve => {
                    ApprovalResolution::Approve { persistence: None }
                }
                ApprovalHookResolution::ApprovePersisted => ApprovalResolution::Approve {
                    persistence: challenge
                        .mutation_options()
                        .iter()
                        .find(|option| option.effect() == PolicyEffect::Allow)
                        .map(|option| option.id().clone()),
                },
                ApprovalHookResolution::Replace => ApprovalResolution::ReplaceInput(
                    CanonicalToolInput::new(serde_json::json!({ "value": 2 })),
                ),
            })
        }
    }

    struct CountingResolver {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ApprovalResolver for CountingResolver {
        async fn resolve(&self, _challenge: &ApprovalChallenge) -> ApprovalResolution {
            self.calls.fetch_add(1, Ordering::SeqCst);
            ApprovalResolution::Approve { persistence: None }
        }
    }

    struct WrongDirectionResolver;

    #[async_trait]
    impl ApprovalResolver for WrongDirectionResolver {
        async fn resolve(&self, challenge: &ApprovalChallenge) -> ApprovalResolution {
            ApprovalResolution::Approve {
                persistence: challenge
                    .mutation_options()
                    .iter()
                    .find(|option| option.effect() == PolicyEffect::Deny)
                    .map(|option| option.id().clone()),
            }
        }
    }

    fn root() -> CanonicalPath {
        CanonicalPath::from_absolute(std::path::Path::new("/workspace")).expect("absolute root")
    }

    fn registry(prepares: Arc<AtomicUsize>, executions: Arc<AtomicUsize>) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry
            .register(CountingTool::new(prepares, executions))
            .expect("test tool registers");
        registry
    }

    fn allow_policy() -> PolicySet {
        let mut policy = PolicySet::new(root());
        policy
            .compile_and_push(
                "ExactTool(counting)",
                PolicyEffect::Allow,
                PolicyOrigin::User,
            )
            .expect("allow rule compiles");
        policy
    }

    fn rejected_kind(outcome: AuthorizationOutcome) -> String {
        let AuthorizationOutcome::Rejected(rejected) = outcome else {
            panic!("expected rejected authorization");
        };
        let (_, output) = rejected.into_parts();
        output
            .error
            .expect("rejection carries an error")
            .kind
            .as_str()
            .to_string()
    }

    #[tokio::test]
    async fn approval_executes_the_prepared_payload_without_repreparing() {
        let prepares = Arc::new(AtomicUsize::new(0));
        let executions = Arc::new(AtomicUsize::new(0));
        let registry = registry(prepares.clone(), executions.clone());
        let policy = PolicySet::new(root());
        let hooks = Hooks::new();
        let mut approvals = ApprovalBroker::new();
        approvals.push(Arc::new(ScriptedApprovalResolver::new([
            ApprovalResolution::Approve { persistence: None },
        ])));
        let engine = AuthorizationEngine::new(&registry, &policy, &hooks, &approvals);
        let mut overlay = SessionPolicyOverlay::default();

        let outcome = engine
            .authorize(
                PendingToolCall::new("call-1", "counting", serde_json::json!({ "value": 1 })),
                &mut overlay,
                &[],
                0,
                &CancellationToken::new(),
            )
            .await
            .expect("authorization succeeds");
        let AuthorizationOutcome::Authorized(authorized) = outcome else {
            panic!("expected authorization");
        };
        let execution = engine
            .execute(authorized, &overlay, &ToolContext::new())
            .await
            .expect("execution succeeds");
        assert!(matches!(execution, ExecutionOutcome::Executed(_)));
        assert_eq!(prepares.load(Ordering::SeqCst), 1);
        assert_eq!(executions.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn capable_hook_allow_replaces_profile_default_ask() {
        let registry = registry(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let policy = PolicySet::new(root());
        let mut hooks = Hooks::new();
        hooks
            .push_with_capabilities(
                "allow",
                Arc::new(AllowHook),
                HookCapabilities {
                    may_allow: true,
                    ..HookCapabilities::default()
                },
            )
            .expect("hook registers");
        let approvals = ApprovalBroker::new();
        let engine = AuthorizationEngine::new(&registry, &policy, &hooks, &approvals);

        let outcome = engine
            .authorize(
                PendingToolCall::new("call-1", "counting", serde_json::json!({})),
                &mut SessionPolicyOverlay::default(),
                &[],
                0,
                &CancellationToken::new(),
            )
            .await
            .expect("authorization resolves");
        assert!(matches!(outcome, AuthorizationOutcome::Authorized(_)));
    }

    #[tokio::test]
    async fn explicit_ask_beats_hook_allow_and_missing_resolver_rejects() {
        let registry = registry(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let mut policy = PolicySet::new(root());
        policy
            .compile_and_push(
                "ExactTool(counting)",
                PolicyEffect::RequireApproval,
                PolicyOrigin::Managed,
            )
            .expect("rule compiles");
        let mut hooks = Hooks::new();
        hooks
            .push_with_capabilities(
                "allow",
                Arc::new(AllowHook),
                HookCapabilities {
                    may_allow: true,
                    ..HookCapabilities::default()
                },
            )
            .expect("hook registers");
        let approvals = ApprovalBroker::new();
        let engine = AuthorizationEngine::new(&registry, &policy, &hooks, &approvals);

        let outcome = engine
            .authorize(
                PendingToolCall::new("call-1", "counting", serde_json::json!({})),
                &mut SessionPolicyOverlay::default(),
                &[],
                0,
                &CancellationToken::new(),
            )
            .await
            .expect("authorization resolves");
        assert!(matches!(outcome, AuthorizationOutcome::Rejected(_)));
    }

    #[tokio::test]
    async fn rewrite_discards_allow_from_the_previous_generation() {
        let prepares = Arc::new(AtomicUsize::new(0));
        let registry = registry(prepares.clone(), Arc::new(AtomicUsize::new(0)));
        let policy = PolicySet::new(root());
        let mut hooks = Hooks::new();
        hooks
            .push_with_capabilities(
                "rewrite",
                Arc::new(RewriteThenAbstain),
                HookCapabilities {
                    may_allow: true,
                    may_rewrite_input: true,
                    ..HookCapabilities::default()
                },
            )
            .expect("hook registers");
        let approvals = ApprovalBroker::new();
        let engine = AuthorizationEngine::new(&registry, &policy, &hooks, &approvals);

        let outcome = engine
            .authorize(
                PendingToolCall::new("call-1", "counting", serde_json::json!({ "value": 1 })),
                &mut SessionPolicyOverlay::default(),
                &[],
                0,
                &CancellationToken::new(),
            )
            .await
            .expect("authorization resolves");
        assert!(matches!(outcome, AuthorizationOutcome::Rejected(_)));
        assert_eq!(prepares.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn hook_allow_without_capability_fails_closed() {
        let registry = registry(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let policy = PolicySet::new(root());
        let mut hooks = Hooks::new();
        hooks.push(Arc::new(AllowHook));
        let approvals = ApprovalBroker::new();
        let engine = AuthorizationEngine::new(&registry, &policy, &hooks, &approvals);

        let outcome = engine
            .authorize(
                PendingToolCall::new("call-1", "counting", serde_json::json!({})),
                &mut SessionPolicyOverlay::default(),
                &[],
                0,
                &CancellationToken::new(),
            )
            .await
            .expect("authorization resolves");
        assert!(matches!(outcome, AuthorizationOutcome::Rejected(_)));
    }

    #[tokio::test]
    async fn hook_deny_discards_replacement_without_preparing_it() {
        let prepares = Arc::new(AtomicUsize::new(0));
        let registry = registry(prepares.clone(), Arc::new(AtomicUsize::new(0)));
        let policy = allow_policy();
        let mut hooks = Hooks::new();
        hooks
            .push_with_capabilities(
                "deny-and-rewrite",
                Arc::new(DenyAndRewrite),
                HookCapabilities {
                    may_rewrite_input: true,
                    ..HookCapabilities::default()
                },
            )
            .expect("hook registers");
        let approvals = ApprovalBroker::new();
        let engine = AuthorizationEngine::new(&registry, &policy, &hooks, &approvals);

        let outcome = engine
            .authorize(
                PendingToolCall::new("call-1", "counting", serde_json::json!({ "value": 1 })),
                &mut SessionPolicyOverlay::default(),
                &[],
                0,
                &CancellationToken::new(),
            )
            .await
            .expect("authorization resolves");

        assert_eq!(rejected_kind(outcome), "permission_denied");
        assert_eq!(prepares.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn equivalent_hook_replacements_merge_independently_of_registration_order() {
        let prepares = Arc::new(AtomicUsize::new(0));
        let registry = registry(prepares.clone(), Arc::new(AtomicUsize::new(0)));
        let policy = allow_policy();
        let mut hooks = Hooks::new();
        for name in ["second", "first"] {
            hooks
                .push_with_capabilities(
                    name,
                    Arc::new(RewriteValue {
                        from: 1,
                        to: serde_json::json!({ "value": 2 }),
                    }),
                    HookCapabilities {
                        may_rewrite_input: true,
                        ..HookCapabilities::default()
                    },
                )
                .expect("hook registers");
        }
        let approvals = ApprovalBroker::new();
        let engine = AuthorizationEngine::new(&registry, &policy, &hooks, &approvals);

        let outcome = engine
            .authorize(
                PendingToolCall::new("call-1", "counting", serde_json::json!({ "value": 1 })),
                &mut SessionPolicyOverlay::default(),
                &[],
                0,
                &CancellationToken::new(),
            )
            .await
            .expect("authorization resolves");

        let AuthorizationOutcome::Authorized(authorized) = outcome else {
            panic!("equivalent replacements should authorize");
        };
        assert_eq!(authorized.request().generation(), 1);
        assert_eq!(authorized.receipt().rewrite_count(), 1);
        assert_eq!(prepares.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn conflicting_hook_replacements_fail_closed() {
        let registry = registry(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let policy = allow_policy();
        let mut hooks = Hooks::new();
        for (name, value) in [("two", 2), ("three", 3)] {
            hooks
                .push_with_capabilities(
                    name,
                    Arc::new(RewriteValue {
                        from: 1,
                        to: serde_json::json!({ "value": value }),
                    }),
                    HookCapabilities {
                        may_rewrite_input: true,
                        ..HookCapabilities::default()
                    },
                )
                .expect("hook registers");
        }
        let approvals = ApprovalBroker::new();
        let engine = AuthorizationEngine::new(&registry, &policy, &hooks, &approvals);

        let outcome = engine
            .authorize(
                PendingToolCall::new("call-1", "counting", serde_json::json!({ "value": 1 })),
                &mut SessionPolicyOverlay::default(),
                &[],
                0,
                &CancellationToken::new(),
            )
            .await
            .expect("authorization resolves");

        assert_eq!(rejected_kind(outcome), "hook_rewrite_failed");
    }

    #[tokio::test]
    async fn hook_rewrite_cycle_and_fifth_rewrite_fail_closed() {
        for (name, hook) in [
            ("cycle", Arc::new(ToggleRewrite) as Arc<dyn Hook>),
            ("limit", Arc::new(IncrementRewrite) as Arc<dyn Hook>),
        ] {
            let registry = registry(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
            let policy = allow_policy();
            let mut hooks = Hooks::new();
            hooks
                .push_with_capabilities(
                    name,
                    hook,
                    HookCapabilities {
                        may_rewrite_input: true,
                        ..HookCapabilities::default()
                    },
                )
                .expect("hook registers");
            let approvals = ApprovalBroker::new();
            let engine = AuthorizationEngine::new(&registry, &policy, &hooks, &approvals);

            let outcome = engine
                .authorize(
                    PendingToolCall::new("call-1", "counting", serde_json::json!({ "value": 1 })),
                    &mut SessionPolicyOverlay::default(),
                    &[],
                    0,
                    &CancellationToken::new(),
                )
                .await
                .expect("authorization resolves");

            assert_eq!(rejected_kind(outcome), "hook_rewrite_failed", "{name}");
        }
    }

    #[tokio::test]
    async fn invalid_hook_replacement_fails_closed() {
        let registry = registry(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let policy = allow_policy();
        let mut hooks = Hooks::new();
        hooks
            .push_with_capabilities(
                "invalid",
                Arc::new(RewriteValue {
                    from: 1,
                    to: serde_json::Value::Null,
                }),
                HookCapabilities {
                    may_rewrite_input: true,
                    ..HookCapabilities::default()
                },
            )
            .expect("hook registers");
        let approvals = ApprovalBroker::new();
        let engine = AuthorizationEngine::new(&registry, &policy, &hooks, &approvals);

        let outcome = engine
            .authorize(
                PendingToolCall::new("call-1", "counting", serde_json::json!({ "value": 1 })),
                &mut SessionPolicyOverlay::default(),
                &[],
                0,
                &CancellationToken::new(),
            )
            .await
            .expect("authorization resolves");

        assert_eq!(rejected_kind(outcome), "hook_rewrite_failed");
    }

    #[tokio::test]
    async fn hook_ask_beats_hook_allow_and_hook_failure_denies() {
        for (name, second) in [
            ("ask", Arc::new(AskHook) as Arc<dyn Hook>),
            ("failure", Arc::new(FailingHook) as Arc<dyn Hook>),
        ] {
            let registry = registry(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
            let policy = PolicySet::new(root());
            let mut hooks = Hooks::new();
            hooks
                .push_with_capabilities(
                    "allow",
                    Arc::new(AllowHook),
                    HookCapabilities {
                        may_allow: true,
                        ..HookCapabilities::default()
                    },
                )
                .expect("allow hook registers");
            hooks
                .push_with_capabilities(name, second, HookCapabilities::default())
                .expect("restrictive hook registers");
            let approvals = ApprovalBroker::new();
            let engine = AuthorizationEngine::new(&registry, &policy, &hooks, &approvals);

            let outcome = engine
                .authorize(
                    PendingToolCall::new("call-1", "counting", serde_json::json!({})),
                    &mut SessionPolicyOverlay::default(),
                    &[],
                    0,
                    &CancellationToken::new(),
                )
                .await
                .expect("authorization resolves");

            assert_eq!(rejected_kind(outcome), "permission_denied", "{name}");
        }
    }

    #[tokio::test]
    async fn approval_hook_requires_answer_and_rewrite_capabilities() {
        for (name, hook, capabilities, authorized) in [
            (
                "uncapable-approve",
                ApprovalHookResolution::Approve,
                HookCapabilities::default(),
                false,
            ),
            (
                "capable-approve",
                ApprovalHookResolution::Approve,
                HookCapabilities {
                    may_answer_approval: true,
                    ..HookCapabilities::default()
                },
                true,
            ),
            (
                "uncapable-rewrite",
                ApprovalHookResolution::Replace,
                HookCapabilities {
                    may_answer_approval: true,
                    ..HookCapabilities::default()
                },
                false,
            ),
        ] {
            let registry = registry(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
            let policy = PolicySet::new(root());
            let mut hooks = Hooks::new();
            hooks
                .push_with_capabilities(
                    name,
                    Arc::new(ApprovalHook { resolution: hook }),
                    capabilities,
                )
                .expect("hook registers");
            let approvals = ApprovalBroker::new();
            let engine = AuthorizationEngine::new(&registry, &policy, &hooks, &approvals);

            let outcome = engine
                .authorize(
                    PendingToolCall::new("call-1", "counting", serde_json::json!({})),
                    &mut SessionPolicyOverlay::default(),
                    &[],
                    0,
                    &CancellationToken::new(),
                )
                .await
                .expect("authorization resolves");

            assert_eq!(
                matches!(outcome, AuthorizationOutcome::Authorized(_)),
                authorized,
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn approval_hook_persistence_requires_the_registered_scope() {
        for (name, scopes, authorized) in [
            ("no-scope", PolicyScopeSet::NONE, false),
            ("session-scope", PolicyScopeSet::SESSION, true),
        ] {
            let registry = registry(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
            let policy = PolicySet::new(root());
            let mut hooks = Hooks::new();
            hooks
                .push_with_capabilities(
                    name,
                    Arc::new(ApprovalHook {
                        resolution: ApprovalHookResolution::ApprovePersisted,
                    }),
                    HookCapabilities {
                        may_answer_approval: true,
                        policy_mutation_scopes: scopes,
                        ..HookCapabilities::default()
                    },
                )
                .expect("hook registers");
            let approvals = ApprovalBroker::new();
            let engine = AuthorizationEngine::new(&registry, &policy, &hooks, &approvals);
            let mut overlay = SessionPolicyOverlay::default();

            let outcome = engine
                .authorize(
                    PendingToolCall::new("call-1", "counting", serde_json::json!({})),
                    &mut overlay,
                    &[],
                    0,
                    &CancellationToken::new(),
                )
                .await
                .expect("authorization resolves");

            assert_eq!(
                matches!(outcome, AuthorizationOutcome::Authorized(_)),
                authorized,
                "{name}"
            );
            assert_eq!(overlay.rules().len(), usize::from(authorized), "{name}");
        }
    }

    #[tokio::test]
    async fn dont_ask_rejects_without_invoking_a_resolver() {
        let registry = registry(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let policy = PolicySet::new(root());
        let hooks = Hooks::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut approvals = ApprovalBroker::new();
        approvals.push(Arc::new(CountingResolver {
            calls: calls.clone(),
        }));
        let engine = AuthorizationEngine::new(&registry, &policy, &hooks, &approvals);
        let mut overlay = SessionPolicyOverlay::new(PermissionMode::DontAsk);

        let outcome = engine
            .authorize(
                PendingToolCall::new("call-1", "counting", serde_json::json!({})),
                &mut overlay,
                &[],
                0,
                &CancellationToken::new(),
            )
            .await
            .expect("authorization resolves");

        assert_eq!(rejected_kind(outcome), "permission_denied");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn resolver_cannot_select_a_persistence_template_in_the_wrong_direction() {
        let registry = registry(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let policy = PolicySet::new(root());
        let hooks = Hooks::new();
        let mut approvals = ApprovalBroker::new();
        approvals.push(Arc::new(WrongDirectionResolver));
        let engine = AuthorizationEngine::new(&registry, &policy, &hooks, &approvals);
        let mut overlay = SessionPolicyOverlay::default();

        let outcome = engine
            .authorize(
                PendingToolCall::new("call-1", "counting", serde_json::json!({})),
                &mut overlay,
                &[],
                0,
                &CancellationToken::new(),
            )
            .await
            .expect("authorization resolves");

        assert_eq!(rejected_kind(outcome), "permission_denied");
        assert!(overlay.rules().is_empty());
    }

    #[tokio::test]
    async fn receipt_cannot_cross_registry_hook_or_session_snapshots() {
        let executions = Arc::new(AtomicUsize::new(0));
        let registry_a = registry(Arc::new(AtomicUsize::new(0)), executions.clone());
        let registry_b = registry(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let policy = allow_policy();
        let hooks_a = Hooks::new();
        let hooks_b = Hooks::new();
        let approvals = ApprovalBroker::new();
        let overlay_a = &mut SessionPolicyOverlay::default();
        let engine_a = AuthorizationEngine::new(&registry_a, &policy, &hooks_a, &approvals);
        let outcome = engine_a
            .authorize(
                PendingToolCall::new("call-1", "counting", serde_json::json!({})),
                overlay_a,
                &[],
                0,
                &CancellationToken::new(),
            )
            .await
            .expect("authorization resolves");
        let AuthorizationOutcome::Authorized(authorized) = outcome else {
            panic!("expected authorization");
        };
        let overlay_b = SessionPolicyOverlay::default();
        let engine_b = AuthorizationEngine::new(&registry_b, &policy, &hooks_b, &approvals);

        let execution = engine_b
            .execute(authorized, &overlay_b, &ToolContext::new())
            .await
            .expect("staleness is recoverable");

        assert!(matches!(execution, ExecutionOutcome::Stale(_)));
        assert_eq!(executions.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn sensitive_missing_path_reaches_approval_before_existence_diagnostics() {
        let tmp = TestDir::new();
        let workspace = tmp.workspace().await;
        let root = CanonicalPath::from_absolute(workspace.root()).expect("canonical root");
        let registry =
            ToolRegistry::with_default_workspace_tools(workspace).expect("default registry builds");
        let policy = PolicySet::builtin(root).expect("builtins compile");
        let hooks = Hooks::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut approvals = ApprovalBroker::new();
        approvals.push(Arc::new(CountingResolver {
            calls: calls.clone(),
        }));
        let engine = AuthorizationEngine::new(&registry, &policy, &hooks, &approvals);

        let outcome = engine
            .authorize(
                PendingToolCall::new("call-1", "read_file", serde_json::json!({ "path": ".env" })),
                &mut SessionPolicyOverlay::default(),
                &[],
                0,
                &CancellationToken::new(),
            )
            .await
            .expect("authorization resolves");

        let AuthorizationOutcome::Authorized(_) = outcome else {
            panic!("the scripted resolver should approve the protected missing target");
        };
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn path_replacement_after_authorization_never_reads_outside_workspace() {
        use std::{fs, os::unix::fs::symlink};

        let tmp = TestDir::new();
        let outside = TestDir::new();
        fs::write(tmp.path().join("safe.txt"), "safe").expect("inside file exists");
        let outside_file = outside.path().join("secret.txt");
        fs::write(&outside_file, "secret").expect("outside file exists");
        let workspace = tmp.workspace().await;
        let root = CanonicalPath::from_absolute(workspace.root()).expect("canonical root");
        let registry =
            ToolRegistry::with_default_workspace_tools(workspace).expect("default registry builds");
        let policy = PolicySet::builtin(root).expect("builtins compile");
        let hooks = Hooks::new();
        let approvals = ApprovalBroker::new();
        let engine = AuthorizationEngine::new(&registry, &policy, &hooks, &approvals);
        let overlay = &mut SessionPolicyOverlay::default();
        let outcome = engine
            .authorize(
                PendingToolCall::new(
                    "call-1",
                    "read_file",
                    serde_json::json!({ "path": "safe.txt" }),
                ),
                overlay,
                &[],
                0,
                &CancellationToken::new(),
            )
            .await
            .expect("authorization resolves");
        let AuthorizationOutcome::Authorized(authorized) = outcome else {
            panic!("ordinary workspace read should authorize");
        };
        fs::remove_file(tmp.path().join("safe.txt")).expect("inside file is removed");
        symlink(&outside_file, tmp.path().join("safe.txt")).expect("escape symlink is installed");

        let execution = engine
            .execute(authorized, overlay, &ToolContext::new())
            .await
            .expect("staleness is recoverable");
        let ExecutionOutcome::Stale(stale) = execution else {
            panic!("changed path must invalidate authorization");
        };
        let retried = engine
            .authorize(stale, overlay, &[], 0, &CancellationToken::new())
            .await
            .expect("retry resolves safely");

        assert_eq!(rejected_kind(retried), "workspace_path");
    }
}
