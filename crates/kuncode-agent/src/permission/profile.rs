//! Registry-owned constraints for permission checks emitted by each tool.

use std::collections::{BTreeMap, BTreeSet};

use kuncode_core::non_empty_vec::NonEmptyVec;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::check::{PermissionCheck, PermissionCheckSpec, ProfileDefault, hex_digest};
use super::target::{CanonicalPath, PathSelector, PermissionNamespace, PermissionTarget};

const PROFILE_REVISION_DOMAIN: &[u8] = b"kuncode.tool-permission-profile.v1\0";
const MAX_PERMISSION_CHECKS: usize = 64;
const MAX_TOOL_NAME_CHARS: usize = 256;

/// Stable revision included in request fingerprints and authorization context.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ToolProfileRevision(String);

impl ToolProfileRevision {
    /// Returns the lowercase SHA-256 revision.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Trusted registry metadata constraining one tool's permission surface.
#[derive(Clone, Debug)]
pub struct ToolPermissionProfile {
    tool: String,
    defaults: BTreeMap<PermissionNamespace, ProfileDefault>,
    allow_multiple: bool,
    path_root: Option<CanonicalPath>,
    revision: ToolProfileRevision,
}

impl ToolPermissionProfile {
    /// Builds a profile from explicit namespace defaults.
    ///
    /// # Errors
    /// Returns an error for a blank tool name or an empty schema set.
    pub fn new(
        tool: impl Into<String>,
        defaults: impl IntoIterator<Item = (PermissionNamespace, ProfileDefault)>,
        allow_multiple: bool,
    ) -> Result<Self, ToolProfileError> {
        let tool = tool.into();
        if tool.trim().is_empty() {
            return Err(ToolProfileError::BlankToolName);
        }
        if tool
            .chars()
            .take(MAX_TOOL_NAME_CHARS.saturating_add(1))
            .count()
            > MAX_TOOL_NAME_CHARS
        {
            return Err(ToolProfileError::ToolNameTooLong);
        }
        let mut compiled_defaults = BTreeMap::new();
        for (namespace, default) in defaults {
            if let Some(previous) = compiled_defaults.insert(namespace, default)
                && previous != default
            {
                return Err(ToolProfileError::ConflictingSchema { tool, namespace });
            }
        }
        let defaults = compiled_defaults;
        if defaults.is_empty() {
            return Err(ToolProfileError::EmptySchemas { tool });
        }
        let revision = profile_revision(&tool, &defaults, allow_multiple, None)?;
        Ok(Self {
            tool,
            defaults,
            allow_multiple,
            path_root: None,
            revision,
        })
    }

    /// Restricts every Read/Edit selector to one canonical workspace root.
    ///
    /// # Errors
    /// Returns an error when the profile has no path namespace or its revised
    /// canonical metadata cannot be encoded.
    pub fn constrain_paths_to(mut self, root: CanonicalPath) -> Result<Self, ToolProfileError> {
        if !self.defaults.contains_key(&PermissionNamespace::Read)
            && !self.defaults.contains_key(&PermissionNamespace::Edit)
        {
            return Err(ToolProfileError::PathConstraintWithoutSchema { tool: self.tool });
        }
        self.path_root = Some(root);
        self.revision = profile_revision(
            &self.tool,
            &self.defaults,
            self.allow_multiple,
            self.path_root.as_ref(),
        )?;
        Ok(self)
    }

    /// Creates the conservative fallback profile used for custom local tools.
    ///
    /// The exact tool identity remains visible to rules and defaults to an
    /// approval rather than trusting a tool-defined safety classification.
    pub fn exact_tool(tool: impl Into<String>) -> Result<Self, ToolProfileError> {
        Self::new(
            tool,
            [(
                PermissionNamespace::ExactTool,
                ProfileDefault::RequireApproval,
            )],
            false,
        )
    }

    /// Returns the model-facing tool identity bound to this profile.
    pub fn tool(&self) -> &str {
        &self.tool
    }

    /// Returns the revision bound into authorization requests.
    pub fn revision(&self) -> &ToolProfileRevision {
        &self.revision
    }

