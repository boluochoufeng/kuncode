//! Resolves typed checks against immutable, session, mode, and Hook policy.

use std::sync::atomic::{AtomicU64, Ordering};

use super::rule::{
    PermissionRule, PermissionRuleError, RuleCompileContext, compile_permission_rule,
};
use super::state::{PermissionMode, SessionPolicyOverlay};
use super::{
    AuthorizationRequest, AuthorizationResolution, CanonicalPath, PermissionCheck,
    PermissionTarget, PolicyContribution, PolicyEffect, PolicyOrigin, SafeExplanation,
    resolve_authorization, resolve_check,
};
use kuncode_core::non_empty_vec::NonEmptyVec;
use thiserror::Error;

static NEXT_POLICY_REVISION: AtomicU64 = AtomicU64::new(1);

/// Whether repository-owned configuration may relax this workspace's policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WorkspaceTrust {
    /// Project Allow rules and relaxing modes must not be activated.
    #[default]
    Untrusted,
    /// A user-controlled trust decision permits project relaxations.
    Trusted,
}

/// Monotonic immutable-policy version bound into authorization context.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PolicySetRevision(u64);

impl PolicySetRevision {
    /// Returns the monotonic version.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Product policy containing typed rules from every static source.
#[derive(Clone, Debug)]
pub struct PolicySet {
    rules: Vec<PermissionRule>,
    workspace_root: CanonicalPath,
    workspace_trust: WorkspaceTrust,
    revision: PolicySetRevision,
}

impl PolicySet {
    /// Creates an empty policy anchored to one canonical workspace.
    pub fn new(workspace_root: CanonicalPath) -> Self {
        Self {
            rules: Vec::new(),
            workspace_root,
            workspace_trust: WorkspaceTrust::Untrusted,
            revision: next_policy_revision(),
        }
    }

    pub(crate) fn fail_closed(workspace_root: CanonicalPath) -> Self {
        Self {
            rules: vec![PermissionRule::fail_closed()],
            workspace_root,
            workspace_trust: WorkspaceTrust::Untrusted,
            revision: next_policy_revision(),
        }
    }

    /// Creates the built-in hardening rules.
    ///
    /// # Errors
    /// Returns an error if a shipped rule is invalid; callers must fail startup
    /// rather than silently dropping a security rule.
    pub fn builtin(workspace_root: CanonicalPath) -> Result<Self, PolicySetError> {
        let mut policy = Self::new(workspace_root);
        for (effect, rule) in [
            (PolicyEffect::Deny, "Bash(sudo)"),
            (PolicyEffect::Deny, "Bash(sudo *)"),
            (PolicyEffect::Deny, "Bash(rm -rf /*)"),
            (PolicyEffect::Deny, "Bash(shutdown)"),
            (PolicyEffect::Deny, "Bash(shutdown *)"),
            (PolicyEffect::Deny, "Bash(reboot)"),
            (PolicyEffect::Deny, "Bash(reboot *)"),
            (PolicyEffect::Deny, "Bash(* > /dev/*)"),
            (PolicyEffect::RequireApproval, "Read(.env)"),
            (PolicyEffect::RequireApproval, "Read(**/.env)"),
            (PolicyEffect::RequireApproval, "Read(**/*.pem)"),
            (PolicyEffect::RequireApproval, "Read(**/id_rsa)"),
        ] {
            policy.compile_and_push(rule, effect, PolicyOrigin::Builtin)?;
        }
        Ok(policy)
    }

    /// Compiles and appends one rule.
    ///
    /// # Errors
    /// Returns a namespace-specific parse error for invalid syntax.
    pub fn compile_and_push(
        &mut self,
        input: &str,
        effect: PolicyEffect,
        origin: PolicyOrigin,
    ) -> Result<(), PolicySetError> {
        let context = RuleCompileContext::new(self.workspace_root.clone());
        let rule = compile_permission_rule(input, effect, origin, &context)?;
        self.push(rule);
        Ok(())
    }

    /// Appends an already compiled trusted rule and advances the revision.
    pub fn push(&mut self, rule: PermissionRule) {
        self.rules.push(rule);
        self.revision = next_policy_revision();
    }

    /// Appends another policy without applying source precedence.
    ///
    /// # Errors
    /// Returns an error when the two policies use different workspace anchors.
    pub fn append(&mut self, other: Self) -> Result<(), PolicySetError> {
        if self.workspace_root != other.workspace_root {
            return Err(PolicySetError::WorkspaceMismatch);
        }
        for rule in other.rules {
            self.push(rule);
        }
        Ok(())
    }

    /// Returns the immutable policy revision.
    pub const fn revision(&self) -> PolicySetRevision {
        self.revision
    }

    /// Returns the workspace anchor used by modes and relative matchers.
    pub fn workspace_root(&self) -> &CanonicalPath {
        &self.workspace_root
    }

