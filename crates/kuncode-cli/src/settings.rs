//! Loads `.kuncode/settings.json` into validated runtime settings.
//!
//! Missing sections and fields inherit built-in defaults. Rules remain
//! attributable to project policy, while model and compaction
//! budgets are checked against known provider capabilities before assembly.

use std::{collections::BTreeMap, num::NonZeroU32, path::Path};

use kuncode_agent::{
    compaction::budget::{CompactionConfig, CompactionMode},
    permission::{CanonicalPath, PermissionMode, PolicyEffect, PolicyOrigin, PolicySet},
    runner::{AgentCompactionConfig, AgentCompactionConfigError, AgentConfig},
};
use kuncode_core::providers::deepseek::{
    DEEPSEEK_V4_PRO_MODEL_ID, DeepSeekModelProfile, model_profile,
};
use serde::Deserialize;

const SETTINGS_PATH: &str = ".kuncode/settings.json";
const USER_PROVIDERS_PATH: &str = ".kuncode/providers.json";
const DEFAULT_SAFETY_MARGIN: u64 = 16_384;
const DEFAULT_SUMMARY_MAX_TOKENS: u64 = 16_384;
const DEFAULT_MAX_ITERATIONS: usize = 50;
const DEFAULT_TODO_REMINDER_INTERVAL: usize = 3;
const DEFAULT_LOG_LEVEL: &str = "info";

