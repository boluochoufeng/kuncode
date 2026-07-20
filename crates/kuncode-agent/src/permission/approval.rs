//! Challenge-bound approval resolution and narrow policy mutation templates.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use kuncode_core::non_empty_vec::NonEmptyVec;
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::{
    AuthorizationContextRevision, AuthorizationRequest, AuthorizationResolution,
    CanonicalToolInput, PermissionCauseId, PermissionCheckId, PermissionRule, PermissionTarget,
    PolicyEffect, PolicyOrigin, RequestFingerprint, ResolutionBasis, ToolDisplay, ToolIdentity,
    check::hex_digest,
};

const CHALLENGE_ID_DOMAIN: &[u8] = b"kuncode.approval-challenge.v1\0";
const TEMPLATE_ID_DOMAIN: &[u8] = b"kuncode.policy-mutation-template.v1\0";

/// Runtime policy destination exposed as a constrained challenge option.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyScope {
    /// Current challenge only; no policy mutation.
    Once,
    /// Current session overlay.
    Session,
    /// User-level settings managed by the frontend.
    User,
    /// Machine-local settings for the trusted project.
    ProjectLocal,
    /// Shared project settings, never writable from runtime approval.
    ProjectShared,
    /// Administrator policy, never writable from runtime approval.
    Managed,
}

/// Capability set restricting which policy scopes a Hook may mutate.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PolicyScopeSet(u8);

impl PolicyScopeSet {
    /// No persistent mutation capability.
    pub const NONE: Self = Self(0);
    /// Session-only mutation capability.
    pub const SESSION: Self = Self(1 << 0);
    /// User-settings mutation capability.
    pub const USER: Self = Self(1 << 1);
    /// Project-local mutation capability.
    pub const PROJECT_LOCAL: Self = Self(1 << 2);

    /// Returns whether the set contains `scope`.
    pub const fn contains(self, scope: PolicyScope) -> bool {
        let bit = match scope {
            PolicyScope::Session => Self::SESSION.0,
            PolicyScope::User => Self::USER.0,
            PolicyScope::ProjectLocal => Self::PROJECT_LOCAL.0,
            PolicyScope::Once | PolicyScope::ProjectShared | PolicyScope::Managed => 0,
        };
        bit != 0 && self.0 & bit == bit
    }

    /// Combines independently granted runtime scopes.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Unforgeable identity of one engine-generated challenge.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ApprovalChallengeId(String);

impl ApprovalChallengeId {
    /// Returns the opaque challenge identity.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Identity of one immutable mutation option included in a challenge.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PolicyMutationTemplateId(String);

impl PolicyMutationTemplateId {
    /// Returns the opaque option identity submitted by a frontend.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Exact, engine-generated mutation option; frontends can select only its ID.
#[derive(Clone, Debug)]
pub struct PolicyMutationTemplate {
    id: PolicyMutationTemplateId,
    effect: PolicyEffect,
    scope: PolicyScope,
    target: PermissionTarget,
    rule: PermissionRule,
}

impl PolicyMutationTemplate {
    fn exact_session(
        target: PermissionTarget,
        effect: PolicyEffect,
    ) -> Result<Self, ApprovalError> {
        #[derive(Serialize)]
        struct TemplateIdentity<'a> {
            effect: PolicyEffect,
            scope: PolicyScope,
            target: &'a PermissionTarget,
        }
        let scope = PolicyScope::Session;
        let mut hasher = Sha256::new();
        hasher.update(TEMPLATE_ID_DOMAIN);
        hasher.update(serde_json::to_vec(&TemplateIdentity {
            effect,
            scope,
            target: &target,
        })?);
        let id = PolicyMutationTemplateId(hex_digest(hasher.finalize().as_slice()));
        let rule = PermissionRule::exact_target(target.clone(), effect, PolicyOrigin::Session)?;
        Ok(Self {
            id,
            effect,
            scope,
            target,
            rule,
        })
    }

    /// Returns the immutable option identity.
    pub fn id(&self) -> &PolicyMutationTemplateId {
        &self.id
    }

    /// Returns the fixed mutation direction.
    pub const fn effect(&self) -> PolicyEffect {
        self.effect
    }

    /// Returns the fixed persistence destination.
    pub const fn scope(&self) -> PolicyScope {
        self.scope
    }

