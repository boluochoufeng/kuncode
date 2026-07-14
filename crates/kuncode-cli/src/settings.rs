//! Loads `.kuncode/settings.json` into permission and automatic-compaction policy.
//!
//! The file shape mirrors Claude Code's `permissions` block. Rules are parsed
//! with [`RuleOrigin::ProjectSettings`] so denials remain explainable. Active
//! compaction settings are validated here before they can reach runtime
//! assembly; absent or disabled settings deliberately produce no runtime policy.

use std::{num::NonZeroU32, path::Path};

use kuncode_agent::{
    compaction::budget::{CompactionConfig, CompactionMode},
    permission::{PermissionMode, PermissionPolicy, Rule, RuleOrigin, parse_rule},
    runner::{AgentCompactionConfig, AgentCompactionConfigError, AgentConfig},
};
use serde::Deserialize;

const SETTINGS_PATH: &str = ".kuncode/settings.json";
const DEFAULT_SAFETY_MARGIN: u64 = 4_096;
const DEFAULT_SUMMARY_MAX_TOKENS: u64 = 4_096;

#[derive(Debug, Default, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    permissions: PermissionsSection,
    compaction: Option<CompactionSection>,
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

// Keep budget and threshold keys closed so a typo cannot silently select a
// default and change when lossy compaction runs.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompactionSection {
    mode: Option<String>,
    context_limit: Option<u64>,
    reserved_output: Option<u64>,
    safety_margin: Option<u64>,
    summary_max_tokens: Option<u64>,
    target_ratio: Option<f64>,
    soft_threshold: Option<f64>,
    hard_threshold: Option<f64>,
    recent_ratio: Option<f64>,
}

/// Validated settings for an active project compaction policy.
///
/// Only `shadow` and `enabled` sections reach this type. An absent section, an
/// explicitly disabled section, or a section whose mode is omitted maps to
/// [`None`] before runtime assembly.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProjectCompaction {
    policy: CompactionConfig,
    summary_max_tokens: NonZeroU32,
}

impl ProjectCompaction {
    /// Returns the output allowance excluded from the usable input window.
    ///
    /// Runtime assembly also installs this value as the provider request output
    /// limit, keeping request construction and compaction accounting aligned.
    pub(crate) const fn reserved_output(self) -> u64 {
        self.policy.reserved_output()
    }

    /// Binds the validated project policy to the concrete provider model.
    ///
    /// # Errors
    ///
    /// Returns [`AgentCompactionConfigError::BlankModelId`] when `model_id` is
    /// empty or whitespace. The parsed non-zero 32-bit summary budget already
    /// satisfies the runtime range requirement.
    pub(crate) fn into_runtime(
        self,
        model_id: &str,
    ) -> Result<AgentCompactionConfig, AgentCompactionConfigError> {
        AgentCompactionConfig::new(
            self.policy,
            model_id,
            u64::from(self.summary_max_tokens.get()),
        )
    }
}

/// Runtime settings read from the project file.
#[derive(Debug, Default)]
pub struct ProjectSettings {
    /// Rules contributed by the file (origin = `ProjectSettings`).
    pub policy: PermissionPolicy,
    /// Default mode requested by the file, if any.
    pub default_mode: Option<PermissionMode>,
    /// Present only for a validated `shadow` or `enabled` compaction section.
    pub(crate) compaction: Option<ProjectCompaction>,
}

/// Loads `.kuncode/settings.json` under `root`.
///
/// A missing file returns defaults. Compaction fields form a closed schema, so
/// unknown budget or threshold names fail instead of silently using defaults.
///
/// # Errors
///
/// Returns [`SettingsError`] when the file cannot be read, its JSON or field
/// schema is invalid, a permission rule or mode is invalid, or an active
/// compaction section has an unsupported mode, missing context limit, invalid
/// budget, or inconsistent thresholds.
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
    let compaction = parse_compaction(file.compaction)?;

    Ok(ProjectSettings {
        policy,
        default_mode,
        compaction,
    })
}

fn parse_compaction(
    section: Option<CompactionSection>,
) -> Result<Option<ProjectCompaction>, SettingsError> {
    let Some(section) = section else {
        return Ok(None);
    };
    let mode = match section.mode.as_deref().unwrap_or("disabled") {
        "disabled" => return Ok(None),
        "shadow" => CompactionMode::Shadow,
        "enabled" => CompactionMode::Enabled,
        name => return Err(SettingsError::CompactionMode(name.to_string())),
    };
    let context_limit = section
        .context_limit
        .ok_or(SettingsError::CompactionContextLimit)?;
    let reserved_output = match section.reserved_output {
        Some(value) => value,
        None => AgentConfig::default().max_tokens.ok_or_else(|| {
            SettingsError::Compaction("agent default max_tokens is unavailable".to_string())
        })?,
    };
    if reserved_output == 0 || reserved_output > u64::from(u32::MAX) {
        return Err(SettingsError::Compaction(format!(
            "reservedOutput must be within 1..={}, got {reserved_output}",
            u32::MAX
        )));
    }
    let policy = CompactionConfig::new(
        mode,
        context_limit,
        reserved_output,
        section.safety_margin.unwrap_or(DEFAULT_SAFETY_MARGIN),
    )
    .map_err(|error| SettingsError::Compaction(error.to_string()))?;
    let policy = policy
        .with_ratios(
            section.target_ratio.unwrap_or(policy.target_ratio()),
            section.soft_threshold.unwrap_or(policy.soft_threshold()),
            section.hard_threshold.unwrap_or(policy.hard_threshold()),
            section.recent_ratio.unwrap_or(policy.recent_ratio()),
        )
        .map_err(|error| SettingsError::Compaction(error.to_string()))?;
    let summary_max_tokens = section
        .summary_max_tokens
        .unwrap_or(DEFAULT_SUMMARY_MAX_TOKENS);
    let summary_max_tokens = u32::try_from(summary_max_tokens)
        .ok()
        .and_then(NonZeroU32::new)
        .ok_or_else(|| {
            SettingsError::Compaction(format!(
                "summaryMaxTokens must be within 1..={}, got {summary_max_tokens}",
                u32::MAX
            ))
        })?;

    Ok(Some(ProjectCompaction {
        policy,
        summary_max_tokens,
    }))
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
    /// The compaction mode is neither `disabled`, `shadow`, nor `enabled`.
    CompactionMode(String),
    /// Active compaction cannot derive a usable window without `contextLimit`.
    CompactionContextLimit,
    /// A compaction budget, window, or threshold invariant was rejected.
    Compaction(String),
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(err) => write!(f, "failed to read {SETTINGS_PATH}: {err}"),
            Self::Parse(err) => write!(f, "failed to parse {SETTINGS_PATH}: {err}"),
            Self::Rule(rule, err) => write!(f, "invalid rule `{rule}` in {SETTINGS_PATH}: {err}"),
            Self::Mode(mode) => write!(f, "invalid defaultMode `{mode}` in {SETTINGS_PATH}"),
            Self::CompactionMode(mode) => {
                write!(f, "invalid compaction mode `{mode}` in {SETTINGS_PATH}")
            }
            Self::CompactionContextLimit => write!(
                f,
                "compaction contextLimit is required for shadow or enabled mode in {SETTINGS_PATH}"
            ),
            Self::Compaction(message) => {
                write!(
                    f,
                    "invalid compaction settings in {SETTINGS_PATH}: {message}"
                )
            }
        }
    }
}

impl std::error::Error for SettingsError {}

#[cfg(test)]
#[path = "settings/tests.rs"]
mod tests;