    /// Records the user-controlled workspace trust decision in the revision.
    pub fn set_workspace_trust(&mut self, trust: WorkspaceTrust) {
        if self.workspace_trust != trust {
            self.workspace_trust = trust;
            self.revision = next_policy_revision();
        }
    }

    /// Returns the trust state bound into authorization context snapshots.
    pub const fn workspace_trust(&self) -> WorkspaceTrust {
        self.workspace_trust
    }

    /// Resolves every check with static, session, mode, and Hook contributions.
    ///
    /// # Errors
    /// Returns an error only when a profile-default cause cannot be encoded.
    pub fn resolve(
        &self,
        request: &AuthorizationRequest,
        session_rules: &[PermissionRule],
        mode: PermissionMode,
        hook_contributions: &[PolicyContribution],
    ) -> Result<AuthorizationResolution, PolicySetError> {
        let mut resolutions = Vec::with_capacity(request.checks().len());
        for check in request.checks().iter() {
            let mut contributions = self
                .rules
                .iter()
                .filter(|rule| self.static_rule_is_effective(rule))
                .chain(session_rules)
                .filter_map(|rule| rule.contribution(check))
                .collect::<Vec<_>>();
            contributions.extend(hook_contributions.iter().cloned());
            if let Some(contribution) = mode_contribution(mode, check, &self.workspace_root)? {
                contributions.push(contribution);
            }
            resolutions.push(resolve_check(
                check.clone(),
                contributions,
                request.profile_revision(),
            )?);
        }
        let resolutions = NonEmptyVec::try_from(resolutions)
            .map_err(|_| PolicySetError::EmptyAuthorizationRequest)?;
        Ok(resolve_authorization(resolutions))
    }

    /// Resolves against one revisioned, session-isolated overlay.
    ///
    /// # Errors
    /// Returns the same resolution failures as [`Self::resolve`].
    pub fn resolve_with_overlay(
        &self,
        request: &AuthorizationRequest,
        overlay: &SessionPolicyOverlay,
        hook_contributions: &[PolicyContribution],
    ) -> Result<AuthorizationResolution, PolicySetError> {
        self.resolve(request, overlay.rules(), overlay.mode(), hook_contributions)
    }

    /// Returns all static compiled rules for diagnostics and tests.
    pub fn rules(&self) -> &[PermissionRule] {
        &self.rules
    }