    /// Returns the exact target selected by the engine.
    pub fn target(&self) -> &PermissionTarget {
        &self.target
    }

    pub(crate) fn rule(&self) -> &PermissionRule {
        &self.rule
    }
}

/// Safe request view presented to approval resolvers.
#[derive(Clone, Debug)]
pub struct SafeRequestSnapshot {
    tool: ToolIdentity,
    display: ToolDisplay,
}

impl SafeRequestSnapshot {
    /// Returns the registry-owned tool identity.
    pub fn tool(&self) -> &ToolIdentity {
        &self.tool
    }

    /// Returns bounded non-authoritative display text.
    pub fn display(&self) -> &ToolDisplay {
        &self.display
    }
}

/// One check and the precise causes awaiting approval.
#[derive(Clone, Debug)]
pub struct PendingApprovalCheck {
    check_id: PermissionCheckId,
    target: PermissionTarget,
    causes: NonEmptyVec<PermissionCauseId>,
}

impl PendingApprovalCheck {
    /// Returns the request-local check identity.
    pub fn check_id(&self) -> &PermissionCheckId {
        &self.check_id
    }

    /// Returns the concrete target shown to the user.
    pub fn target(&self) -> &PermissionTarget {
        &self.target
    }

    /// Returns every restrictive cause this approval must satisfy.
    pub fn causes(&self) -> &NonEmptyVec<PermissionCauseId> {
        &self.causes
    }
}

/// Single-use approval challenge bound to an exact request and context.
#[derive(Clone, Debug)]
pub struct ApprovalChallenge {
    id: ApprovalChallengeId,
    request_fingerprint: RequestFingerprint,
    context_revision: AuthorizationContextRevision,
    request_snapshot: SafeRequestSnapshot,
    pending_checks: NonEmptyVec<PendingApprovalCheck>,
    mutation_options: Vec<PolicyMutationTemplate>,
    expires_at: Option<Instant>,
}

impl ApprovalChallenge {
    pub(crate) fn new(
        nonce: u64,
        request: &AuthorizationRequest,
        resolution: &AuthorizationResolution,
        context_revision: AuthorizationContextRevision,
        expires_at: Option<Instant>,
    ) -> Result<Self, ApprovalError> {
        let pending = resolution
            .checks()
            .iter()
            .filter(|check| check.effect() == PolicyEffect::RequireApproval)
            .map(|check| {
                let causes = NonEmptyVec::try_from(check.deciding_causes().to_vec())
                    .map_err(|_| ApprovalError::MissingApprovalCause)?;
                Ok(PendingApprovalCheck {
                    check_id: check.check().id().clone(),
                    target: check.check().target().clone(),
                    causes,
                })
            })
            .collect::<Result<Vec<_>, ApprovalError>>()?;
        let pending_checks =
            NonEmptyVec::try_from(pending).map_err(|_| ApprovalError::NoPendingChecks)?;

        let mut mutation_options = Vec::new();
        for check in resolution.checks().iter() {
            if check.effect() != PolicyEffect::RequireApproval {
                continue;
            }
            if check.basis() == &ResolutionBasis::ProfileDefault {
                mutation_options.push(PolicyMutationTemplate::exact_session(
                    check.check().target().clone(),
                    PolicyEffect::Allow,
                )?);
            }
            mutation_options.push(PolicyMutationTemplate::exact_session(
                check.check().target().clone(),
                PolicyEffect::Deny,
            )?);
        }
        mutation_options.sort_by(|left, right| left.id.cmp(&right.id));
        mutation_options.dedup_by(|left, right| left.id == right.id);

        let id = challenge_id(nonce, request, &context_revision)?;
        Ok(Self {
            id,
            request_fingerprint: request.request_fingerprint().clone(),
            context_revision,
            request_snapshot: SafeRequestSnapshot {
                tool: request.tool().clone(),
                display: request.display().clone(),
            },
            pending_checks,
            mutation_options,
            expires_at,
        })
    }

    /// Returns the single-use challenge identity.
    pub fn id(&self) -> &ApprovalChallengeId {
        &self.id
    }

    /// Returns the exact request identity awaiting approval.
    pub fn request_fingerprint(&self) -> &RequestFingerprint {
        &self.request_fingerprint
    }

