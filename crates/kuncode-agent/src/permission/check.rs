//! Concrete permission checks emitted by prepared tool invocations.

use std::fmt::Write as _;

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::target::PermissionTarget;

const CHECK_ID_DOMAIN: &[u8] = b"kuncode.permission-check.v1\0";

/// Trusted fallback used only when no explicit policy contribution matches.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileDefault {
    /// Execute without prompting when no explicit rule applies.
    Allow,
    /// Require an approval when no explicit rule applies.
    RequireApproval,
}

/// A target proposed by a tool before registry-profile validation.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct PermissionCheckSpec {
    target: PermissionTarget,
}

impl PermissionCheckSpec {
    /// Creates a check proposal for one canonical target.
    pub fn new(target: PermissionTarget) -> Self {
        Self { target }
    }

    /// Returns the proposed target.
    pub fn target(&self) -> &PermissionTarget {
        &self.target
    }
}

/// Stable identity of one normalized check within an authorization request.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct PermissionCheckId(String);

impl PermissionCheckId {
    pub(crate) fn for_target(target: &PermissionTarget) -> Result<Self, serde_json::Error> {
        let mut hasher = Sha256::new();
        hasher.update(CHECK_ID_DOMAIN);
        hasher.update(serde_json::to_vec(target)?);
        Ok(Self(hex_digest(hasher.finalize().as_slice())))
    }

    /// Returns the lowercase SHA-256 identity.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Profile-validated check used by policy, approval, and execution receipts.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct PermissionCheck {
    id: PermissionCheckId,
    target: PermissionTarget,
    default_effect: ProfileDefault,
}

impl PermissionCheck {
    pub(crate) fn new(
        target: PermissionTarget,
        default_effect: ProfileDefault,
    ) -> Result<Self, serde_json::Error> {
        let id = PermissionCheckId::for_target(&target)?;
        Ok(Self {
            id,
            target,
            default_effect,
        })
    }

    /// Returns the stable request-local identity.
    pub fn id(&self) -> &PermissionCheckId {
        &self.id
    }

    /// Returns the canonical resource authorized by this check.
    pub fn target(&self) -> &PermissionTarget {
        &self.target
    }

    /// Returns the trusted fallback selected by the registry profile.
    pub const fn default_effect(&self) -> ProfileDefault {
        self.default_effect
    }
}

pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing into a String cannot fail; ignore the infallible fmt result
        // instead of introducing a panic-only branch in library code.
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::target::{CanonicalCommand, CommandKind};

    #[test]
    fn check_ids_bind_the_canonical_target() {
        let first = PermissionCheck::new(
            PermissionTarget::Bash(
                CanonicalCommand::new("cargo test", CommandKind::Simple).expect("valid command"),
            ),
            ProfileDefault::RequireApproval,
        )
        .expect("target serializes");
        let second = PermissionCheck::new(
            PermissionTarget::Bash(
                CanonicalCommand::new("cargo test", CommandKind::Simple).expect("valid command"),
            ),
            ProfileDefault::Allow,
        )
        .expect("target serializes");

        assert_eq!(first.id(), second.id());
        assert_eq!(first.id().as_str().len(), 64);
        assert_ne!(first.default_effect(), second.default_effect());
    }
}
