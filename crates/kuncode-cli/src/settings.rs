//! Loads `.kuncode/settings.json` into validated runtime settings.
//!
//! Missing sections and fields inherit built-in defaults. Rules remain
//! attributable to [`RuleOrigin::ProjectSettings`], while model and compaction
//! budgets are checked against known provider capabilities before assembly.

use std::{num::NonZeroU32, path::Path};

use kuncode_agent::{
    compaction::budget::{CompactionConfig, CompactionMode},
    permission::{PermissionMode, PermissionPolicy, Rule, RuleOrigin, parse_rule},
    runner::{AgentCompactionConfig, AgentCompactionConfigError, AgentConfig},
};
use kuncode_core::providers::deepseek::{
    DEEPSEEK_V4_PRO_MODEL_ID, DeepSeekModelProfile, model_profile,
};
use serde::Deserialize;

const SETTINGS_PATH: &str = ".kuncode/settings.json";
const DEFAULT_SAFETY_MARGIN: u64 = 16_384;
const DEFAULT_SUMMARY_MAX_TOKENS: u64 = 16_384;
const DEFAULT_MAX_ITERATIONS: usize = 50;
const DEFAULT_TODO_REMINDER_INTERVAL: usize = 3;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SettingsFile {
    permissions: PermissionsSection,
    model: ModelSection,
    agent: AgentSection,
    compaction: CompactionSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PermissionsSection {
    allow: Vec<String>,
    ask: Vec<String>,
    deny: Vec<String>,
    #[serde(rename = "defaultMode")]
    default_mode: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct ModelSection {
    name: String,
    max_tokens: Option<u64>,
}

impl Default for ModelSection {
    fn default() -> Self {
        Self {
            name: DEEPSEEK_V4_PRO_MODEL_ID.to_string(),
            max_tokens: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct AgentSection {
    max_iterations: usize,
    #[serde(default = "default_todo_reminder_interval")]
    todo_reminder_interval: Option<usize>,
}

impl Default for AgentSection {
    fn default() -> Self {
        Self {
            max_iterations: DEFAULT_MAX_ITERATIONS,
            todo_reminder_interval: default_todo_reminder_interval(),
        }
    }
}

const fn default_todo_reminder_interval() -> Option<usize> {
    Some(DEFAULT_TODO_REMINDER_INTERVAL)
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
#[derive(Debug)]
pub struct ProjectSettings {
    /// Rules contributed by the file (origin = `ProjectSettings`).
    pub policy: PermissionPolicy,
    /// Default mode requested by the file, if any.
    pub default_mode: Option<PermissionMode>,
    /// Effective model identifier after file and environment precedence.
    pub(crate) model_name: String,
    /// Effective provider output budget for an ordinary turn.
    pub(crate) max_tokens: u64,
    /// Maximum tool-loop iterations allowed for one turn.
    pub(crate) max_iterations: usize,
    /// Optional cadence for reminding the model about unfinished todos.
    pub(crate) todo_reminder_interval: Option<usize>,
    /// Present only for a validated `shadow` or `enabled` compaction section.
    pub(crate) compaction: Option<ProjectCompaction>,
}

impl Default for ProjectSettings {
    fn default() -> Self {
        let max_tokens = model_profile(DEEPSEEK_V4_PRO_MODEL_ID).map_or_else(
            default_agent_max_tokens,
            DeepSeekModelProfile::default_max_tokens,
        );
        Self {
            policy: PermissionPolicy::new(),
            default_mode: None,
            model_name: DEEPSEEK_V4_PRO_MODEL_ID.to_string(),
            max_tokens,
            max_iterations: DEFAULT_MAX_ITERATIONS,
            todo_reminder_interval: default_todo_reminder_interval(),
            compaction: None,
        }
    }
}

/// Loads `.kuncode/settings.json` under `root`.
///
/// A missing file returns defaults. `DEEPSEEK_MODEL` overrides the file's model
/// name when present. Every section forms a closed schema, so misspelled fields
/// fail instead of silently selecting defaults.
///
/// # Errors
///
/// Returns [`SettingsError`] when the file cannot be read, its JSON or field
/// schema is invalid, a permission rule or mode is invalid, or an active
/// compaction section has an unsupported mode, missing context limit, invalid
/// budget, or inconsistent thresholds.
pub fn load_project_settings(root: &Path) -> Result<ProjectSettings, SettingsError> {
    let model_override = std::env::var("DEEPSEEK_MODEL").ok();
    load_project_settings_from(root, model_override.as_deref())
}

pub(crate) fn load_project_settings_from(
    root: &Path,
    model_override: Option<&str>,
) -> Result<ProjectSettings, SettingsError> {
    let path = root.join(SETTINGS_PATH);
    let file = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(SettingsError::Parse)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => SettingsFile::default(),
        Err(err) => return Err(SettingsError::Read(err)),
    };

    resolve_settings(file, model_override)
}

fn resolve_settings(
    file: SettingsFile,
    model_override: Option<&str>,
) -> Result<ProjectSettings, SettingsError> {
    let model_name = model_override.unwrap_or(&file.model.name).to_string();
    if model_name.trim().is_empty() {
        return Err(SettingsError::Model(
            "model name must not be blank".to_string(),
        ));
    }
    let profile = model_profile(&model_name);
    let max_tokens = file.model.max_tokens.unwrap_or_else(|| {
        profile.map_or_else(
            default_agent_max_tokens,
            DeepSeekModelProfile::default_max_tokens,
        )
    });
    validate_model_max_tokens(max_tokens, profile)?;
    validate_agent(&file.agent)?;

    let mut policy = PermissionPolicy::new();
    push_rules(&mut policy.allow, &file.permissions.allow)?;
    push_rules(&mut policy.ask, &file.permissions.ask)?;
    push_rules(&mut policy.deny, &file.permissions.deny)?;

    let default_mode = match file.permissions.default_mode {
        Some(name) => Some(PermissionMode::parse(&name).ok_or(SettingsError::Mode(name))?),
        None => None,
    };
    let compaction = parse_compaction(file.compaction, profile, max_tokens)?;

    Ok(ProjectSettings {
        policy,
        default_mode,
        model_name,
        max_tokens,
        max_iterations: file.agent.max_iterations,
        todo_reminder_interval: file.agent.todo_reminder_interval,
        compaction,
    })
}

fn parse_compaction(
    section: CompactionSection,
    profile: Option<DeepSeekModelProfile>,
    max_tokens: u64,
) -> Result<Option<ProjectCompaction>, SettingsError> {
    let mode = match section.mode.as_deref().unwrap_or("disabled") {
        "disabled" => return Ok(None),
        "shadow" => CompactionMode::Shadow,
        "enabled" => CompactionMode::Enabled,
        name => return Err(SettingsError::CompactionMode(name.to_string())),
    };
    let context_limit = section
        .context_limit
        .or_else(|| profile.map(DeepSeekModelProfile::default_context_limit))
        .ok_or(SettingsError::CompactionContextLimit)?;
    if let Some(profile) = profile
        && context_limit > profile.context_window_tokens()
    {
        return Err(SettingsError::Compaction(format!(
            "contextLimit {context_limit} exceeds model context window {}",
            profile.context_window_tokens()
        )));
    }
    let reserved_output = section.reserved_output.unwrap_or(max_tokens);
    if reserved_output == 0 || reserved_output > u64::from(u32::MAX) {
        return Err(SettingsError::Compaction(format!(
            "reservedOutput must be within 1..={}, got {reserved_output}",
            u32::MAX
        )));
    }
    if reserved_output != max_tokens {
        return Err(SettingsError::Compaction(format!(
            "reservedOutput {reserved_output} must equal model maxTokens {max_tokens}"
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
    if let Some(profile) = profile
        && summary_max_tokens > profile.max_output_tokens()
    {
        return Err(SettingsError::Compaction(format!(
            "summaryMaxTokens {summary_max_tokens} exceeds model output limit {}",
            profile.max_output_tokens()
        )));
    }
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

fn validate_model_max_tokens(
    max_tokens: u64,
    profile: Option<DeepSeekModelProfile>,
) -> Result<(), SettingsError> {
    if max_tokens == 0 || max_tokens > u64::from(u32::MAX) {
        return Err(SettingsError::Model(format!(
            "maxTokens must be within 1..={}, got {max_tokens}",
            u32::MAX
        )));
    }
    if let Some(profile) = profile
        && max_tokens > profile.max_output_tokens()
    {
        return Err(SettingsError::Model(format!(
            "maxTokens {max_tokens} exceeds model output limit {}",
            profile.max_output_tokens()
        )));
    }
    Ok(())
}

fn validate_agent(agent: &AgentSection) -> Result<(), SettingsError> {
    if agent.max_iterations == 0 {
        return Err(SettingsError::Agent(
            "maxIterations must be greater than zero".to_string(),
        ));
    }
    if agent.todo_reminder_interval == Some(0) {
        return Err(SettingsError::Agent(
            "todoReminderInterval must be null or greater than zero".to_string(),
        ));
    }
    Ok(())
}

fn default_agent_max_tokens() -> u64 {
    AgentConfig::default().max_tokens.unwrap_or(32_768)
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
    /// The settings file could not be read.
    Read(std::io::Error),
    /// JSON syntax or the closed settings schema was invalid.
    Parse(serde_json::Error),
    /// A permission rule and its parser diagnostic.
    Rule(String, String),
    /// The permission default mode is unsupported.
    Mode(String),
    /// A model identifier or output budget is invalid.
    Model(String),
    /// An agent-loop limit or reminder cadence is invalid.
    Agent(String),
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
            Self::Model(message) => {
                write!(f, "invalid model settings in {SETTINGS_PATH}: {message}")
            }
            Self::Agent(message) => {
                write!(f, "invalid agent settings in {SETTINGS_PATH}: {message}")
            }
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
mod tests;
