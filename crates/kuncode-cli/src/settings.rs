//! Loads `.kuncode/settings.json` into a permission policy and initial mode.
//!
//! The file shape mirrors Claude Code's `permissions` block. Rules are parsed
//! with [`RuleOrigin::ProjectSettings`] so denials remain explainable.

use std::path::Path;

use kuncode_agent::permission::{PermissionMode, PermissionPolicy, Rule, RuleOrigin, parse_rule};
use serde::Deserialize;

use crate::hook::{HookConfig, HookPoint};

const SETTINGS_PATH: &str = ".kuncode/settings.json";

#[derive(Debug, Default, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    permissions: PermissionsSection,
    #[serde(default)]
    hooks: HooksSection,
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

/// The `hooks` block: one command list per trigger point. Every field defaults
/// to empty, so a file that predates hooks (or omits the section) still loads —
/// same non-breaking stance as [`PermissionsSection`].
#[derive(Debug, Default, Deserialize)]
struct HooksSection {
    #[serde(default, rename = "UserPromptSubmit")]
    user_prompt_submit: Vec<HookEntry>,
    #[serde(default, rename = "PreToolUse")]
    pre_tool_use: Vec<HookEntry>,
    #[serde(default, rename = "PostToolUse")]
    post_tool_use: Vec<HookEntry>,
    #[serde(default, rename = "Stop")]
    stop: Vec<HookEntry>,
}

/// One `{ command, matcher?, failClosed? }` entry under a trigger point.
#[derive(Debug, Deserialize)]
struct HookEntry {
    command: String,
    #[serde(default)]
    matcher: Option<String>,
    #[serde(default, rename = "failClosed")]
    fail_closed: Option<bool>,
}

/// Permission settings read from the project file.
#[derive(Debug, Default)]
pub struct ProjectSettings {
    /// Rules contributed by the file (origin = `ProjectSettings`).
    pub policy: PermissionPolicy,
    /// Default mode requested by the file, if any.
    pub default_mode: Option<PermissionMode>,
    /// Validated hook configurations, in `UserPromptSubmit → PreToolUse →
    /// PostToolUse → Stop` order (registration order within each point).
    pub hooks: Vec<HookConfig>,
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

    let hooks = build_hooks(file.hooks)?;

    Ok(ProjectSettings {
        policy,
        default_mode,
        hooks,
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

/// Flattens the per-point lists into validated [`HookConfig`]s, preserving the
/// `UserPromptSubmit → PreToolUse → PostToolUse → Stop` order.
fn build_hooks(section: HooksSection) -> Result<Vec<HookConfig>, SettingsError> {
    let mut configs = Vec::new();
    push_hooks(
        &mut configs,
        HookPoint::UserPromptSubmit,
        section.user_prompt_submit,
    )?;
    push_hooks(&mut configs, HookPoint::PreToolUse, section.pre_tool_use)?;
    push_hooks(&mut configs, HookPoint::PostToolUse, section.post_tool_use)?;
    push_hooks(&mut configs, HookPoint::Stop, section.stop)?;
    Ok(configs)
}

/// Validates and lowers one point's entries.
///
/// Resolves `failClosed` against the per-point default — `true` for
/// `PreToolUse` (a failed guard must not silently let a call through), `false`
/// elsewhere. `PostToolUse` has no veto outcome, so any `failClosed` there is a
/// configuration error rather than a silently-ignored field.
fn push_hooks(
    target: &mut Vec<HookConfig>,
    point: HookPoint,
    entries: Vec<HookEntry>,
) -> Result<(), SettingsError> {
    for entry in entries {
        if point == HookPoint::PostToolUse && entry.fail_closed.is_some() {
            return Err(SettingsError::Hook(format!(
                "`failClosed` is not valid on PostToolUse (it has no veto outcome): {}",
                entry.command
            )));
        }
        let fail_closed = entry.fail_closed.unwrap_or(point == HookPoint::PreToolUse);
        target.push(HookConfig {
            point,
            command: entry.command,
            matcher: entry.matcher,
            fail_closed,
        });
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
    Hook(String),
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(err) => write!(f, "failed to read {SETTINGS_PATH}: {err}"),
            Self::Parse(err) => write!(f, "failed to parse {SETTINGS_PATH}: {err}"),
            Self::Rule(rule, err) => write!(f, "invalid rule `{rule}` in {SETTINGS_PATH}: {err}"),
            Self::Mode(mode) => write!(f, "invalid defaultMode `{mode}` in {SETTINGS_PATH}"),
            Self::Hook(err) => write!(f, "invalid hook in {SETTINGS_PATH}: {err}"),
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

    #[test]
    fn loads_hooks_in_point_order_with_defaults() {
        let dir = unique_dir("hooks");
        fs::write(
            dir.join(".kuncode/settings.json"),
            r#"{ "hooks": {
                "UserPromptSubmit": [{ "command": "redact" }],
                "PreToolUse": [{ "matcher": "bash", "command": "guard" }],
                "Stop": [{ "command": "check", "failClosed": true }]
            } }"#,
        )
        .unwrap();

        let loaded = load_project_settings(&dir).expect("loads");
        let hooks = &loaded.hooks;
        assert_eq!(hooks.len(), 3);

        // Order is UserPromptSubmit → PreToolUse → PostToolUse → Stop.
        assert_eq!(hooks[0].point, HookPoint::UserPromptSubmit);
        assert!(!hooks[0].fail_closed); // non-PreToolUse defaults fail-open
        assert_eq!(hooks[1].point, HookPoint::PreToolUse);
        assert_eq!(hooks[1].matcher.as_deref(), Some("bash"));
        assert!(hooks[1].fail_closed); // PreToolUse defaults fail-closed
        assert_eq!(hooks[2].point, HookPoint::Stop);
        assert!(hooks[2].fail_closed); // explicit override honored
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fail_closed_on_post_tool_use_is_an_error() {
        let dir = unique_dir("hookbad");
        fs::write(
            dir.join(".kuncode/settings.json"),
            r#"{ "hooks": {
                "PostToolUse": [{ "command": "fmt", "failClosed": true }]
            } }"#,
        )
        .unwrap();
        assert!(matches!(
            load_project_settings(&dir),
            Err(SettingsError::Hook(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }
}