    /// Validates, sorts, and deduplicates tool-emitted check proposals.
    ///
    /// # Errors
    /// Returns an error for empty checks, forbidden namespaces, mismatched
    /// exact-tool selectors, or multiple checks when the profile forbids them.
    pub fn validate(
        &self,
        specs: impl IntoIterator<Item = PermissionCheckSpec>,
    ) -> Result<NonEmptyVec<PermissionCheck>, ToolProfileError> {
        let mut specs = specs.into_iter().collect::<Vec<_>>();
        if specs.is_empty() {
            return Err(ToolProfileError::EmptyChecks {
                tool: self.tool.clone(),
            });
        }
        specs.sort();
        specs.dedup();
        if specs.len() > MAX_PERMISSION_CHECKS {
            return Err(ToolProfileError::TooManyChecks {
                tool: self.tool.clone(),
                actual: specs.len(),
                maximum: MAX_PERMISSION_CHECKS,
            });
        }
        if !self.allow_multiple && specs.len() != 1 {
            return Err(ToolProfileError::MultipleChecksForbidden {
                tool: self.tool.clone(),
                actual: specs.len(),
            });
        }

        let mut checks = Vec::with_capacity(specs.len());
        for spec in specs {
            let namespace = spec.target().namespace();
            let default_effect = self.defaults.get(&namespace).copied().ok_or_else(|| {
                ToolProfileError::ForbiddenNamespace {
                    tool: self.tool.clone(),
                    namespace,
                }
            })?;
            self.validate_selector(spec.target())?;
            if let PermissionTarget::ExactTool(name) = spec.target()
                && name != &self.tool
            {
                return Err(ToolProfileError::ExactToolMismatch {
                    registered: self.tool.clone(),
                    emitted: name.clone(),
                });
            }
            checks.push(PermissionCheck::new(spec.target().clone(), default_effect)?);
        }

        NonEmptyVec::try_from(checks).map_err(|_| ToolProfileError::EmptyChecks {
            tool: self.tool.clone(),
        })
    }

    /// Returns the namespaces this profile permits.
    pub fn namespaces(&self) -> BTreeSet<PermissionNamespace> {
        self.defaults.keys().copied().collect()
    }

    pub(crate) fn path_root(&self) -> Option<&CanonicalPath> {
        self.path_root.as_ref()
    }

