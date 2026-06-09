//! Loads `.kuncode/settings.json` into a permission policy and initial mode.
//!
//! The file shape mirrors Claude Code's `permissions` block. Rules are parsed
//! with [`RuleOrigin::ProjectSettings`] so denials remain explainable.

use std::path::Path;

use kuncode_agent::permission::{PermissionMode, PermissionPolicy, Rule, RuleOrigin, parse_rule};
use serde::Deserialize;

const SETTINGS_PATH: &str = ".kuncode/settings.json";

#[derive(Debug, Default, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    permissions: PermissionsSection,
}

#[derive(Debug, Default, Deserialize)]
struct PermissionsSection {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    ask: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
    #[serde(default, rename = "defaultMode")]
    default_mode: Option<String>,
}

/// Permission settings read from the project file.
#[derive(Debug, Default)]
pub struct ProjectSettings {
    /// Rules contributed by the file (origin = `ProjectSettings`).
    pub policy: PermissionPolicy,
    /// Default mode requested by the file, if any.
    pub default_mode: Option<PermissionMode>,
}

/// Loads `.kuncode/settings.json` under `root`.
///
/// A missing file is not an error (returns defaults). A malformed file, a bad
/// rule, or an unknown mode *is* an error, so the user learns their config is
/// broken instead of silently running with less protection than they think.
pub fn load_project_settings(root: &Path) -> Result<ProjectSettings, SettingsError> {
    let path = root.join(SETTINGS_PATH);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProjectSettings::default());
        }
        Err(err) => return Err(SettingsError::Read(err)),
    };

    let file: SettingsFile = serde_json::from_str(&raw).map_err(SettingsError::Parse)?;

    let mut policy = PermissionPolicy::new();
    push_rules(&mut policy.allow, &file.permissions.allow)?;
    push_rules(&mut policy.ask, &file.permissions.ask)?;
    push_rules(&mut policy.deny, &file.permissions.deny)?;

    let default_mode = match file.permissions.default_mode {
        Some(name) => Some(PermissionMode::parse(&name).ok_or(SettingsError::Mode(name))?),
        None => None,
    };

    Ok(ProjectSettings {
        policy,
        default_mode,
    })
}

fn push_rules(target: &mut Vec<Rule>, rules: &[String]) -> Result<(), SettingsError> {
    for rule in rules {
        let parsed = parse_rule(rule, RuleOrigin::ProjectSettings)
            .map_err(|err| SettingsError::Rule(rule.clone(), err.to_string()))?;
        target.extend(parsed);
    }
    Ok(())
}

/// Errors raised while loading project settings.
#[derive(Debug)]
pub enum SettingsError {
    Read(std::io::Error),
    Parse(serde_json::Error),
    Rule(String, String),
    Mode(String),
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(err) => write!(f, "failed to read {SETTINGS_PATH}: {err}"),
            Self::Parse(err) => write!(f, "failed to parse {SETTINGS_PATH}: {err}"),
            Self::Rule(rule, err) => write!(f, "invalid rule `{rule}` in {SETTINGS_PATH}: {err}"),
            Self::Mode(mode) => write!(f, "invalid defaultMode `{mode}` in {SETTINGS_PATH}"),
        }
    }
}

impl std::error::Error for SettingsError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};

    fn unique_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kuncode-settings-{}-{tag}", std::process::id()));
        fs::create_dir_all(dir.join(".kuncode")).expect("temp dir");
        dir
    }

    #[test]
    fn missing_file_is_default_not_error() {
        let dir = std::env::temp_dir().join(format!("kuncode-absent-{}", std::process::id()));
        let loaded = load_project_settings(&dir).expect("a missing file is fine");
        assert!(loaded.policy.deny.is_empty());
        assert!(loaded.default_mode.is_none());
    }

    #[test]
    fn loads_rules_and_mode() {
        let dir = unique_dir("ok");
        fs::write(
            dir.join(".kuncode/settings.json"),
            r#"{ "permissions": {
                "allow": ["Read", "Bash(cargo *)"],
                "deny": ["Bash(curl *)"],
                "defaultMode": "acceptEdits"
            } }"#,
        )
        .unwrap();

        let loaded = load_project_settings(&dir).expect("loads");
        // `Read` expands to read_file + glob (2) plus `Bash(cargo *)` (1) = 3.
        assert_eq!(loaded.policy.allow.len(), 3);
        assert_eq!(loaded.policy.deny.len(), 1);
        assert_eq!(loaded.default_mode, Some(PermissionMode::AcceptEdits));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_json_is_an_error() {
        let dir = unique_dir("bad");
        fs::write(dir.join(".kuncode/settings.json"), "{ not json").unwrap();
        assert!(load_project_settings(&dir).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_rule_is_an_error() {
        let dir = unique_dir("badrule");
        fs::write(
            dir.join(".kuncode/settings.json"),
            r#"{ "permissions": { "deny": ["Bash("] } }"#,
        )
        .unwrap();
        assert!(matches!(
            load_project_settings(&dir),
            Err(SettingsError::Rule(_, _))
        ));
        let _ = fs::remove_dir_all(&dir);
    }
}
