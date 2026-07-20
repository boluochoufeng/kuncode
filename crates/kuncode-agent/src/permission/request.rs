//! What a tool call is asking permission to do, and how that request is
//! resolved.

use kuncode_core::non_empty_vec::NonEmptyVec;
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::check::{PermissionCheck, hex_digest};
use super::profile::ToolProfileRevision;
const INPUT_FINGERPRINT_DOMAIN: &[u8] = b"kuncode.tool-input.v1\0";
const REQUEST_FINGERPRINT_DOMAIN: &[u8] = b"kuncode.authorization-request.v1\0";
const MAX_TOOL_IDENTITY_CHARS: usize = 256;

/// Registry-owned identity of the tool that will execute a prepared call.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ToolIdentity(String);

impl ToolIdentity {
    /// Builds a concrete tool identity.
    ///
    /// # Errors
    /// Returns an error for a blank model-facing name.
    pub fn new(value: impl Into<String>) -> Result<Self, AuthorizationRequestError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(AuthorizationRequestError::BlankToolIdentity);
        }
        if value
            .chars()
            .take(MAX_TOOL_IDENTITY_CHARS.saturating_add(1))
            .count()
            > MAX_TOOL_IDENTITY_CHARS
        {
            return Err(AuthorizationRequestError::ToolIdentityTooLong);
        }
        Ok(Self(value))
    }

    /// Returns the model-facing tool name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Canonical JSON consumed by hooks, fingerprints, and the prepared payload.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct CanonicalToolInput(serde_json::Value);

impl CanonicalToolInput {
    /// Wraps adapter-normalized JSON.
    pub fn new(value: serde_json::Value) -> Self {
        Self(value)
    }

    /// Returns the normalized JSON view without exposing the executable payload.
    pub fn as_value(&self) -> &serde_json::Value {
        &self.0
    }
}

/// Bounded, non-authoritative text safe to show in approval and event UIs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ToolDisplay {
    summary: String,
}

impl ToolDisplay {
    /// Builds a single-line summary and caps it to 512 Unicode scalar values.
    pub fn new(summary: impl AsRef<str>) -> Self {
        let flattened = summary
            .as_ref()
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect::<String>();
        let summary = flattened
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(512)
            .collect();
        Self { summary }
    }

    /// Returns the safe display summary.
    pub fn summary(&self) -> &str {
        &self.summary
    }
}

/// Stable digest used to detect input rewrite cycles.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct InputFingerprint(String);

impl InputFingerprint {
    /// Returns the lowercase SHA-256 identity.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Stable digest binding the exact policy object approved for execution.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RequestFingerprint(String);

impl RequestFingerprint {
    /// Returns the lowercase SHA-256 identity.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Final prepared permission object shared by policy, approval, and receipts.
#[derive(Clone, Debug)]
pub struct AuthorizationRequest {
    call_id: String,
    generation: u8,
    tool: ToolIdentity,
    canonical_input: CanonicalToolInput,
    checks: NonEmptyVec<PermissionCheck>,
    input_fingerprint: InputFingerprint,
    request_fingerprint: RequestFingerprint,
    display: ToolDisplay,
    profile_revision: ToolProfileRevision,
}

impl AuthorizationRequest {
    /// Binds canonical input and validated checks into one authorization object.
    ///
    /// # Errors
    /// Returns an error when canonical data cannot be encoded deterministically.
    pub fn new(
        call_id: impl Into<String>,
        generation: u8,
        tool: ToolIdentity,
        canonical_input: CanonicalToolInput,
        checks: NonEmptyVec<PermissionCheck>,
        display: ToolDisplay,
        profile_revision: ToolProfileRevision,
    ) -> Result<Self, AuthorizationRequestError> {
        let call_id = call_id.into();
        let input_fingerprint = input_fingerprint(&tool, &canonical_input)?;
        let request_fingerprint =
            request_fingerprint(&tool, &canonical_input, &checks, &profile_revision)?;
        Ok(Self {
            call_id,
            generation,
            tool,
            canonical_input,
            checks,
            input_fingerprint,
            request_fingerprint,
            display,
            profile_revision,
        })
    }

    /// Returns the provider tool-call identity retained across rewrites.
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// Returns the internal rewrite generation.
    pub const fn generation(&self) -> u8 {
        self.generation
    }

    /// Returns the registry-owned tool identity.
    pub fn tool(&self) -> &ToolIdentity {
        &self.tool
    }

    /// Returns the canonical JSON seen by authorization hooks.
    pub fn canonical_input(&self) -> &CanonicalToolInput {
        &self.canonical_input
    }

    /// Returns all profile-validated checks.
    pub fn checks(&self) -> &NonEmptyVec<PermissionCheck> {
        &self.checks
    }

    /// Returns the rewrite-cycle identity.
    pub fn input_fingerprint(&self) -> &InputFingerprint {
        &self.input_fingerprint
    }

    /// Returns the approval and receipt identity.
    pub fn request_fingerprint(&self) -> &RequestFingerprint {
        &self.request_fingerprint
    }