    fn validate_selector(&self, target: &PermissionTarget) -> Result<(), ToolProfileError> {
        if let Some(root) = &self.path_root {
            let selector = match target {
                PermissionTarget::Read(selector) | PermissionTarget::Edit(selector) => selector,
                _ => return Ok(()),
            };
            let inside = match selector {
                PathSelector::Exact { path } => path_is_inside(path, root),
                PathSelector::Pattern {
                    root: pattern_root, ..
                } => pattern_root == root,
            };
            if !inside {
                return Err(ToolProfileError::PathOutsideRoot {
                    tool: self.tool.clone(),
                });
            }
        }
        target
            .validate()
            .map_err(|source| ToolProfileError::InvalidSelector {
                tool: self.tool.clone(),
                source,
            })
    }
}

/// Invalid tool registration or emitted permission surface.
#[derive(Debug, Error)]
pub enum ToolProfileError {
    /// Tool identities must be concrete.
    #[error("tool profile name must not be blank")]
    BlankToolName,
    /// Tool identities remain bounded for registry and rule matching.
    #[error("tool profile name exceeds the maximum of 256 characters")]
    ToolNameTooLong,
    /// Every tool needs at least one trusted namespace schema.
    #[error("tool `{tool}` profile has no permission schemas")]
    EmptySchemas {
        /// Registered tool name.
        tool: String,
    },
    /// One namespace cannot have conflicting trusted defaults.
    #[error("tool `{tool}` profile repeats {namespace:?} with conflicting defaults")]
    ConflictingSchema {
        /// Registered tool name.
        tool: String,
        /// Conflicting namespace.
        namespace: PermissionNamespace,
    },
    /// Every prepared call needs at least one check.
    #[error("tool `{tool}` prepared a call without permission checks")]
    EmptyChecks {
        /// Registered tool name.
        tool: String,
    },
    /// The adapter emitted a namespace not granted by its profile.
    #[error("tool `{tool}` emitted forbidden permission namespace {namespace:?}")]
    ForbiddenNamespace {
        /// Registered tool name.
        tool: String,
        /// Rejected namespace.
        namespace: PermissionNamespace,
    },
    /// A single-target profile emitted several targets.
    #[error("tool `{tool}` emitted {actual} checks but its profile allows exactly one")]
    MultipleChecksForbidden {
        /// Registered tool name.
        tool: String,
        /// Number of unique checks emitted.
        actual: usize,
    },
    /// Multi-check profiles remain bounded against adversarial inputs.
    #[error("tool `{tool}` emitted {actual} checks, exceeding the maximum {maximum}")]
    TooManyChecks {
        /// Registered tool name.
        tool: String,
        /// Number of unique checks emitted.
        actual: usize,
        /// Trusted product limit.
        maximum: usize,
    },
    /// A fallback adapter attempted to authorize a different tool identity.
    #[error("exact-tool profile `{registered}` cannot emit target `{emitted}`")]
    ExactToolMismatch {
        /// Registry-owned identity.
        registered: String,
        /// Tool-emitted identity.
        emitted: String,
    },
    /// A root constraint is meaningful only for a path namespace.
    #[error("tool `{tool}` cannot constrain paths without a Read or Edit schema")]
    PathConstraintWithoutSchema {
        /// Registered tool name.
        tool: String,
    },
    /// A built-in workspace tool emitted a target outside its registered root.
    #[error("tool `{tool}` emitted a path outside its registered workspace root")]
    PathOutsideRoot {
        /// Registered tool name.
        tool: String,
    },
    /// Direct enum construction cannot bypass selector constructors.
    #[error("tool `{tool}` emitted an invalid permission selector: {source}")]
    InvalidSelector {
        /// Registered tool name.
        tool: String,
        /// Constructor invariant that was bypassed.
        #[source]
        source: super::PermissionTargetError,
    },
    /// Canonical profile or check data could not be encoded.
    #[error("failed to encode tool permission profile: {0}")]
    Encoding(#[from] serde_json::Error),
}

fn profile_revision(
    tool: &str,
    defaults: &BTreeMap<PermissionNamespace, ProfileDefault>,
    allow_multiple: bool,
    path_root: Option<&CanonicalPath>,
) -> Result<ToolProfileRevision, serde_json::Error> {
    #[derive(Serialize)]
    struct RevisionPayload<'a> {
        tool: &'a str,
        defaults: &'a BTreeMap<PermissionNamespace, ProfileDefault>,
        allow_multiple: bool,
        path_root: Option<&'a CanonicalPath>,
    }

