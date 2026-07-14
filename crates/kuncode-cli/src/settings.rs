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
mod tests {
    use super::*;
    use kuncode_agent::compaction::budget::CompactionMode;
    use std::{fs, path::PathBuf};

    fn unique_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kuncode-settings-{}-{tag}", std::process::id()));
        fs::create_dir_all(dir.join(".kuncode")).expect("temp dir");
        dir
    }

    fn load_json(tag: &str, json: &str) -> Result<ProjectSettings, SettingsError> {
        let dir = unique_dir(tag);
        fs::write(dir.join(".kuncode/settings.json"), json).expect("write settings");
        let result = load_project_settings_from(&dir, None);
        let _ = fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn missing_file_is_default_not_error() {
        let dir = std::env::temp_dir().join(format!("kuncode-absent-{}", std::process::id()));

        let loaded = load_project_settings_from(&dir, None).expect("a missing file is fine");

        assert!(loaded.policy.deny.is_empty());
        assert!(loaded.default_mode.is_none());
        assert!(loaded.compaction.is_none());
        assert_eq!(loaded.model_name, "deepseek-v4-pro");
        assert_eq!(loaded.max_tokens, 65_536);
        assert_eq!(loaded.max_iterations, 50);
        assert_eq!(loaded.todo_reminder_interval, Some(3));
    }

    #[test]
    fn loads_rules_and_mode() {
        let loaded = load_json(
            "permissions-ok",
            r#"{ "permissions": {
                "allow": ["Read", "Bash(cargo *)"],
                "deny": ["Bash(curl *)"],
                "defaultMode": "acceptEdits"
            } }"#,
        )
        .expect("loads");

        assert_eq!(loaded.policy.allow.len(), 3);
        assert_eq!(loaded.policy.deny.len(), 1);
        assert_eq!(loaded.default_mode, Some(PermissionMode::AcceptEdits));
    }

    #[test]
    fn absent_compaction_is_not_installed() {
        let loaded = load_json("compaction-absent", "{}").expect("loads");

        assert!(loaded.compaction.is_none());
    }

    #[test]
    fn compaction_mode_defaults_to_disabled() {
        let loaded = load_json(
            "compaction-mode-default",
            r#"{ "compaction": { "contextLimit": 131072 } }"#,
        )
        .expect("loads");

        assert!(loaded.compaction.is_none());
    }

    #[test]
    fn disabled_compaction_is_not_installed() {
        let loaded = load_json(
            "compaction-disabled",
            r#"{ "compaction": { "mode": "disabled" } }"#,
        )
        .expect("loads");

        assert!(loaded.compaction.is_none());
    }

    #[test]
    fn shadow_compaction_uses_runtime_defaults() {
        let loaded = load_json(
            "compaction-defaults",
            r#"{ "compaction": { "mode": "shadow" } }"#,
        )
        .expect("loads");

        let compaction = loaded.compaction.expect("compaction enabled");
        assert_eq!(compaction.policy.mode(), CompactionMode::Shadow);
        assert_eq!(compaction.policy.context_limit(), 400_000);
        assert_eq!(compaction.policy.reserved_output(), 65_536);
        assert_eq!(compaction.policy.safety_margin(), 16_384);
        assert_eq!(compaction.policy.target_ratio(), 0.50);
        assert_eq!(compaction.policy.soft_threshold(), 0.75);
        assert_eq!(compaction.policy.hard_threshold(), 0.90);
        assert_eq!(compaction.policy.recent_ratio(), 0.10);
        assert_eq!(compaction.summary_max_tokens.get(), 16_384);
    }

    #[test]
    fn partial_settings_override_only_selected_defaults() {
        let loaded = load_json(
            "partial-defaults",
            r#"{
                "model": { "maxTokens": 32768 },
                "agent": { "maxIterations": 12, "todoReminderInterval": null },
                "compaction": {
                    "mode": "shadow",
                    "reservedOutput": 32768
                }
            }"#,
        )
        .expect("loads");

        assert_eq!(loaded.model_name, "deepseek-v4-pro");
        assert_eq!(loaded.max_tokens, 32_768);
        assert_eq!(loaded.max_iterations, 12);
        assert_eq!(loaded.todo_reminder_interval, None);
        let compaction = loaded.compaction.expect("compaction enabled");
        assert_eq!(compaction.policy.context_limit(), 400_000);
        assert_eq!(compaction.policy.reserved_output(), 32_768);
        assert_eq!(compaction.policy.safety_margin(), 16_384);
        assert_eq!(compaction.summary_max_tokens.get(), 16_384);
    }

    #[test]
    fn enabled_compaction_loads_all_camel_case_fields() {
        let loaded = load_json(
            "compaction-full",
            r#"{
            "model": { "maxTokens": 8192 },
            "compaction": {
                "mode": "enabled",
                "contextLimit": 65536,
                "reservedOutput": 8192,
                "safetyMargin": 2048,
                "summaryMaxTokens": 1024,
                "targetRatio": 0.4,
                "softThreshold": 0.7,
                "hardThreshold": 0.85,
                "recentRatio": 0.2
            } }"#,
        )
        .expect("loads");

        let compaction = loaded.compaction.expect("compaction enabled");
        assert_eq!(compaction.policy.mode(), CompactionMode::Enabled);
        assert_eq!(compaction.policy.context_limit(), 65_536);
        assert_eq!(compaction.policy.reserved_output(), 8_192);
        assert_eq!(compaction.policy.safety_margin(), 2_048);
        assert_eq!(compaction.policy.target_ratio(), 0.4);
        assert_eq!(compaction.policy.soft_threshold(), 0.7);
        assert_eq!(compaction.policy.hard_threshold(), 0.85);
        assert_eq!(compaction.policy.recent_ratio(), 0.2);
        assert_eq!(compaction.summary_max_tokens.get(), 1_024);
    }

    #[test]
    fn invalid_compaction_mode_is_an_error() {
        let result = load_json(
            "compaction-mode-invalid",
            r#"{ "compaction": { "mode": "automatic", "contextLimit": 65536 } }"#,
        );

        assert!(matches!(result, Err(SettingsError::CompactionMode(_))));
    }

    #[test]
    fn unknown_compaction_field_is_an_error() {
        let result = load_json(
            "compaction-unknown-field",
            r#"{ "compaction": { "mode": "enabled", "contextLimit": 65536, "softRatio": 0.7 } }"#,
        );

        assert!(matches!(result, Err(SettingsError::Parse(_))));
    }

    #[test]
    fn unknown_model_active_compaction_requires_context_limit() {
        let result = load_json(
            "compaction-context-missing",
            r#"{
                "model": { "name": "custom-model" },
                "compaction": { "mode": "enabled" }
            }"#,
        );

        assert!(matches!(result, Err(SettingsError::CompactionContextLimit)));
    }

    #[test]
    fn model_environment_override_drives_known_defaults() {
        let dir = unique_dir("model-env-override");
        fs::write(
            dir.join(".kuncode/settings.json"),
            r#"{
                "model": { "name": "custom-model", "maxTokens": 8192 },
                "compaction": { "mode": "enabled" }
            }"#,
        )
        .expect("write settings");

        let loaded = load_project_settings_from(&dir, Some("deepseek-v4-flash"))
            .expect("environment override selects a known model");
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(loaded.model_name, "deepseek-v4-flash");
        assert_eq!(loaded.max_tokens, 8_192);
        assert_eq!(
            loaded
                .compaction
                .expect("compaction enabled")
                .policy
                .context_limit(),
            400_000
        );
    }

    #[test]
    fn compaction_reserved_output_must_match_model_max_tokens() {
        let result = load_json(
            "compaction-output-mismatch",
            r#"{
                "model": { "maxTokens": 32768 },
                "compaction": {
                    "mode": "enabled",
                    "reservedOutput": 65536
                }
            }"#,
        );

        assert!(matches!(result, Err(SettingsError::Compaction(_))));
    }

    #[test]
    fn known_model_limits_reject_oversized_operational_budgets() {
        let context = load_json(
            "model-context-oversized",
            r#"{
                "compaction": { "mode": "enabled", "contextLimit": 1000001 }
            }"#,
        );
        let output = load_json(
            "model-output-oversized",
            r#"{ "model": { "maxTokens": 384001 } }"#,
        );

        assert!(matches!(context, Err(SettingsError::Compaction(_))));
        assert!(matches!(output, Err(SettingsError::Model(_))));
    }

    #[test]
    fn unknown_top_level_and_section_fields_are_errors() {
        let top_level = load_json("unknown-top-level", r#"{ "modle": {} }"#);
        let model = load_json("unknown-model-field", r#"{ "model": { "id": "x" } }"#);
        let agent = load_json("unknown-agent-field", r#"{ "agent": { "iterations": 3 } }"#);

        assert!(matches!(top_level, Err(SettingsError::Parse(_))));
        assert!(matches!(model, Err(SettingsError::Parse(_))));
        assert!(matches!(agent, Err(SettingsError::Parse(_))));
    }

    #[test]
    fn invalid_agent_defaults_are_errors() {
        let iterations = load_json(
            "agent-iterations-zero",
            r#"{ "agent": { "maxIterations": 0 } }"#,
        );
        let reminder = load_json(
            "agent-reminder-zero",
            r#"{ "agent": { "todoReminderInterval": 0 } }"#,
        );

        assert!(matches!(iterations, Err(SettingsError::Agent(_))));
        assert!(matches!(reminder, Err(SettingsError::Agent(_))));
    }

    #[test]
    fn invalid_compaction_window_is_an_error() {
        let result = load_json(
            "compaction-window-invalid",
            r#"{ "compaction": {
                "mode": "enabled",
                "contextLimit": 8192,
                "reservedOutput": 4096,
                "safetyMargin": 4096
            } }"#,
        );

        assert!(matches!(result, Err(SettingsError::Compaction(_))));
    }

    #[test]
    fn invalid_reserved_output_provider_range_is_an_error() {
        let zero = load_json(
            "compaction-output-zero",
            r#"{ "compaction": {
                "mode": "enabled",
                "contextLimit": 65536,
                "reservedOutput": 0
            } }"#,
        );
        let oversized = load_json(
            "compaction-output-oversized",
            r#"{ "compaction": {
                "mode": "enabled",
                "contextLimit": 8589934592,
                "reservedOutput": 4294967296
            } }"#,
        );

        assert!(matches!(zero, Err(SettingsError::Compaction(_))));
        assert!(matches!(oversized, Err(SettingsError::Compaction(_))));
    }

    #[test]
    fn invalid_compaction_ratios_are_an_error() {
        let result = load_json(
            "compaction-ratios-invalid",
            r#"{ "compaction": {
                "mode": "shadow",
                "contextLimit": 65536,
                "targetRatio": 0.8,
                "softThreshold": 0.7
            } }"#,
        );

        assert!(matches!(result, Err(SettingsError::Compaction(_))));
    }

    #[test]
    fn invalid_summary_budget_is_an_error() {
        let result = load_json(
            "compaction-summary-invalid",
            r#"{ "compaction": {
                "mode": "enabled",
                "contextLimit": 65536,
                "summaryMaxTokens": 0
            } }"#,
        );

        assert!(matches!(result, Err(SettingsError::Compaction(_))));
    }

    #[test]
    fn oversized_summary_budget_is_an_error() {
        let result = load_json(
            "compaction-summary-oversized",
            r#"{ "compaction": {
                "mode": "enabled",
                "contextLimit": 65536,
                "summaryMaxTokens": 4294967296
            } }"#,
        );

        assert!(matches!(result, Err(SettingsError::Compaction(_))));
    }

    #[test]
    fn malformed_json_is_an_error() {
        let result = load_json("malformed", "{ not json");

        assert!(matches!(result, Err(SettingsError::Parse(_))));
    }

    #[test]
    fn bad_rule_is_an_error() {
        let result = load_json(
            "permission-rule-invalid",
            r#"{ "permissions": { "deny": ["Bash("] } }"#,
        );

        assert!(matches!(result, Err(SettingsError::Rule(_, _))));
    }
}