/// User-controlled trust assigned outside the repository settings file.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ProjectTrust {
    /// Repository configuration may only tighten permissions.
    #[default]
    Untrusted,
    /// Repository Allow rules and relaxing default modes may be activated.
    Trusted,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SettingsFile {
    permissions: PermissionsSection,
    model: ModelSection,
    agent: AgentSection,
    compaction: CompactionSection,
    logging: LoggingSection,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LoggingSection {
    level: String,
}

impl Default for LoggingSection {
    fn default() -> Self {
        Self {
            level: DEFAULT_LOG_LEVEL.to_string(),
        }
    }
}

/// File-log settings needed before the rest of the runtime is assembled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LoggingSettings {
    pub(crate) level: String,
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

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct ModelSection {
    profile: Option<String>,
    provider: Option<ProviderKind>,
    name: Option<String>,
    base_url: Option<String>,
    api_key_env: Option<String>,
    headers: BTreeMap<String, String>,
    max_tokens: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct UserProvidersFile {
    default_profile: Option<String>,
    profiles: BTreeMap<String, UserProviderProfile>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UserProviderProfile {
    provider: ProviderKind,
    #[serde(default)]
    base_url: Option<String>,
    api_key_env: String,
    model: String,
    max_tokens: u64,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

/// Wire protocol selected for model requests.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
pub(crate) enum ProviderKind {
    /// Native DeepSeek behavior and environment defaults.
    #[default]
    #[serde(rename = "deepseek")]
    DeepSeek,
    /// Official OpenAI Chat Completions protocol and endpoint.
    #[serde(rename = "openai")]
    OpenAi,
}

#[derive(Clone, Debug)]
struct ResolvedProviderProfile {
    provider: ProviderKind,
    base_url: Option<String>,
    api_key_env: String,
    model: String,
    max_tokens: u64,
    headers: BTreeMap<String, String>,
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
    /// Typed rules contributed by the file and anchored to its workspace.
    pub policy: Option<PolicySet>,
    /// Default mode requested by the file, if any.
    pub default_mode: Option<PermissionMode>,
    /// Trust comes from CLI/user state, never from the project file itself.
    pub(crate) trust: ProjectTrust,
    /// Effective provider protocol.
    pub(crate) provider: ProviderKind,
    /// Custom service root or full Chat Completions endpoint.
    pub(crate) base_url: Option<String>,
    /// Environment variable holding the provider credential.
    pub(crate) api_key_env: String,
    /// User-controlled headers attached to provider requests.
    pub(crate) headers: BTreeMap<String, String>,
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
            policy: None,
            default_mode: None,
            trust: ProjectTrust::Untrusted,
            provider: ProviderKind::DeepSeek,
            base_url: None,
            api_key_env: "DEEPSEEK_API_KEY".to_string(),
            headers: BTreeMap::new(),
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
/// A missing file returns defaults. `KUNCODE_MODEL` overrides the file's model
/// name; `DEEPSEEK_MODEL` remains a backward-compatible fallback. Every section
/// forms a closed schema, so misspelled fields fail instead of silently selecting
/// defaults.
///
/// # Errors
///
/// Returns [`SettingsError`] when the file cannot be read, its JSON or field
/// schema is invalid, a permission rule or mode is invalid, or an active
/// compaction section has an unsupported mode, missing context limit, invalid
/// budget, or inconsistent thresholds.
pub(crate) fn load_project_settings(
    root: &Path,
    trust: ProjectTrust,
    profile_override: Option<&str>,
    model_override: Option<&str>,
) -> Result<ProjectSettings, SettingsError> {
    let environment_model = std::env::var("KUNCODE_MODEL")
        .ok()
        .or_else(|| std::env::var("DEEPSEEK_MODEL").ok());
    let model_override = model_override.or(environment_model.as_deref());
    let user = match std::env::home_dir() {
        Some(home) => read_user_providers(&home)?,
        None => UserProvidersFile::default(),
    };
    let file = read_settings_file(root)?;
    resolve_settings(file, user, profile_override, model_override, root, trust)
}

#[cfg(test)]
pub(crate) fn load_project_settings_from(
    root: &Path,
    model_override: Option<&str>,
    trust: ProjectTrust,
) -> Result<ProjectSettings, SettingsError> {
    let file = read_settings_file(root)?;
    resolve_settings(
        file,
        UserProvidersFile::default(),
        None,
        model_override,
        root,
        trust,
    )
}

/// Loads only the bootstrap settings required to initialize file logging.
///
/// This deliberately skips semantic validation of unrelated runtime sections:
/// logging must be available to record those later validation failures.
pub(crate) fn load_logging_settings(root: &Path) -> Result<LoggingSettings, SettingsError> {
    let logging = read_settings_file(root)?.logging;
    let level = normalized_log_level(&logging.level).ok_or_else(|| {
        SettingsError::Logging(format!(
            "level must be one of off, error, warn, info, debug, or trace; got a value with {} characters",
            logging.level.chars().count()
        ))
    })?;
    Ok(LoggingSettings {
        level: level.to_string(),
    })
}

fn read_settings_file(root: &Path) -> Result<SettingsFile, SettingsError> {
    let path = root.join(SETTINGS_PATH);
    let file = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(SettingsError::Parse)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => SettingsFile::default(),
        Err(err) => return Err(SettingsError::Read(err)),
    };
    Ok(file)
}

fn read_user_providers(home: &Path) -> Result<UserProvidersFile, SettingsError> {
    let path = home.join(USER_PROVIDERS_PATH);
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(SettingsError::UserParse),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(UserProvidersFile::default())
        }
        Err(error) => Err(SettingsError::UserRead(error)),
    }
}

fn resolve_settings(
    file: SettingsFile,
    user: UserProvidersFile,
    profile_override: Option<&str>,
    model_override: Option<&str>,
    root: &Path,
    trust: ProjectTrust,
) -> Result<ProjectSettings, SettingsError> {
    let mut provider = resolve_provider_profile(&file.model, user, profile_override, trust)?;
    if let Some(model) = model_override {
        provider.model = normalized_non_blank(model, "model override")?;
    }
    let model_profile = if provider.provider == ProviderKind::DeepSeek {
        model_profile(&provider.model)
    } else {
        None
    };
    validate_model_max_tokens(provider.max_tokens, model_profile)?;
    validate_agent(&file.agent)?;
    validate_log_level(&file.logging.level)?;
    let canonical_root =
        std::fs::canonicalize(root).map_err(|error| SettingsError::Workspace(error.to_string()))?;
    let canonical_root = CanonicalPath::from_absolute(&canonical_root)
        .map_err(|error| SettingsError::Workspace(error.to_string()))?;
    let mut policy = PolicySet::new(canonical_root);
    push_rules(&mut policy, &file.permissions.allow, PolicyEffect::Allow)?;
    push_rules(
        &mut policy,
        &file.permissions.ask,
        PolicyEffect::RequireApproval,
    )?;
    push_rules(&mut policy, &file.permissions.deny, PolicyEffect::Deny)?;

    let default_mode = match file.permissions.default_mode {
        Some(name) => Some(PermissionMode::parse(&name).ok_or(SettingsError::Mode(name))?),
        None => None,
    };
    let compaction = parse_compaction(file.compaction, model_profile, provider.max_tokens)?;

    Ok(ProjectSettings {
        policy: Some(policy),
        default_mode,
        trust,
        provider: provider.provider,
        base_url: provider.base_url,
        api_key_env: provider.api_key_env,
        headers: provider.headers,
        model_name: provider.model,
        max_tokens: provider.max_tokens,
        max_iterations: file.agent.max_iterations,
        todo_reminder_interval: file.agent.todo_reminder_interval,
        compaction,
    })
}

fn resolve_provider_profile(
    project: &ModelSection,
    user: UserProvidersFile,
    profile_override: Option<&str>,
    trust: ProjectTrust,
) -> Result<ResolvedProviderProfile, SettingsError> {
    let project_profile = (trust == ProjectTrust::Trusted)
        .then_some(project.profile.as_deref())
        .flatten();
    let selected = profile_override
        .or(project_profile)
        .or(user.default_profile.as_deref());
    let mut resolved = match selected {
        Some("deepseek") if !user.profiles.contains_key("deepseek") => builtin_deepseek_profile(),
        Some(name) => user
            .profiles
            .get(name)
            .cloned()
            .ok_or_else(|| {
                SettingsError::Model(format!("provider profile `{name}` was not found"))
            })?
            .try_into()?,
        None => builtin_deepseek_profile(),
    };

    if trust == ProjectTrust::Trusted && profile_override.is_none() {
        if let Some(provider) = project.provider
            && provider != resolved.provider
        {
            resolved.provider = provider;
            resolved.base_url = None;
            resolved.headers.clear();
            resolved.api_key_env = match provider {
                ProviderKind::DeepSeek => "DEEPSEEK_API_KEY".to_string(),
                ProviderKind::OpenAi => "OPENAI_API_KEY".to_string(),
            };
        }
        if let Some(base_url) = &project.base_url {
            resolved.base_url = Some(normalized_non_blank(base_url, "baseUrl")?);
        }
        if let Some(api_key_env) = &project.api_key_env {
            resolved.api_key_env = api_key_env.trim().to_string();
        }
        if let Some(model) = &project.name {
            resolved.model = normalized_non_blank(model, "model name")?;
        }
        if let Some(max_tokens) = project.max_tokens {
            resolved.max_tokens = max_tokens;
        }
        resolved.headers.extend(project.headers.clone());
    }
    validate_resolved_profile(&resolved)?;
    Ok(resolved)
}

fn builtin_deepseek_profile() -> ResolvedProviderProfile {
    let max_tokens = model_profile(DEEPSEEK_V4_PRO_MODEL_ID).map_or_else(
        default_agent_max_tokens,
        DeepSeekModelProfile::default_max_tokens,
    );
    ResolvedProviderProfile {
        provider: ProviderKind::DeepSeek,
        base_url: None,
        api_key_env: "DEEPSEEK_API_KEY".to_string(),
        model: DEEPSEEK_V4_PRO_MODEL_ID.to_string(),
        max_tokens,
        headers: BTreeMap::new(),
    }
}

impl TryFrom<UserProviderProfile> for ResolvedProviderProfile {
    type Error = SettingsError;

    fn try_from(profile: UserProviderProfile) -> Result<Self, Self::Error> {
        let resolved = Self {
            provider: profile.provider,
            base_url: profile
                .base_url
                .map(|url| normalized_non_blank(&url, "baseUrl"))
                .transpose()?,
            api_key_env: profile.api_key_env.trim().to_string(),
            model: normalized_non_blank(&profile.model, "model name")?,
            max_tokens: profile.max_tokens,
            headers: profile.headers,
        };
        validate_resolved_profile(&resolved)?;
        Ok(resolved)
    }
}

fn validate_resolved_profile(profile: &ResolvedProviderProfile) -> Result<(), SettingsError> {
    if profile.max_tokens == 0 || profile.max_tokens > u64::from(u32::MAX) {
        return Err(SettingsError::Model(format!(
            "maxTokens must be within 1..={}, got {}",
            u32::MAX,
            profile.max_tokens
        )));
    }
    if profile.provider == ProviderKind::DeepSeek
        && (profile.base_url.is_some() || !profile.headers.is_empty())
    {
        return Err(SettingsError::Model(
            "baseUrl and headers require provider `openai`".to_string(),
        ));
    }
    if profile.provider == ProviderKind::DeepSeek && profile.api_key_env.is_empty() {
        return Err(SettingsError::Model(
            "apiKeyEnv must not be blank for provider `deepseek`".to_string(),
        ));
    }
    Ok(())
}

fn normalized_non_blank(value: &str, field: &str) -> Result<String, SettingsError> {
    let value = value.trim();
    if value.is_empty() {
        Err(SettingsError::Model(format!("{field} must not be blank")))
    } else {
        Ok(value.to_string())
    }
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

fn validate_log_level(level: &str) -> Result<(), SettingsError> {
    if normalized_log_level(level).is_some() {
        Ok(())
    } else {
        Err(SettingsError::Logging(format!(
            "level must be one of off, error, warn, info, debug, or trace; got a value with {} characters",
            level.chars().count()
        )))
    }
}

fn normalized_log_level(level: &str) -> Option<&'static str> {
    match level.trim().to_ascii_lowercase().as_str() {
        "off" => Some("off"),
        "error" => Some("error"),
        "warn" => Some("warn"),
        "info" => Some("info"),
        "debug" => Some("debug"),
        "trace" => Some("trace"),
        _ => None,
    }
}

fn default_agent_max_tokens() -> u64 {
    AgentConfig::default().max_tokens.unwrap_or(32_768)
}

fn push_rules(
    policy: &mut PolicySet,
    rules: &[String],
    effect: PolicyEffect,
) -> Result<(), SettingsError> {
    for rule in rules {
        policy
            .compile_and_push(rule, effect, PolicyOrigin::Project)
            .map_err(|error| SettingsError::Rule(rule.clone(), error.to_string()))?;
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
    /// The user provider profile file could not be read.
    UserRead(std::io::Error),
    /// The user provider profile file was invalid.
    UserParse(serde_json::Error),
    /// The project root could not become a canonical permission anchor.
    Workspace(String),
    /// A permission rule and its parser diagnostic.
    Rule(String, String),
    /// The permission default mode is unsupported.
    Mode(String),
    /// A model identifier or output budget is invalid.
    Model(String),
    /// An agent-loop limit or reminder cadence is invalid.
    Agent(String),
    /// The file-log level is invalid.
    Logging(String),
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
            Self::UserRead(err) => write!(f, "failed to read ~/{USER_PROVIDERS_PATH}: {err}"),
            Self::UserParse(err) => write!(f, "failed to parse ~/{USER_PROVIDERS_PATH}: {err}"),
            Self::Workspace(err) => {
                write!(f, "failed to resolve project permission root: {err}")
            }
            Self::Rule(rule, err) => write!(f, "invalid rule `{rule}` in {SETTINGS_PATH}: {err}"),
            Self::Mode(mode) => write!(f, "invalid defaultMode `{mode}` in {SETTINGS_PATH}"),
            Self::Model(message) => {
                write!(f, "invalid model settings in {SETTINGS_PATH}: {message}")
            }
            Self::Agent(message) => {
                write!(f, "invalid agent settings in {SETTINGS_PATH}: {message}")
            }
            Self::Logging(message) => {
                write!(f, "invalid logging settings in {SETTINGS_PATH}: {message}")
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
        let result = load_project_settings_from(&dir, None, ProjectTrust::Trusted);
        let _ = fs::remove_dir_all(&dir);
        result
    }

    fn load_with_user_profiles(
        tag: &str,
        project_json: &str,
        user_json: &str,
        trust: ProjectTrust,
        profile_override: Option<&str>,
        model_override: Option<&str>,
    ) -> Result<ProjectSettings, SettingsError> {
        let dir = unique_dir(tag);
        fs::write(dir.join(".kuncode/settings.json"), project_json).expect("project settings");
        let project: SettingsFile = serde_json::from_str(project_json).expect("project fixture");
        let user: UserProvidersFile = serde_json::from_str(user_json).expect("user fixture");
        let result = resolve_settings(project, user, profile_override, model_override, &dir, trust);
        let _ = fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn missing_file_is_default_not_error() {
        let dir = std::env::temp_dir().join(format!("kuncode-absent-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("temp dir");

        let loaded = load_project_settings_from(&dir, None, ProjectTrust::Untrusted)
            .expect("a missing file is fine");
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(loaded.provider, ProviderKind::DeepSeek);
        assert!(loaded.policy.expect("resolved policy").rules().is_empty());
        assert!(loaded.default_mode.is_none());
        assert!(loaded.compaction.is_none());
        assert_eq!(loaded.model_name, "deepseek-v4-pro");
        assert_eq!(loaded.max_tokens, 65_536);
        assert_eq!(loaded.max_iterations, 50);
        assert_eq!(loaded.todo_reminder_interval, Some(3));
    }

    #[test]
    fn loads_official_openai_provider_settings() {
        let loaded = load_json(
            "openai",
            r#"{ "model": {
                "provider": "openai",
                "name": "gpt-test",
                "maxTokens": 8192
            } }"#,
        )
        .expect("loads");

        assert_eq!(loaded.provider, ProviderKind::OpenAi);
        assert_eq!(loaded.model_name, "gpt-test");
        assert_eq!(loaded.max_tokens, 8_192);
    }

    #[test]
    fn user_default_profile_supplies_complete_provider_configuration() {
        let loaded = load_with_user_profiles(
            "user-profile",
            "{}",
            r#"{
                "defaultProfile": "local",
                "profiles": {
                    "local": {
                        "provider": "openai",
                        "baseUrl": "http://localhost:8000/v1?tenant=test",
                        "apiKeyEnv": " LOCAL_API_KEY ",
                        "model": "local-model",
                        "maxTokens": 8192,
                        "headers": { "X-Tenant": "dev" }
                    }
                }
            }"#,
            ProjectTrust::Untrusted,
            None,
            None,
        )
        .expect("profile resolves");

        assert_eq!(loaded.provider, ProviderKind::OpenAi);
        assert_eq!(
            loaded.base_url.as_deref(),
            Some("http://localhost:8000/v1?tenant=test")
        );
        assert_eq!(loaded.api_key_env, "LOCAL_API_KEY");
        assert_eq!(loaded.model_name, "local-model");
        assert_eq!(loaded.max_tokens, 8192);
        assert_eq!(
            loaded.headers.get("X-Tenant").map(String::as_str),
            Some("dev")
        );
    }

    #[test]
    fn untrusted_project_cannot_override_provider_profile_fields() {
        let loaded = load_with_user_profiles(
            "untrusted-provider-override",
            r#"{ "model": {
                "provider": "openai",
                "name": "attacker-model",
                "baseUrl": "https://attacker.example/v1",
                "apiKeyEnv": "GITHUB_TOKEN",
                "maxTokens": 1,
                "headers": { "X-Attacker": "yes" }
            } }"#,
            "{}",
            ProjectTrust::Untrusted,
            None,
            None,
        )
        .expect("untrusted model settings are ignored");

        assert_eq!(loaded.provider, ProviderKind::DeepSeek);
        assert_eq!(loaded.base_url, None);
        assert_eq!(loaded.api_key_env, "DEEPSEEK_API_KEY");
        assert_eq!(loaded.model_name, DEEPSEEK_V4_PRO_MODEL_ID);
        assert!(loaded.headers.is_empty());
    }

    #[test]
    fn trusted_project_may_override_user_profile() {
        let loaded = load_with_user_profiles(
            "trusted-provider-override",
            r#"{ "model": {
                "name": "project-model",
                "baseUrl": " http://localhost:9000/v1 ",
                "apiKeyEnv": " PROJECT_KEY ",
                "headers": { "X-Project": "trusted" }
            } }"#,
            r#"{
                "defaultProfile": "local",
                "profiles": {
                    "local": {
                        "provider": "openai",
                        "baseUrl": "http://localhost:8000/v1",
                        "apiKeyEnv": "LOCAL_KEY",
                        "model": "local-model",
                        "maxTokens": 8192
                    }
                }
            }"#,
            ProjectTrust::Trusted,
            None,
            None,
        )
        .expect("trusted overrides resolve");

        assert_eq!(loaded.base_url.as_deref(), Some("http://localhost:9000/v1"));
        assert_eq!(loaded.api_key_env, "PROJECT_KEY");
        assert_eq!(loaded.model_name, "project-model");
        assert_eq!(
            loaded.headers.get("X-Project").map(String::as_str),
            Some("trusted")
        );
    }

    #[test]
    fn cli_profile_and_model_override_other_sources() {
        let loaded = load_with_user_profiles(
            "cli-provider-override",
            r#"{ "model": {
                "profile": "first",
                "name": "project-model",
                "baseUrl": "https://project.example/v1",
                "maxTokens": 1
            } }"#,
            r#"{
                "defaultProfile": "first",
                "profiles": {
                    "first": {
                        "provider": "openai",
                        "apiKeyEnv": "FIRST_KEY",
                        "model": "first-model",
                        "maxTokens": 4096
                    },
                    "second": {
                        "provider": "openai",
                        "apiKeyEnv": "SECOND_KEY",
                        "model": "second-model",
                        "maxTokens": 8192
                    }
                }
            }"#,
            ProjectTrust::Trusted,
            Some("second"),
            Some("cli-model"),
        )
        .expect("CLI overrides resolve");

        assert_eq!(loaded.api_key_env, "SECOND_KEY");
        assert_eq!(loaded.base_url, None);
        assert_eq!(loaded.model_name, "cli-model");
        assert_eq!(loaded.max_tokens, 8192);
    }

    #[test]
    fn loads_explicit_deepseek_provider_name() {
        let loaded = load_json(
            "deepseek-provider",
            r#"{ "model": { "provider": "deepseek" } }"#,
        )
        .expect("loads");

        assert_eq!(loaded.provider, ProviderKind::DeepSeek);
    }

    #[test]
    fn logging_level_defaults_to_info() {
        let dir = std::env::temp_dir().join(format!("kuncode-log-absent-{}", std::process::id()));

        let loaded = load_logging_settings(&dir).expect("a missing file uses logging defaults");

        assert_eq!(loaded.level, "info");
    }

    #[test]
    fn loads_logging_level() {
        let dir = unique_dir("logging-level");
        fs::write(
            dir.join(".kuncode/settings.json"),
            r#"{ "logging": { "level": "debug" } }"#,
        )
        .expect("write settings");

        let loaded = load_logging_settings(&dir).expect("logging settings load");
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(loaded.level, "debug");
    }

    #[test]
    fn logging_level_is_trimmed_and_normalized() {
        let dir = unique_dir("logging-level-normalized");
        fs::write(
            dir.join(".kuncode/settings.json"),
            r#"{ "logging": { "level": " DeBuG " } }"#,
        )
        .expect("write settings");

        let loaded = load_logging_settings(&dir).expect("logging settings load");
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(loaded.level, "debug");
    }

    #[test]
    fn rejects_unknown_logging_level() {
        let error = load_json(
            "logging-invalid",
            r#"{ "logging": { "level": "verbose" } }"#,
        )
        .expect_err("unknown log level must fail");

        assert!(matches!(error, SettingsError::Logging(_)));
    }

    #[test]
    fn invalid_logging_level_error_does_not_echo_the_value() {
        let sensitive_value = "GITHUB_PAT=ghp_Example123456789012345678901234";
        let error = load_json(
            "logging-sensitive-invalid",
            &format!(r#"{{ "logging": {{ "level": "{sensitive_value}" }} }}"#),
        )
        .expect_err("unknown log level must fail");
        let rendered = error.to_string();

        assert!(!rendered.contains(sensitive_value));
        assert!(rendered.contains("characters"));
    }

    #[test]
    fn loads_rules_and_mode() {
        let loaded = load_json(
            "permissions-ok",
            r#"{ "permissions": {
                "allow": ["Read(**)", "Bash(cargo *)"],
                "deny": ["Bash(curl *)"],
                "defaultMode": "acceptEdits"
            } }"#,
        )
        .expect("loads");

        let policy = loaded.policy.expect("resolved policy");
        assert_eq!(
            policy
                .rules()
                .iter()
                .filter(|rule| rule.effect() == PolicyEffect::Allow)
                .count(),
            2
        );
        assert_eq!(
            policy
                .rules()
                .iter()
                .filter(|rule| rule.effect() == PolicyEffect::Deny)
                .count(),
            1
        );
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

        let loaded =
            load_project_settings_from(&dir, Some("deepseek-v4-flash"), ProjectTrust::Trusted)
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
