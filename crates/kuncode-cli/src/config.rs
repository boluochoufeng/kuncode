//! Resolves the effective permission policy and mode from layered sources.
//!
//! The CLI assembles permissions from three layers — built-in denies, the
//! project file, and command-line flags — then picks a mode by precedence.
//! Keeping that merge here, behind a pure interface, lets it be unit-tested
//! without a terminal, a model, or the filesystem: the load (I/O) stays in
//! [`crate::settings`], and the wiring stays in `main`.

use kuncode_agent::permission::{PermissionMode, PermissionPolicy, Rule, RuleOrigin, parse_rule};

use crate::settings::ProjectSettings;

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
    pub policy: PermissionPolicy,
    pub mode: PermissionMode,
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
) -> Result<ResolvedPermissions, ConfigError> {
    let mut policy = PermissionPolicy::builtin();
    policy.append(project.policy);
    push_flag_rules(&mut policy.allow, flags.allow)?;
    push_flag_rules(&mut policy.ask, flags.ask)?;
    push_flag_rules(&mut policy.deny, flags.deny)?;

    let mode = match flags.mode {
        Some(name) => {
            PermissionMode::parse(name).ok_or_else(|| ConfigError::Mode(name.to_string()))?
        }
        None => project.default_mode.unwrap_or_default(),
    };

    Ok(ResolvedPermissions { policy, mode })
}

/// Parses each `--allow`/`--ask`/`--deny` value (origin = `CliFlag`) and appends
/// it onto the matching policy list.
fn push_flag_rules(target: &mut Vec<Rule>, rules: &[String]) -> Result<(), ConfigError> {
    for rule in rules {
        let parsed = parse_rule(rule, RuleOrigin::CliFlag)
            .map_err(|err| ConfigError::Rule(rule.clone(), err.to_string()))?;
        target.extend(parsed);
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
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rule(rule, err) => write!(f, "invalid rule `{rule}`: {err}"),
            Self::Mode(mode) => write!(f, "invalid --mode `{mode}`"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut policy = PermissionPolicy::new();
        policy
            .deny
            .extend(parse_rule(rule, RuleOrigin::ProjectSettings).expect("valid test rule"));
        ProjectSettings {
            policy,
            default_mode: None,
            compaction: None,
        }
    }

    #[test]
    fn merges_builtin_project_and_cli_layers() {
        // Built-in denies must survive; project adds one deny, CLI adds one of each.
        let builtin_deny = PermissionPolicy::builtin().deny.len();
        let allow = vec!["Bash(cargo *)".to_string()];
        let deny = vec!["Bash(curl *)".to_string()];

        let resolved =
            resolve_permissions(project_with_deny("Bash(rm *)"), &flags(&allow, &deny, None))
                .expect("layers merge");

        assert_eq!(resolved.policy.deny.len(), builtin_deny + 1 + 1);
        assert_eq!(resolved.policy.allow.len(), 1);
        assert_eq!(resolved.mode, PermissionMode::Default);
    }

    fn project_with_mode(mode: PermissionMode) -> ProjectSettings {
        ProjectSettings {
            policy: PermissionPolicy::new(),
            default_mode: Some(mode),
            compaction: None,
        }
    }

    #[test]
    fn mode_flag_overrides_project_default() {
        let resolved = resolve_permissions(
            project_with_mode(PermissionMode::AcceptEdits),
            &flags(&[], &[], Some("bypass")),
        )
        .expect("mode resolves");

        assert_eq!(resolved.mode, PermissionMode::BypassPermissions);
    }

    #[test]
    fn mode_falls_back_to_project_then_default() {
        let resolved = resolve_permissions(
            project_with_mode(PermissionMode::AcceptEdits),
            &flags(&[], &[], None),
        )
        .expect("uses project mode");
        assert_eq!(resolved.mode, PermissionMode::AcceptEdits);

        let resolved = resolve_permissions(ProjectSettings::default(), &flags(&[], &[], None))
            .expect("uses default");
        assert_eq!(resolved.mode, PermissionMode::Default);
    }

    #[test]
    fn bad_cli_rule_is_an_error() {
        let allow = vec!["Bash(".to_string()];
        assert!(matches!(
            resolve_permissions(ProjectSettings::default(), &flags(&allow, &[], None)),
            Err(ConfigError::Rule(_, _))
        ));
    }

    #[test]
    fn unknown_mode_is_an_error() {
        assert!(matches!(
            resolve_permissions(ProjectSettings::default(), &flags(&[], &[], Some("turbo"))),
            Err(ConfigError::Mode(_))
        ));
    }
}
