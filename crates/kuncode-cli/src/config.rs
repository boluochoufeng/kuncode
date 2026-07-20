//! Resolves the effective permission policy and mode from layered sources.
//!
//! The CLI assembles permissions from three layers — built-in denies, the
//! project file, and command-line flags — then picks a mode by precedence.
//! Keeping that merge here, behind a pure interface, lets it be unit-tested
//! without a terminal, a model, or the filesystem: the load (I/O) stays in
//! [`crate::settings`], and the wiring stays in `main`.

use kuncode_agent::permission::{
    CanonicalPath, PermissionMode, PolicyEffect, PolicyOrigin, PolicySet, WorkspaceTrust,
};

use crate::settings::{ProjectSettings, ProjectTrust};

/// Command-line permission inputs, borrowed from the parsed `Cli`.
///
/// Holds the raw, unparsed rule strings so that flag-parsing failures surface
/// through [`resolve_permissions`] and stay on the tested path — rather than
/// being parsed ad hoc at the call site.
#[derive(Debug, Default)]
pub struct PermissionFlags<'a> {
    pub allow: &'a [String],
    pub ask: &'a [String],
    pub deny: &'a [String],
    pub mode: Option<&'a str>,
}

/// The effective permission configuration after merging every layer.
#[derive(Debug)]
pub struct ResolvedPermissions {
    pub policy: PolicySet,
    pub mode: PermissionMode,
    pub ignored_project_relaxations: usize,
}

/// Merges `built-in ∪ project ∪ CLI` rules and resolves the mode precedence.
///
/// Layering is ordered built-in denies first, then project-file rules, then
/// CLI flags; a later layer can only *add* rules, so built-in denies are never
/// dropped. Mode precedence is `CLI flag > project file > Default`.
///
/// # Errors
/// Returns [`ConfigError::Rule`] for an unparseable `--allow`/`--ask`/`--deny`
/// value, and [`ConfigError::Mode`] for an unknown `--mode`.
pub fn resolve_permissions(
    project: ProjectSettings,
    flags: &PermissionFlags<'_>,
    workspace_root: CanonicalPath,
) -> Result<ResolvedPermissions, ConfigError> {
    let mut policy = PolicySet::builtin(workspace_root)
        .map_err(|error| ConfigError::Policy(error.to_string()))?;
    let mut ignored_project_relaxations = 0usize;
    if let Some(project_policy) = project.policy {
        if project_policy.workspace_root() != policy.workspace_root() {
            return Err(ConfigError::Policy(
                "project policy uses a different workspace root".to_string(),
            ));
        }
        for rule in project_policy.rules() {
            if project.trust == ProjectTrust::Untrusted && rule.effect() == PolicyEffect::Allow {
                ignored_project_relaxations = ignored_project_relaxations.saturating_add(1);
            } else {
                policy.push(rule.clone());
            }
        }
    }
    push_flag_rules(&mut policy, flags.allow, PolicyEffect::Allow)?;
    push_flag_rules(&mut policy, flags.ask, PolicyEffect::RequireApproval)?;
    push_flag_rules(&mut policy, flags.deny, PolicyEffect::Deny)?;

    let mode = match flags.mode {
        Some(name) => {
            PermissionMode::parse(name).ok_or_else(|| ConfigError::Mode(name.to_string()))?
        }
        None => match project.default_mode {
            Some(mode) if project.trust == ProjectTrust::Trusted || mode_does_not_relax(mode) => {
                mode
            }
            Some(_) => {
                ignored_project_relaxations = ignored_project_relaxations.saturating_add(1);
                PermissionMode::Default
            }
            None => PermissionMode::Default,
        },
    };
    policy.set_workspace_trust(match project.trust {
        ProjectTrust::Untrusted => WorkspaceTrust::Untrusted,
        ProjectTrust::Trusted => WorkspaceTrust::Trusted,
    });

    Ok(ResolvedPermissions {
        policy,
        mode,
        ignored_project_relaxations,
    })
}

fn mode_does_not_relax(mode: PermissionMode) -> bool {
    matches!(
        mode,
        PermissionMode::Default | PermissionMode::Plan | PermissionMode::DontAsk
    )
}

/// Parses each `--allow`/`--ask`/`--deny` value (origin = `CliFlag`) and appends
/// it onto the matching policy list.
fn push_flag_rules(
    policy: &mut PolicySet,
    rules: &[String],
    effect: PolicyEffect,
) -> Result<(), ConfigError> {
    for rule in rules {
        policy
            .compile_and_push(rule, effect, PolicyOrigin::Cli)
            .map_err(|error| ConfigError::Rule(rule.clone(), error.to_string()))?;
    }
    Ok(())
}

/// Errors raised while resolving permission flags.
#[derive(Debug)]
pub enum ConfigError {
    /// A `--allow`/`--ask`/`--deny` value failed to parse (rule, reason).
    Rule(String, String),
    /// `--mode` named a mode that does not exist.
    Mode(String),
    /// Typed policies could not be anchored or merged safely.
    Policy(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rule(rule, err) => write!(f, "invalid rule `{rule}`: {err}"),
            Self::Mode(mode) => write!(f, "invalid --mode `{mode}`"),
            Self::Policy(error) => write!(f, "invalid permission policy: {error}"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn root() -> CanonicalPath {
        CanonicalPath::from_absolute(Path::new("/workspace")).expect("absolute test root")
    }

    fn flags<'a>(
        allow: &'a [String],
        deny: &'a [String],
        mode: Option<&'a str>,
    ) -> PermissionFlags<'a> {
        PermissionFlags {
            allow,
            ask: &[],
            deny,
            mode,
        }
    }

