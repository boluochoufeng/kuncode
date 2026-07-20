//! Order-independent policy contributions and resolved permission checks.

use kuncode_core::non_empty_vec::NonEmptyVec;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::check::{PermissionCheck, ProfileDefault, hex_digest};
use super::profile::ToolProfileRevision;

const CAUSE_ID_DOMAIN: &[u8] = b"kuncode.permission-cause.v1\0";

/// Explicit policy effect ordered from least to most restrictive.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyEffect {
    /// Executes without interactive approval.
    Allow,
    /// Requires an approval satisfaction before execution.
    RequireApproval,
    /// Prevents execution and cannot be approved around.
    Deny,
}

/// Trusted provenance of one explicit contribution.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyOrigin {
    /// Rules compiled into the product.
    Builtin,
    /// Administrator-controlled configuration.
    Managed,
    /// User-level configuration.
    User,
    /// Trusted project configuration.
    Project,
    /// Explicit command-line configuration.
    Cli,
    /// Mutable session overlay.
    Session,
    /// Current permission mode.
    Mode,
    /// Capability-checked authorization hook.
    Hook(String),
}

/// Stable identity bound into approval causes and audit records.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct PermissionCauseId(String);

impl PermissionCauseId {
    /// Derives a stable cause identity from canonical trusted data.
    pub fn derive<T: Serialize>(value: &T) -> Result<Self, serde_json::Error> {
        let mut hasher = Sha256::new();
        hasher.update(CAUSE_ID_DOMAIN);
        hasher.update(serde_json::to_vec(value)?);
        Ok(Self(hex_digest(hasher.finalize().as_slice())))
    }

    pub(crate) fn from_trusted_bytes(value: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(CAUSE_ID_DOMAIN);
        hasher.update(value);
        Self(hex_digest(hasher.finalize().as_slice()))
    }

    /// Returns the lowercase SHA-256 identity.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Bounded explanation safe for ordinary diagnostics and approval UIs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SafeExplanation(String);

impl SafeExplanation {
    /// Flattens control characters and caps text to 256 Unicode scalars.
    pub fn new(value: impl AsRef<str>) -> Self {
        let value = value
            .as_ref()
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(256)
            .collect();
        Self(value)
    }

    /// Returns the safe text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One matching rule, mode, session grant, or Hook effect.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PolicyContribution {
    effect: PolicyEffect,
    origin: PolicyOrigin,
    cause_id: PermissionCauseId,
    explanation: SafeExplanation,
}

impl PolicyContribution {
    /// Builds an explicit policy contribution.
    pub fn new(
        effect: PolicyEffect,
        origin: PolicyOrigin,
        cause_id: PermissionCauseId,
        explanation: SafeExplanation,
    ) -> Self {
        Self {
            effect,
            origin,
            cause_id,
            explanation,
        }
    }

    /// Returns the contribution's restrictive effect.
    pub const fn effect(&self) -> PolicyEffect {
        self.effect
    }

    /// Returns trusted provenance.
    pub fn origin(&self) -> &PolicyOrigin {
        &self.origin
    }

    /// Returns the stable approval/audit cause.
    pub fn cause_id(&self) -> &PermissionCauseId {
        &self.cause_id
    }

    /// Returns bounded display text.
    pub fn explanation(&self) -> &SafeExplanation {
        &self.explanation
    }
}

/// Whether a resolved effect came from explicit contributors or a profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolutionBasis {
    /// At least one explicit source matched.
    Explicit,
    /// No explicit source matched, so the trusted profile default applied.
    ProfileDefault,
}

/// Final effect and provenance for one permission check.
#[derive(Clone, Debug)]
pub struct CheckResolution {
    check: PermissionCheck,
    effect: PolicyEffect,
    basis: ResolutionBasis,
    deciding_causes: Vec<PermissionCauseId>,
    contributions: Vec<PolicyContribution>,
}

impl CheckResolution {
    /// Returns the check covered by this resolution.
    pub fn check(&self) -> &PermissionCheck {
        &self.check
    }

    /// Returns the final restrictive effect.
    pub const fn effect(&self) -> PolicyEffect {
        self.effect
    }

    /// Returns whether a profile fallback was used.
    pub fn basis(&self) -> &ResolutionBasis {
        &self.basis
    }

    /// Returns causes that must be satisfied when the effect asks.
    pub fn deciding_causes(&self) -> &[PermissionCauseId] {
        &self.deciding_causes
    }

    /// Returns every explicit matching contribution for explanation.
    pub fn contributions(&self) -> &[PolicyContribution] {
        &self.contributions
    }
}

/// Call-level effect after combining every resolved check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorizationEffect {
    /// All checks allow execution.
    Allow,
    /// At least one check needs approval and none deny.
    RequireApproval,
    /// At least one check denies execution.
    Deny,
}

/// Resolved checks and their call-level restrictive aggregate.
#[derive(Clone, Debug)]
pub struct AuthorizationResolution {
    effect: AuthorizationEffect,
    checks: NonEmptyVec<CheckResolution>,
}

impl AuthorizationResolution {
    /// Returns the call-level aggregate.
    pub const fn effect(&self) -> AuthorizationEffect {
        self.effect
    }

    /// Returns every independently resolved check.
    pub fn checks(&self) -> &NonEmptyVec<CheckResolution> {
        &self.checks
    }
}