    /// Returns the context version captured before waiting.
    pub fn context_revision(&self) -> &AuthorizationContextRevision {
        &self.context_revision
    }

    /// Returns the bounded UI snapshot.
    pub fn request_snapshot(&self) -> &SafeRequestSnapshot {
        &self.request_snapshot
    }

    /// Returns all checks and causes that need approval.
    pub fn pending_checks(&self) -> &NonEmptyVec<PendingApprovalCheck> {
        &self.pending_checks
    }

    /// Returns immutable engine-generated persistence choices.
    pub fn mutation_options(&self) -> &[PolicyMutationTemplate] {
        &self.mutation_options
    }

    /// Returns the optional monotonic expiry deadline.
    pub const fn expires_at(&self) -> Option<Instant> {
        self.expires_at
    }

    pub(crate) fn mutation(
        &self,
        id: &PolicyMutationTemplateId,
    ) -> Option<&PolicyMutationTemplate> {
        self.mutation_options
            .iter()
            .find(|option| option.id() == id)
    }
}

/// Result returned by an approval resolver.
#[derive(Clone, Debug)]
pub enum ApprovalResolution {
    /// Lets the next resolver handle the challenge.
    Abstain,
    /// Approves this request, optionally selecting a listed Allow mutation.
    Approve {
        /// Immutable option selected from [`ApprovalChallenge::mutation_options`].
        persistence: Option<PolicyMutationTemplateId>,
    },
    /// Rejects this request, optionally selecting a listed Deny mutation.
    Deny {
        /// Immutable option selected from [`ApprovalChallenge::mutation_options`].
        persistence: Option<PolicyMutationTemplateId>,
    },
    /// Replaces input and restarts preparation without approving the old request.
    ReplaceInput(CanonicalToolInput),
    /// Cancels the current agent operation.
    Cancel,
}

/// Frontend or automation endpoint capable of resolving a challenge.
#[async_trait]
pub trait ApprovalResolver: Send + Sync {
    /// Returns one challenge-bound decision without constructing policy rules.
    async fn resolve(&self, challenge: &ApprovalChallenge) -> ApprovalResolution;
}

/// Safe final resolver used whenever no interactive endpoint is available.
#[derive(Clone, Copy, Debug, Default)]
pub struct RejectUnavailable;

#[async_trait]
impl ApprovalResolver for RejectUnavailable {
    async fn resolve(&self, _challenge: &ApprovalChallenge) -> ApprovalResolution {
        ApprovalResolution::Deny { persistence: None }
    }
}

/// Ordered resolver chain with single-use challenge enforcement.
#[derive(Default)]
pub struct ApprovalBroker {
    resolvers: Vec<Arc<dyn ApprovalResolver>>,
    consumed: Arc<Mutex<BTreeSet<String>>>,
}

impl Clone for ApprovalBroker {
    fn clone(&self) -> Self {
        Self {
            resolvers: self.resolvers.clone(),
            consumed: self.consumed.clone(),
        }
    }
}

impl ApprovalBroker {
    /// Creates a broker whose final behavior is [`RejectUnavailable`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a resolver before the fail-closed fallback.
    pub fn push(&mut self, resolver: Arc<dyn ApprovalResolver>) {
        self.resolvers.push(resolver);
    }

    /// Resolves a challenge once; all abstentions fall through to rejection.
    ///
    /// # Errors
    /// Returns an error for duplicate or expired challenge use.
    pub async fn resolve(
        &self,
        challenge: &ApprovalChallenge,
    ) -> Result<ApprovalResolution, ApprovalBrokerError> {
        self.claim(challenge)?;
        self.resolve_claimed(challenge).await
    }

    pub(crate) fn claim(&self, challenge: &ApprovalChallenge) -> Result<(), ApprovalBrokerError> {
        if challenge
            .expires_at()
            .is_some_and(|expiry| expiry <= Instant::now())
        {
            tracing::warn!(
                target: "kuncode::authorization",
                challenge_id = challenge.id().as_str(),
                reason = "expired_before_claim",
                "approval challenge rejected",
            );
            return Err(ApprovalBrokerError::Expired);
        }
        {
            let mut consumed = self
                .consumed
                .lock()
                .map_err(|_| ApprovalBrokerError::StateUnavailable)?;
            if !consumed.insert(challenge.id().as_str().to_string()) {
                tracing::warn!(
                    target: "kuncode::authorization",
                    challenge_id = challenge.id().as_str(),
                    reason = "already_consumed",
                    "approval challenge rejected",
                );
                return Err(ApprovalBrokerError::AlreadyConsumed);
            }
        }
        Ok(())
    }