    fn project_with_deny(rule: &str) -> ProjectSettings {
        let mut policy = PolicySet::new(root());
        policy
            .compile_and_push(rule, PolicyEffect::Deny, PolicyOrigin::Project)
            .expect("valid test rule");
        ProjectSettings {
            policy: Some(policy),
            default_mode: None,
            compaction: None,
            ..ProjectSettings::default()
        }
    }

    #[test]
    fn merges_builtin_project_and_cli_layers() {
        // Built-in denies must survive; project adds one deny, CLI adds one of each.
        let builtin_deny = PolicySet::builtin(root())
            .expect("built-ins compile")
            .rules()
            .iter()
            .filter(|rule| rule.effect() == PolicyEffect::Deny)
            .count();
        let allow = vec!["Bash(cargo *)".to_string()];
        let deny = vec!["Bash(curl *)".to_string()];

        let resolved = resolve_permissions(
            project_with_deny("Bash(rm *)"),
            &flags(&allow, &deny, None),
            root(),
        )
        .expect("layers merge");

        assert_eq!(
            resolved
                .policy
                .rules()
                .iter()
                .filter(|rule| rule.effect() == PolicyEffect::Deny)
                .count(),
            builtin_deny + 1 + 1
        );
        assert_eq!(
            resolved
                .policy
                .rules()
                .iter()
                .filter(|rule| rule.effect() == PolicyEffect::Allow)
                .count(),
            1
        );
        assert_eq!(resolved.mode, PermissionMode::Default);
    }

    fn project_with_mode(mode: PermissionMode) -> ProjectSettings {
        ProjectSettings {
            policy: Some(PolicySet::new(root())),
            default_mode: Some(mode),
            trust: ProjectTrust::Trusted,
            compaction: None,
            ..ProjectSettings::default()
        }
    }

    #[test]
    fn mode_flag_overrides_project_default() {
        let resolved = resolve_permissions(
            project_with_mode(PermissionMode::AcceptEdits),
            &flags(&[], &[], Some("bypass")),
            root(),
        )
        .expect("mode resolves");

        assert_eq!(resolved.mode, PermissionMode::BypassPermissions);
    }

    #[test]
    fn mode_falls_back_to_project_then_default() {
        let resolved = resolve_permissions(
            project_with_mode(PermissionMode::AcceptEdits),
            &flags(&[], &[], None),
            root(),
        )
        .expect("uses project mode");
        assert_eq!(resolved.mode, PermissionMode::AcceptEdits);

        let resolved =
            resolve_permissions(ProjectSettings::default(), &flags(&[], &[], None), root())
                .expect("uses default");
        assert_eq!(resolved.mode, PermissionMode::Default);
    }

    #[test]
    fn bad_cli_rule_is_an_error() {
        let allow = vec!["Bash(".to_string()];
        assert!(matches!(
            resolve_permissions(
                ProjectSettings::default(),
                &flags(&allow, &[], None),
                root()
            ),
            Err(ConfigError::Rule(_, _))
        ));
    }

    #[test]
    fn unknown_mode_is_an_error() {
        assert!(matches!(
            resolve_permissions(
                ProjectSettings::default(),
                &flags(&[], &[], Some("turbo")),
                root()
            ),
            Err(ConfigError::Mode(_))
        ));
    }

    #[test]
    fn untrusted_project_cannot_activate_allow_or_bypass() {
        let mut project_policy = PolicySet::new(root());
        project_policy
            .compile_and_push("Bash(cargo *)", PolicyEffect::Allow, PolicyOrigin::Project)
            .expect("rule compiles");
        project_policy
            .compile_and_push("Bash(curl *)", PolicyEffect::Deny, PolicyOrigin::Project)
            .expect("rule compiles");
        let project = ProjectSettings {
            policy: Some(project_policy),
            default_mode: Some(PermissionMode::BypassPermissions),
            trust: ProjectTrust::Untrusted,
            ..ProjectSettings::default()
        };

        let resolved = resolve_permissions(project, &flags(&[], &[], None), root())
            .expect("untrusted tightening rules remain valid");

        assert_eq!(resolved.mode, PermissionMode::Default);
        assert_eq!(resolved.ignored_project_relaxations, 2);
        assert_eq!(resolved.policy.workspace_trust(), WorkspaceTrust::Untrusted);
        assert!(resolved.policy.rules().iter().all(|rule| {
            rule.origin() != &PolicyOrigin::Project || rule.effect() != PolicyEffect::Allow
        }));
        assert!(resolved.policy.rules().iter().any(|rule| {
            rule.origin() == &PolicyOrigin::Project && rule.effect() == PolicyEffect::Deny
        }));
    }

    #[test]
    fn trusted_project_can_activate_reviewed_relaxations() {
        let mut project_policy = PolicySet::new(root());
        project_policy
            .compile_and_push("Bash(cargo *)", PolicyEffect::Allow, PolicyOrigin::Project)
            .expect("rule compiles");
        let project = ProjectSettings {
            policy: Some(project_policy),
            default_mode: Some(PermissionMode::AcceptEdits),
            trust: ProjectTrust::Trusted,
            ..ProjectSettings::default()
        };

        let resolved = resolve_permissions(project, &flags(&[], &[], None), root())
            .expect("trusted settings merge");

        assert_eq!(resolved.mode, PermissionMode::AcceptEdits);
        assert_eq!(resolved.ignored_project_relaxations, 0);
        assert_eq!(resolved.policy.workspace_trust(), WorkspaceTrust::Trusted);
        assert!(resolved.policy.rules().iter().any(|rule| {
            rule.origin() == &PolicyOrigin::Project && rule.effect() == PolicyEffect::Allow
        }));
    }
}