    fn static_rule_is_effective(&self, rule: &PermissionRule) -> bool {
        self.workspace_trust == WorkspaceTrust::Trusted
            || rule.origin() != &PolicyOrigin::Project
            || rule.effect() != PolicyEffect::Allow
    }
}

/// Invalid policy construction or resolution.
#[derive(Debug, Error)]
pub enum PolicySetError {
    /// A typed rule failed to compile.
    #[error(transparent)]
    Rule(#[from] PermissionRuleError),
    /// Policies anchored to different roots cannot be merged.
    #[error("permission policies use different workspace roots")]
    WorkspaceMismatch,
    /// Authorization requests are required to contain checks.
    #[error("authorization request contains no permission checks")]
    EmptyAuthorizationRequest,
    /// Cause encoding failed.
    #[error("failed to encode permission decision: {0}")]
    Encoding(#[from] serde_json::Error),
}

fn mode_contribution(
    mode: PermissionMode,
    check: &PermissionCheck,
    workspace_root: &CanonicalPath,
) -> Result<Option<PolicyContribution>, serde_json::Error> {
    let effect = match mode {
        PermissionMode::Default | PermissionMode::DontAsk => return Ok(None),
        PermissionMode::AcceptEdits => match check.target() {
            PermissionTarget::Edit(super::PathSelector::Exact { path })
                if path_is_inside(path, workspace_root) =>
            {
                PolicyEffect::Allow
            }
            _ => return Ok(None),
        },
        PermissionMode::Plan => match check.target() {
            PermissionTarget::Read(_) | PermissionTarget::TodoWrite => return Ok(None),
            PermissionTarget::Edit(_)
            | PermissionTarget::Bash(_)
            | PermissionTarget::WebFetch(_)
            | PermissionTarget::Mcp(_)
            | PermissionTarget::Agent(_)
            | PermissionTarget::ExactTool(_) => PolicyEffect::Deny,
        },
        PermissionMode::BypassPermissions => PolicyEffect::Allow,
    };
    #[derive(serde::Serialize)]
    struct Cause<'a> {
        mode: &'a str,
        check_id: &'a str,
    }
    let mode_name = match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "accept_edits",
        PermissionMode::Plan => "plan",
        PermissionMode::BypassPermissions => "bypass_permissions",
        PermissionMode::DontAsk => "dont_ask",
    };
    Ok(Some(PolicyContribution::new(
        effect,
        PolicyOrigin::Mode,
        super::PermissionCauseId::derive(&Cause {
            mode: mode_name,
            check_id: check.id().as_str(),
        })?,
        SafeExplanation::new(format!("permission mode {mode_name}")),
    )))
}

fn path_is_inside(path: &CanonicalPath, root: &CanonicalPath) -> bool {
    path.as_str() == root.as_str()
        || path
            .as_str()
            .strip_prefix(root.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn next_policy_revision() -> PolicySetRevision {
    PolicySetRevision(NEXT_POLICY_REVISION.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed_request(
        target: PermissionTarget,
        default: super::super::ProfileDefault,
    ) -> AuthorizationRequest {
        let namespace = target.namespace();
        let profile =
            super::super::ToolPermissionProfile::new("test_tool", [(namespace, default)], false)
                .expect("valid profile");
        let checks = profile
            .validate([super::super::PermissionCheckSpec::new(target)])
            .expect("valid check");
        AuthorizationRequest::new(
            "call-1",
            0,
            super::super::ToolIdentity::new("test_tool").expect("valid tool"),
            super::super::CanonicalToolInput::new(serde_json::json!({})),
            checks,
            super::super::ToolDisplay::new("test"),
            profile.revision().clone(),
        )
        .expect("request encodes")
    }

    fn root() -> CanonicalPath {
        CanonicalPath::from_absolute(std::path::Path::new("/workspace")).expect("absolute root")
    }

    #[test]
    fn typed_explicit_ask_beats_bypass_allow() {
        let request = typed_request(
            PermissionTarget::Bash(
                super::super::CanonicalCommand::new(
                    "cargo test",
                    super::super::CommandKind::Simple,
                )
                .expect("valid command"),
            ),
            super::super::ProfileDefault::RequireApproval,
        );
        let mut policy = PolicySet::new(root());
        policy
            .compile_and_push(
                "Bash(cargo *)",
                PolicyEffect::RequireApproval,
                PolicyOrigin::Managed,
            )
            .expect("rule compiles");

        let resolution = policy
            .resolve(&request, &[], PermissionMode::BypassPermissions, &[])
            .expect("policy resolves");
        assert_eq!(
            resolution.effect(),
            super::super::AuthorizationEffect::RequireApproval
        );
    }

    #[test]
    fn plan_denies_edits_while_accept_edits_allows_workspace_paths() {
        let target = PermissionTarget::Edit(super::super::PathSelector::exact(
            CanonicalPath::from_absolute(std::path::Path::new("/workspace/src/lib.rs"))
                .expect("absolute path"),
        ));
        let request = typed_request(target, super::super::ProfileDefault::RequireApproval);
        let policy = PolicySet::new(root());

        let plan = policy
            .resolve(&request, &[], PermissionMode::Plan, &[])
            .expect("policy resolves");
        let accept = policy
            .resolve(&request, &[], PermissionMode::AcceptEdits, &[])
            .expect("policy resolves");
        assert_eq!(plan.effect(), super::super::AuthorizationEffect::Deny);
        assert_eq!(accept.effect(), super::super::AuthorizationEffect::Allow);
    }

    #[test]
    fn builtin_sensitive_read_overrides_read_default() {
        let request = typed_request(
            PermissionTarget::Read(super::super::PathSelector::exact(
                CanonicalPath::from_absolute(std::path::Path::new("/workspace/.env"))
                    .expect("absolute path"),
            )),
            super::super::ProfileDefault::Allow,
        );
        let policy = PolicySet::builtin(root()).expect("builtins compile");
        let resolution = policy
            .resolve(&request, &[], PermissionMode::Default, &[])
            .expect("policy resolves");

        assert_eq!(
            resolution.effect(),
            super::super::AuthorizationEffect::RequireApproval
        );
    }

    #[test]
    fn untrusted_project_allow_is_ignored_but_project_deny_remains_effective() {
        let request = typed_request(
            PermissionTarget::Bash(
                super::super::CanonicalCommand::new(
                    "cargo test",
                    super::super::CommandKind::Simple,
                )
                .expect("valid command"),
            ),
            super::super::ProfileDefault::RequireApproval,
        );
        let mut policy = PolicySet::new(root());
        policy
            .compile_and_push(
                "Bash(cargo test)",
                PolicyEffect::Allow,
                PolicyOrigin::Project,
            )
            .expect("allow compiles");
        let untrusted = policy
            .resolve(&request, &[], PermissionMode::Default, &[])
            .expect("policy resolves");
        assert_eq!(
            untrusted.effect(),
            super::super::AuthorizationEffect::RequireApproval
        );

        policy.set_workspace_trust(WorkspaceTrust::Trusted);
        let trusted = policy
            .resolve(&request, &[], PermissionMode::Default, &[])
            .expect("policy resolves");
        assert_eq!(trusted.effect(), super::super::AuthorizationEffect::Allow);

        policy
            .compile_and_push(
                "Bash(cargo test)",
                PolicyEffect::Deny,
                PolicyOrigin::Project,
            )
            .expect("deny compiles");
        policy.set_workspace_trust(WorkspaceTrust::Untrusted);
        let denied = policy
            .resolve(&request, &[], PermissionMode::Default, &[])
            .expect("policy resolves");
        assert_eq!(denied.effect(), super::super::AuthorizationEffect::Deny);
    }
}