    pub(crate) async fn resolve_claimed(
        &self,
        challenge: &ApprovalChallenge,
    ) -> Result<ApprovalResolution, ApprovalBrokerError> {
        for (resolver_index, resolver) in self.resolvers.iter().enumerate() {
            let resolution = resolver.resolve(challenge).await;
            if challenge
                .expires_at()
                .is_some_and(|expiry| expiry <= Instant::now())
            {
                tracing::warn!(
                    target: "kuncode::authorization",
                    challenge_id = challenge.id().as_str(),
                    resolver_index,
                    reason = "expired_after_resolution",
                    "approval challenge rejected",
                );
                return Err(ApprovalBrokerError::Expired);
            }
            tracing::info!(
                target: "kuncode::authorization",
                challenge_id = challenge.id().as_str(),
                resolver_index,
                outcome = approval_resolution_name(&resolution),
                "approval resolver completed",
            );
            if !matches!(resolution, ApprovalResolution::Abstain) {
                return Ok(resolution);
            }
        }
        tracing::info!(
            target: "kuncode::authorization",
            challenge_id = challenge.id().as_str(),
            outcome = "reject_unavailable",
            "approval resolver chain exhausted",
        );
        Ok(ApprovalResolution::Deny { persistence: None })
    }
}

/// Internal proof that one approval covers only the listed causes.
#[derive(Clone, Debug)]
pub(crate) struct ApprovalSatisfaction {
    challenge_id: ApprovalChallengeId,
    request_fingerprint: RequestFingerprint,
    context_revision: AuthorizationContextRevision,
    approved: BTreeMap<PermissionCheckId, BTreeSet<PermissionCauseId>>,
}

impl ApprovalSatisfaction {
    pub(crate) fn from_challenge(
        challenge: &ApprovalChallenge,
        context_revision: AuthorizationContextRevision,
    ) -> Self {
        let approved = challenge
            .pending_checks()
            .iter()
            .map(|check| {
                (
                    check.check_id().clone(),
                    check.causes().iter().cloned().collect(),
                )
            })
            .collect();
        Self {
            challenge_id: challenge.id().clone(),
            request_fingerprint: challenge.request_fingerprint().clone(),
            context_revision,
            approved,
        }
    }

    pub(crate) fn covers(
        &self,
        request: &AuthorizationRequest,
        context_revision: &AuthorizationContextRevision,
        resolution: &AuthorizationResolution,
    ) -> bool {
        if &self.request_fingerprint != request.request_fingerprint()
            || &self.context_revision != context_revision
        {
            return false;
        }
        resolution.checks().iter().all(|check| {
            if check.effect() != PolicyEffect::RequireApproval {
                return true;
            }
            self.approved
                .get(check.check().id())
                .is_some_and(|approved| {
                    check
                        .deciding_causes()
                        .iter()
                        .all(|cause| approved.contains(cause))
                })
        })
    }