/// Resolves one check using `Deny > RequireApproval > Allow`; an empty list
/// uses the trusted profile default instead of adding another contribution.
pub fn resolve_check(
    check: PermissionCheck,
    mut contributions: Vec<PolicyContribution>,
    profile_revision: &ToolProfileRevision,
) -> Result<CheckResolution, serde_json::Error> {
    contributions.sort_by(|left, right| {
        left.cause_id
            .cmp(&right.cause_id)
            // A malformed duplicate identity must retain the stricter effect.
            .then_with(|| right.effect.cmp(&left.effect))
            .then_with(|| left.origin.cmp(&right.origin))
    });
    contributions.dedup_by(|left, right| left.cause_id == right.cause_id);

    if contributions.is_empty() {
        let effect = match check.default_effect() {
            ProfileDefault::Allow => PolicyEffect::Allow,
            ProfileDefault::RequireApproval => PolicyEffect::RequireApproval,
        };
        let deciding_causes = if effect == PolicyEffect::RequireApproval {
            #[derive(Serialize)]
            struct DefaultCause<'a> {
                kind: &'static str,
                check_id: &'a str,
                profile_revision: &'a str,
            }
            vec![PermissionCauseId::derive(&DefaultCause {
                kind: "profile_default",
                check_id: check.id().as_str(),
                profile_revision: profile_revision.as_str(),
            })?]
        } else {
            Vec::new()
        };
        return Ok(CheckResolution {
            check,
            effect,
            basis: ResolutionBasis::ProfileDefault,
            deciding_causes,
            contributions,
        });
    }

    let effect = contributions
        .iter()
        .map(PolicyContribution::effect)
        .max()
        .unwrap_or(PolicyEffect::Deny);
    let deciding_causes = contributions
        .iter()
        .filter(|contribution| contribution.effect() == effect)
        .map(|contribution| contribution.cause_id().clone())
        .collect();
    Ok(CheckResolution {
        check,
        effect,
        basis: ResolutionBasis::Explicit,
        deciding_causes,
        contributions,
    })
}

/// Aggregates independently resolved checks into the call-level effect.
pub fn resolve_authorization(checks: NonEmptyVec<CheckResolution>) -> AuthorizationResolution {
    let effect = if checks
        .iter()
        .any(|check| check.effect() == PolicyEffect::Deny)
    {
        AuthorizationEffect::Deny
    } else if checks
        .iter()
        .any(|check| check.effect() == PolicyEffect::RequireApproval)
    {
        AuthorizationEffect::RequireApproval
    } else {
        AuthorizationEffect::Allow
    };
    AuthorizationResolution { effect, checks }
}

impl std::fmt::Display for PermissionCauseId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{PermissionCheckSpec, PermissionTarget, ToolPermissionProfile};
    use crate::permission::{PermissionNamespace, ProfileDefault};

    fn profile(default: ProfileDefault) -> ToolPermissionProfile {
        ToolPermissionProfile::new(
            "todo_write",
            [(PermissionNamespace::TodoWrite, default)],
            false,
        )
        .expect("valid profile")
    }

    fn check(profile: &ToolPermissionProfile) -> PermissionCheck {
        profile
            .validate([PermissionCheckSpec::new(PermissionTarget::TodoWrite)])
            .expect("valid check")
            .first()
            .clone()
    }

    fn contribution(effect: PolicyEffect, id: &str) -> PolicyContribution {
        PolicyContribution::new(
            effect,
            PolicyOrigin::User,
            PermissionCauseId::derive(&id).expect("cause encodes"),
            SafeExplanation::new(id),
        )
    }

    #[test]
    fn explicit_effects_form_a_restriction_lattice() {
        let profile = profile(ProfileDefault::Allow);
        let resolution = resolve_check(
            check(&profile),
            vec![
                contribution(PolicyEffect::Allow, "allow"),
                contribution(PolicyEffect::RequireApproval, "ask"),
                contribution(PolicyEffect::Deny, "deny"),
            ],
            profile.revision(),
        )
        .expect("resolution encodes");
        assert_eq!(resolution.effect(), PolicyEffect::Deny);
        assert_eq!(resolution.deciding_causes().len(), 1);
    }

    #[test]
    fn explicit_allow_suppresses_profile_default_ask() {
        let profile = profile(ProfileDefault::RequireApproval);
        let resolution = resolve_check(
            check(&profile),
            vec![contribution(PolicyEffect::Allow, "allow")],
            profile.revision(),
        )
        .expect("resolution encodes");
        assert_eq!(resolution.effect(), PolicyEffect::Allow);
        assert_eq!(resolution.basis(), &ResolutionBasis::Explicit);
    }

    #[test]
    fn contribution_order_does_not_change_resolution() {
        let profile = profile(ProfileDefault::Allow);
        let allow = contribution(PolicyEffect::Allow, "allow");
        let ask = contribution(PolicyEffect::RequireApproval, "ask");
        let left = resolve_check(
            check(&profile),
            vec![allow.clone(), ask.clone()],
            profile.revision(),
        )
        .expect("resolution encodes");
        let right = resolve_check(check(&profile), vec![ask, allow], profile.revision())
            .expect("resolution encodes");
        assert_eq!(left.effect(), right.effect());
        assert_eq!(left.deciding_causes(), right.deciding_causes());
    }

    #[test]
    fn duplicate_cause_identity_keeps_the_stricter_effect() {
        let profile = profile(ProfileDefault::Allow);
        let cause = PermissionCauseId::derive(&"same").expect("cause encodes");
        let allow = PolicyContribution::new(
            PolicyEffect::Allow,
            PolicyOrigin::User,
            cause.clone(),
            SafeExplanation::new("allow"),
        );
        let deny = PolicyContribution::new(
            PolicyEffect::Deny,
            PolicyOrigin::User,
            cause,
            SafeExplanation::new("deny"),
        );

        let resolution = resolve_check(check(&profile), vec![allow, deny], profile.revision())
            .expect("resolution encodes");

        assert_eq!(resolution.effect(), PolicyEffect::Deny);
        assert_eq!(resolution.contributions().len(), 1);
    }
}