    let mut hasher = Sha256::new();
    hasher.update(PROFILE_REVISION_DOMAIN);
    hasher.update(serde_json::to_vec(&RevisionPayload {
        tool,
        defaults,
        allow_multiple,
        path_root,
    })?);
    Ok(ToolProfileRevision(hex_digest(
        hasher.finalize().as_slice(),
    )))
}

fn path_is_inside(path: &CanonicalPath, root: &CanonicalPath) -> bool {
    path.as_str() == root.as_str()
        || path
            .as_str()
            .strip_prefix(root.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::permission::target::{CanonicalCommand, CommandKind};

    #[test]
    fn profile_rejects_unregistered_namespace() {
        let profile = ToolPermissionProfile::exact_tool("custom").expect("valid profile");
        let spec = PermissionCheckSpec::new(PermissionTarget::Bash(
            CanonicalCommand::new("cargo test", CommandKind::Simple).expect("valid command"),
        ));

        assert!(matches!(
            profile.validate([spec]),
            Err(ToolProfileError::ForbiddenNamespace {
                namespace: PermissionNamespace::Bash,
                ..
            })
        ));
    }

    #[test]
    fn validation_is_order_independent_and_deduplicates() {
        let profile = ToolPermissionProfile::new(
            "bash",
            [(PermissionNamespace::Bash, ProfileDefault::RequireApproval)],
            true,
        )
        .expect("valid profile");
        let cargo = PermissionCheckSpec::new(PermissionTarget::Bash(
            CanonicalCommand::new("cargo test", CommandKind::Simple).expect("valid command"),
        ));
        let git = PermissionCheckSpec::new(PermissionTarget::Bash(
            CanonicalCommand::new("git status", CommandKind::Simple).expect("valid command"),
        ));

        let left = profile
            .validate([cargo.clone(), git.clone(), cargo.clone()])
            .expect("checks validate");
        let right = profile.validate([git, cargo]).expect("checks validate");

        assert_eq!(left, right);
        assert_eq!(left.len(), 2);
    }

    #[test]
    fn exact_fallback_cannot_claim_another_tool() {
        let profile = ToolPermissionProfile::exact_tool("custom").expect("valid profile");
        let spec = PermissionCheckSpec::new(
            PermissionTarget::exact_tool("other").expect("valid exact target"),
        );
        assert!(matches!(
            profile.validate([spec]),
            Err(ToolProfileError::ExactToolMismatch { .. })
        ));
    }

    #[test]
    fn conflicting_namespace_defaults_are_rejected() {
        assert!(matches!(
            ToolPermissionProfile::new(
                "reader",
                [
                    (PermissionNamespace::Read, ProfileDefault::Allow),
                    (PermissionNamespace::Read, ProfileDefault::RequireApproval,),
                ],
                false,
            ),
            Err(ToolProfileError::ConflictingSchema {
                namespace: PermissionNamespace::Read,
                ..
            })
        ));
    }

    #[test]
    fn workspace_profile_rejects_out_of_root_paths() {
        let root = CanonicalPath::from_absolute(Path::new("/workspace")).expect("valid root");
        let profile = ToolPermissionProfile::new(
            "reader",
            [(PermissionNamespace::Read, ProfileDefault::Allow)],
            false,
        )
        .expect("valid profile")
        .constrain_paths_to(root)
        .expect("path schema");
        let outside =
            CanonicalPath::from_absolute(Path::new("/outside/secret")).expect("valid path");
        let spec = PermissionCheckSpec::new(PermissionTarget::Read(PathSelector::exact(outside)));

        assert!(matches!(
            profile.validate([spec]),
            Err(ToolProfileError::PathOutsideRoot { .. })
        ));
    }

    #[test]
    fn profile_requires_a_non_empty_check_set() {
        let profile = ToolPermissionProfile::exact_tool("custom").expect("valid profile");

        assert!(matches!(
            profile.validate([]),
            Err(ToolProfileError::EmptyChecks { .. })
        ));
    }

    #[test]
    fn single_target_profile_rejects_multiple_checks() {
        let profile = ToolPermissionProfile::new(
            "bash",
            [(PermissionNamespace::Bash, ProfileDefault::RequireApproval)],
            false,
        )
        .expect("valid profile");
        let specs = ["cargo test", "git status"].map(|command| {
            PermissionCheckSpec::new(PermissionTarget::Bash(
                CanonicalCommand::new(command, CommandKind::Simple).expect("valid command"),
            ))
        });

        assert!(matches!(
            profile.validate(specs),
            Err(ToolProfileError::MultipleChecksForbidden { actual: 2, .. })
        ));
    }

    #[test]
    fn multi_target_profile_caps_unique_checks() {
        let profile = ToolPermissionProfile::new(
            "bash",
            [(PermissionNamespace::Bash, ProfileDefault::RequireApproval)],
            true,
        )
        .expect("valid profile");
        let specs = (0..=MAX_PERMISSION_CHECKS).map(|index| {
            PermissionCheckSpec::new(PermissionTarget::Bash(
                CanonicalCommand::new(format!("echo {index}"), CommandKind::Simple)
                    .expect("valid command"),
            ))
        });

        assert!(matches!(
            profile.validate(specs),
            Err(ToolProfileError::TooManyChecks {
                actual: 65,
                maximum: 64,
                ..
            })
        ));
    }

    #[test]
    fn direct_enum_construction_cannot_bypass_selector_validation() {
        let profile = ToolPermissionProfile::new(
            "agent",
            [(PermissionNamespace::Agent, ProfileDefault::RequireApproval)],
            false,
        )
        .expect("valid profile");
        let spec = PermissionCheckSpec::new(PermissionTarget::Agent("   ".to_string()));

        assert!(matches!(
            profile.validate([spec]),
            Err(ToolProfileError::InvalidSelector { .. })
        ));
    }
}