    pub(crate) fn challenge_id(&self) -> &ApprovalChallengeId {
        &self.challenge_id
    }
}

/// Invalid challenge or mutation construction.
#[derive(Debug, Error)]
pub enum ApprovalError {
    /// A challenge may be created only for an Ask resolution.
    #[error("approval challenge contains no pending checks")]
    NoPendingChecks,
    /// Every Ask must have at least one stable cause.
    #[error("approval check has no stable cause")]
    MissingApprovalCause,
    /// Stable challenge data could not be encoded.
    #[error("failed to encode approval challenge: {0}")]
    Encoding(#[from] serde_json::Error),
}

/// Failure enforcing broker lifecycle invariants.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum ApprovalBrokerError {
    /// A challenge response arrived after its deadline.
    #[error("approval challenge expired")]
    Expired,
    /// A challenge ID can be resolved only once.
    #[error("approval challenge was already consumed")]
    AlreadyConsumed,
    /// Internal one-time state could not be accessed safely.
    #[error("approval broker state is unavailable")]
    StateUnavailable,
}

fn challenge_id(
    nonce: u64,
    request: &AuthorizationRequest,
    context: &AuthorizationContextRevision,
) -> Result<ApprovalChallengeId, serde_json::Error> {
    #[derive(Serialize)]
    struct ChallengeIdentity<'a> {
        nonce: u64,
        unix_nanos: u128,
        request_fingerprint: &'a str,
        policy_revision: u64,
        session_revision: u64,
        hook_revision: u64,
        tool_revision: u64,
    }
    let unix_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let identity = ChallengeIdentity {
        nonce,
        unix_nanos,
        request_fingerprint: request.request_fingerprint().as_str(),
        policy_revision: context.policy_set().get(),
        session_revision: context.session_overlay().get(),
        hook_revision: context.hook_registry().get(),
        tool_revision: context.tool_registry().get(),
    };
    let mut hasher = Sha256::new();
    hasher.update(CHALLENGE_ID_DOMAIN);
    hasher.update(serde_json::to_vec(&identity)?);
    Ok(ApprovalChallengeId(hex_digest(
        hasher.finalize().as_slice(),
    )))
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

#[cfg(test)]
pub(crate) mod tests_support {
    use std::{collections::VecDeque, sync::Mutex};

    use super::*;

    /// Deterministic resolver used by authorization tests.
    pub struct ScriptedApprovalResolver {
        resolutions: Mutex<VecDeque<ApprovalResolution>>,
    }

    impl ScriptedApprovalResolver {
        /// Creates a resolver returning the supplied sequence.
        pub fn new(resolutions: impl IntoIterator<Item = ApprovalResolution>) -> Self {
            Self {
                resolutions: Mutex::new(resolutions.into_iter().collect()),
            }
        }
    }

    #[async_trait]
    impl ApprovalResolver for ScriptedApprovalResolver {
        async fn resolve(&self, _challenge: &ApprovalChallenge) -> ApprovalResolution {
            self.resolutions
                .lock()
                .ok()
                .and_then(|mut resolutions| resolutions.pop_front())
                .unwrap_or(ApprovalResolution::Deny { persistence: None })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, time::Duration};

    use super::*;
    use crate::{
        hook::Hooks,
        permission::{
            CanonicalCommand, CanonicalPath, CommandKind, PermissionCheckSpec, PermissionMode,
            PermissionNamespace, PolicySet, ProfileDefault, SessionPolicyOverlay,
            ToolPermissionProfile,
        },
        registry::ToolRegistry,
    };

    struct Fixture {
        request: AuthorizationRequest,
        resolution: AuthorizationResolution,
        context: AuthorizationContextRevision,
    }

    fn root() -> CanonicalPath {
        CanonicalPath::from_absolute(Path::new("/workspace")).expect("absolute root")
    }

    fn fixture(input: serde_json::Value, explicit_ask: bool) -> Fixture {
        let profile = ToolPermissionProfile::exact_tool("counting").expect("valid profile");
        let checks = profile
            .validate([PermissionCheckSpec::new(
                PermissionTarget::exact_tool("counting").expect("valid target"),
            )])
            .expect("checks validate");
        let request = AuthorizationRequest::new(
            "call-1",
            0,
            ToolIdentity::new("counting").expect("valid tool"),
            CanonicalToolInput::new(input),
            checks,
            ToolDisplay::new("Run counting tool"),
            profile.revision().clone(),
        )
        .expect("request encodes");
        let mut policy = PolicySet::new(root());
        if explicit_ask {
            policy
                .compile_and_push(
                    "ExactTool(counting)",
                    PolicyEffect::RequireApproval,
                    PolicyOrigin::Managed,
                )
                .expect("ask compiles");
        }
        let overlay = SessionPolicyOverlay::default();
        let hooks = Hooks::new();
        let registry = ToolRegistry::new();
        let resolution = policy
            .resolve(&request, &[], PermissionMode::Default, &[])
            .expect("policy resolves");
        let context = AuthorizationContextRevision::new(
            policy.revision(),
            overlay.revision(),
            hooks.revision(),
            registry.revision(),
        );
        Fixture {
            request,
            resolution,
            context,
        }
    }

    fn challenge(fixture: &Fixture, nonce: u64) -> ApprovalChallenge {
        ApprovalChallenge::new(
            nonce,
            &fixture.request,
            &fixture.resolution,
            fixture.context.clone(),
            None,
        )
        .expect("challenge builds")
    }