    /// Returns non-authoritative UI text.
    pub fn display(&self) -> &ToolDisplay {
        &self.display
    }

    /// Returns the tool-profile version covered by this request.
    pub fn profile_revision(&self) -> &ToolProfileRevision {
        &self.profile_revision
    }
}

/// Failure to build a canonical authorization request.
#[derive(Debug, Error)]
pub enum AuthorizationRequestError {
    /// Tool identities must be concrete.
    #[error("tool identity must not be blank")]
    BlankToolIdentity,
    /// Tool identities remain bounded for rules, events, and provider protocols.
    #[error("tool identity exceeds the maximum of 256 characters")]
    ToolIdentityTooLong,
    /// Canonical request data could not be encoded.
    #[error("failed to encode authorization request: {0}")]
    Encoding(#[from] serde_json::Error),
}

fn input_fingerprint(
    tool: &ToolIdentity,
    input: &CanonicalToolInput,
) -> Result<InputFingerprint, serde_json::Error> {
    #[derive(Serialize)]
    struct Payload<'a> {
        tool: &'a ToolIdentity,
        input: &'a CanonicalToolInput,
    }

    Ok(InputFingerprint(digest(
        INPUT_FINGERPRINT_DOMAIN,
        &Payload { tool, input },
    )?))
}

fn request_fingerprint(
    tool: &ToolIdentity,
    input: &CanonicalToolInput,
    checks: &NonEmptyVec<PermissionCheck>,
    profile_revision: &ToolProfileRevision,
) -> Result<RequestFingerprint, serde_json::Error> {
    #[derive(Serialize)]
    struct Payload<'a> {
        tool: &'a ToolIdentity,
        input: &'a CanonicalToolInput,
        checks: &'a NonEmptyVec<PermissionCheck>,
        profile_revision: &'a ToolProfileRevision,
    }

    Ok(RequestFingerprint(digest(
        REQUEST_FINGERPRINT_DOMAIN,
        &Payload {
            tool,
            input,
            checks,
            profile_revision,
        },
    )?))
}

fn digest<T: Serialize>(domain: &[u8], value: &T) -> Result<String, serde_json::Error> {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(serde_json::to_vec(value)?);
    Ok(hex_digest(hasher.finalize().as_slice()))
}

#[cfg(test)]
mod authorization_tests {
    use super::*;
    use crate::permission::{
        PermissionCheckSpec, PermissionNamespace, PermissionTarget, ProfileDefault,
        ToolPermissionProfile,
    };

    fn request(display: &str) -> AuthorizationRequest {
        let profile = ToolPermissionProfile::new(
            "todo_write",
            [(PermissionNamespace::TodoWrite, ProfileDefault::Allow)],
            false,
        )
        .expect("valid profile");
        let checks = profile
            .validate([PermissionCheckSpec::new(PermissionTarget::TodoWrite)])
            .expect("valid checks");
        AuthorizationRequest::new(
            "call-1",
            0,
            ToolIdentity::new("todo_write").expect("valid tool"),
            CanonicalToolInput::new(serde_json::json!({ "todos": [] })),
            checks,
            ToolDisplay::new(display),
            profile.revision().clone(),
        )
        .expect("request encodes")
    }

    #[test]
    fn display_does_not_change_request_identity() {
        let first = request("Update todo list");
        let second = request("Different UI text");
        assert_eq!(first.input_fingerprint(), second.input_fingerprint());
        assert_eq!(first.request_fingerprint(), second.request_fingerprint());
    }

    #[test]
    fn display_is_single_line_and_bounded() {
        let display = ToolDisplay::new(format!("secret\n{}", "x".repeat(600)));
        assert!(!display.summary().contains('\n'));
        assert_eq!(display.summary().chars().count(), 512);
    }

    #[test]
    fn equivalent_check_sets_have_the_same_request_identity() {
        let profile = ToolPermissionProfile::new(
            "bash",
            [(PermissionNamespace::Bash, ProfileDefault::RequireApproval)],
            true,
        )
        .expect("valid profile");
        let cargo = PermissionCheckSpec::new(PermissionTarget::Bash(
            crate::permission::CanonicalCommand::new(
                "cargo test",
                crate::permission::CommandKind::Simple,
            )
            .expect("valid command"),
        ));
        let git = PermissionCheckSpec::new(PermissionTarget::Bash(
            crate::permission::CanonicalCommand::new(
                "git status",
                crate::permission::CommandKind::Simple,
            )
            .expect("valid command"),
        ));
        let left_checks = profile
            .validate([cargo.clone(), git.clone()])
            .expect("checks validate");
        let right_checks = profile.validate([git, cargo]).expect("checks validate");
        let build = |checks| {
            AuthorizationRequest::new(
                "call-1",
                0,
                ToolIdentity::new("bash").expect("valid tool"),
                CanonicalToolInput::new(serde_json::json!({ "command": "cargo test" })),
                checks,
                ToolDisplay::new("Run shell command"),
                profile.revision().clone(),
            )
            .expect("request encodes")
        };

        assert_eq!(
            build(left_checks).request_fingerprint(),
            build(right_checks).request_fingerprint()
        );
    }
}