    #[tokio::test]
    async fn broker_clones_share_single_use_state() {
        let fixture = fixture(serde_json::json!({}), false);
        let challenge = challenge(&fixture, 1);
        let broker = ApprovalBroker::new();
        let cloned = broker.clone();

        assert!(matches!(
            broker.resolve(&challenge).await,
            Ok(ApprovalResolution::Deny { persistence: None })
        ));
        assert_eq!(
            cloned.resolve(&challenge).await.expect_err("replay fails"),
            ApprovalBrokerError::AlreadyConsumed
        );
    }

    #[tokio::test]
    async fn expired_challenge_is_never_offered_to_resolvers() {
        let fixture = fixture(serde_json::json!({}), false);
        let challenge = ApprovalChallenge::new(
            1,
            &fixture.request,
            &fixture.resolution,
            fixture.context,
            Some(Instant::now() - Duration::from_millis(1)),
        )
        .expect("challenge builds");

        assert_eq!(
            ApprovalBroker::new()
                .resolve(&challenge)
                .await
                .expect_err("expired challenge fails"),
            ApprovalBrokerError::Expired
        );
    }

    #[test]
    fn explicit_ask_does_not_offer_a_false_always_allow_template() {
        let default_fixture = fixture(serde_json::json!({}), false);
        let explicit_fixture = fixture(serde_json::json!({}), true);
        let default = challenge(&default_fixture, 1);
        let explicit = challenge(&explicit_fixture, 2);

        assert!(
            default
                .mutation_options()
                .iter()
                .any(|option| option.effect() == PolicyEffect::Allow)
        );
        assert!(
            explicit
                .mutation_options()
                .iter()
                .all(|option| option.effect() != PolicyEffect::Allow)
        );
        assert!(
            explicit
                .mutation_options()
                .iter()
                .any(|option| option.effect() == PolicyEffect::Deny)
        );
    }

    #[test]
    fn one_approval_does_not_cover_a_new_ask_cause_or_request() {
        let original = fixture(serde_json::json!({ "value": 1 }), false);
        let challenge = challenge(&original, 1);
        let satisfaction =
            ApprovalSatisfaction::from_challenge(&challenge, original.context.clone());
        assert!(satisfaction.covers(&original.request, &original.context, &original.resolution));

        let explicit = fixture(serde_json::json!({ "value": 1 }), true);
        assert!(!satisfaction.covers(&original.request, &original.context, &explicit.resolution));

        let changed_request = fixture(serde_json::json!({ "value": 2 }), false);
        assert!(!satisfaction.covers(
            &changed_request.request,
            &original.context,
            &changed_request.resolution
        ));
    }

    #[test]
    fn multi_check_challenge_keeps_every_target_and_narrow_template() {
        let profile = ToolPermissionProfile::new(
            "bash",
            [(PermissionNamespace::Bash, ProfileDefault::RequireApproval)],
            true,
        )
        .expect("valid profile");
        let checks = profile
            .validate(["cargo test", "git status"].map(|command| {
                PermissionCheckSpec::new(PermissionTarget::Bash(
                    CanonicalCommand::new(command, CommandKind::Simple).expect("valid command"),
                ))
            }))
            .expect("checks validate");
        let request = AuthorizationRequest::new(
            "call-1",
            0,
            ToolIdentity::new("bash").expect("valid tool"),
            CanonicalToolInput::new(serde_json::json!({ "cmd": "cargo test && git status" })),
            checks,
            ToolDisplay::new("Run command chain"),
            profile.revision().clone(),
        )
        .expect("request encodes");
        let policy = PolicySet::new(root());
        let resolution = policy
            .resolve(&request, &[], PermissionMode::Default, &[])
            .expect("policy resolves");
        let context = fixture(serde_json::json!({}), false).context;
        let challenge = ApprovalChallenge::new(1, &request, &resolution, context, None)
            .expect("challenge builds");

        assert_eq!(challenge.pending_checks().len(), 2);
        assert_eq!(challenge.mutation_options().len(), 4);
        for option in challenge.mutation_options() {
            assert!(
                challenge
                    .pending_checks()
                    .iter()
                    .any(|check| check.target() == option.target())
            );
            assert_eq!(option.scope(), PolicyScope::Session);
        }
    }
}
