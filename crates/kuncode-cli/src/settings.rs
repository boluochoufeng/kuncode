//! Loads `.kuncode/settings.json` into validated runtime settings.
//!
//! Missing sections and fields inherit built-in defaults. Rules remain
//! attributable to project policy, while model and compaction
//! budgets are checked against known provider capabilities before assembly.

use std::{collections::BTreeMap, num::NonZeroU32, path::Path};

use kuncode_agent::{
    compaction::budget::{CompactionConfig, CompactionMode},
    permission::{CanonicalPath, PermissionMode, PolicyEffect, PolicyOrigin, PolicySet},
    runner::{AgentCompactionConfig, AgentCompactionConfigError},
};
use kuncode_core::providers::deepseek::{
    DEEPSEEK_V4_FLASH_MODEL_ID, DeepSeekModelProfile, known_model_ids, model_profile,
};
use serde::Deserialize;

const SETTINGS_PATH: &str = ".kuncode/settings.json";
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
    #[serde(rename = "modelProfiles")]
    model_profiles: BTreeMap<String, ModelProfileSection>,
    agent: AgentSection,
    compaction: CompactionSection,
    logging: LoggingSection,
}

/// One named entry in `modelProfiles`: a reusable model choice that `/model`
/// (and, later, agent-type frontmatter) selects by profile name.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelProfileSection {
    /// Provider the profile targets, defaulting to the file's `model.provider`.
    /// A run keeps one provider client, so a profile for a different provider
    /// is valid configuration that this run cannot switch to.
    provider: Option<ProviderKind>,
    /// Model identifier sent to the provider.
    name: String,
    max_tokens: Option<u64>,
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
    provider: ProviderKind,
    /// Model identifier. Only DeepSeek has a built-in default; other providers
    /// must name a model explicitly — silently sending the DeepSeek default
    /// model id to a different provider would fail at the first request.
    name: Option<String>,
    max_tokens: Option<u64>,
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

impl ProviderKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::DeepSeek => "deepseek",
            Self::OpenAi => "openai",
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

/// One switchable model choice with every model-dependent setting resolved:
/// the provider model id, its output budget, and the compaction policy bound
/// to that budget.
#[derive(Clone, Debug)]
pub(crate) struct ModelRegistryEntry {
    pub(crate) model_name: String,
    pub(crate) max_tokens: u64,
    pub(crate) compaction: Option<ProjectCompaction>,
}

/// Every model this run can switch to, keyed by the name `/model` accepts.
///
/// Built once at load time from the named `modelProfiles`, the provider's
/// built-in model ids, and the startup model, so a mid-session switch is a
/// lookup — no disk re-read, no revalidation, and no way for a settings file
/// edited mid-session to hold the switch hostage. Insertion order is listing
/// order, and the first insertion of a name wins: profiles are inserted first
/// because a profile is the user's explicit word on what a name means, even
/// when it shadows a built-in id.
#[derive(Clone, Debug, Default)]
pub(crate) struct ModelRegistry {
    entries: Vec<(String, ModelRegistryEntry)>,
}

impl ModelRegistry {
    fn insert(&mut self, name: String, entry: ModelRegistryEntry) {
        if self.get(&name).is_none() {
            self.entries.push((name, entry));
        }
    }

    pub(crate) fn get(&self, name: &str) -> Option<&ModelRegistryEntry> {
        self.entries
            .iter()
            .find(|(entry_name, _)| entry_name == name)
            .map(|(_, entry)| entry)
    }

    /// Registered names in listing order: profiles (alphabetically), then
    /// provider built-ins, then the startup model when not already listed.
    pub(crate) fn names(&self) -> Vec<String> {
        self.entries.iter().map(|(name, _)| name.clone()).collect()
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
    /// Every model `/model` can switch to, resolved and validated at load.
    pub(crate) model_registry: ModelRegistry,
}

impl Default for ProjectSettings {
    fn default() -> Self {
        let max_tokens = model_profile(DEEPSEEK_V4_FLASH_MODEL_ID).map_or(
            CONSERVATIVE_DEFAULT_MAX_TOKENS,
            DeepSeekModelProfile::default_max_tokens,
        );
        Self {
            policy: None,
            default_mode: None,
            trust: ProjectTrust::Untrusted,
            provider: ProviderKind::DeepSeek,
            model_name: DEEPSEEK_V4_FLASH_MODEL_ID.to_string(),
            max_tokens,
            max_iterations: DEFAULT_MAX_ITERATIONS,
            todo_reminder_interval: default_todo_reminder_interval(),
            compaction: None,
            model_registry: ModelRegistry::default(),
        }
    }
}

/// Loads `.kuncode/settings.json` under `root`.
///
/// A missing file returns defaults. `cli_model` (the `--model` flag) names the
/// model for any provider and wins over the environment; `KUNCODE_MODEL`
/// overrides the file's model name for any provider; `DEEPSEEK_MODEL` remains
/// a backward-compatible fallback that applies only when the DeepSeek provider
/// is selected. Every section forms a closed schema, so misspelled fields fail
/// instead of silently selecting defaults.
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
    cli_model: Option<&str>,
) -> Result<ProjectSettings, SettingsError> {
    let universal = std::env::var("KUNCODE_MODEL").ok();
    let deepseek = std::env::var("DEEPSEEK_MODEL").ok();
    load_project_settings_from(
        root,
        ModelOverrides {
            cli: cli_model,
            universal: universal.as_deref(),
            deepseek: deepseek.as_deref(),
        },
        trust,
    )
}

/// Model-name overrides applied ahead of the file's own `model.name`: `cli`
/// (the `--model` flag) wins for every provider, then `universal`
/// (`KUNCODE_MODEL`) for every provider, while `deepseek` (`DEEPSEEK_MODEL`,
/// the pre-multi-provider compatibility variable) applies only when the file
/// selects the DeepSeek provider. A stale `DEEPSEEK_MODEL` export in a shell
/// rc must neither steer another provider's requests nor bypass that
/// provider's explicit-model-name requirement.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ModelOverrides<'a> {
    pub(crate) cli: Option<&'a str>,
    pub(crate) universal: Option<&'a str>,
    pub(crate) deepseek: Option<&'a str>,
}

impl<'a> ModelOverrides<'a> {
    /// The override that actually applies once the file's provider is known.
    fn for_provider(self, provider: ProviderKind) -> Option<&'a str> {
        self.cli.or(self.universal).or(match provider {
            ProviderKind::DeepSeek => self.deepseek,
            ProviderKind::OpenAi => None,
        })
    }
}

pub(crate) fn load_project_settings_from(
    root: &Path,
    overrides: ModelOverrides<'_>,
    trust: ProjectTrust,
) -> Result<ProjectSettings, SettingsError> {
    let file = read_settings_file(root)?;

    resolve_settings(file, overrides, root, trust)
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

fn resolve_settings(
    file: SettingsFile,
    overrides: ModelOverrides<'_>,
    root: &Path,
    trust: ProjectTrust,
) -> Result<ProjectSettings, SettingsError> {
    if overrides.universal.is_none()
        && overrides.deepseek.is_some()
        && file.model.provider != ProviderKind::DeepSeek
    {
        tracing::warn!(
            target: "kuncode::runtime",
            "DEEPSEEK_MODEL is set but the project selects a different provider; ignoring it",
        );
    }
    let model = resolve_model(
        file.model.provider,
        overrides.for_provider(file.model.provider),
        file.model.name.as_deref(),
        file.model.max_tokens,
    )?;
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
    let compaction = parse_compaction(
        &file.compaction,
        model.profile,
        model.max_tokens,
        model.max_tokens_note(),
    )?;
    let model_registry = build_model_registry(
        &file.model_profiles,
        file.model.provider,
        file.model.max_tokens,
        &file.compaction,
        &model,
        compaction,
    )?;

    Ok(ProjectSettings {
        policy: Some(policy),
        default_mode,
        trust,
        provider: file.model.provider,
        model_name: model.name,
        max_tokens: model.max_tokens,
        max_iterations: file.agent.max_iterations,
        todo_reminder_interval: file.agent.todo_reminder_interval,
        compaction,
        model_registry,
    })
}

/// Builds the switchable-model registry: named profiles first, then the
/// provider's built-in ids, then the startup model. Every entry is fully
/// resolved and validated here so `/model` can switch by lookup alone.
///
/// Built-ins reuse the file's `model.maxTokens` — exactly what resolving the
/// same id at startup would produce — and one the file's shared constraints
/// exclude is logged and left out rather than failing settings that are valid
/// for the startup model. A declared profile failing those constraints is an
/// error: it names a configuration the user asked for and cannot get.
fn build_model_registry(
    profiles: &BTreeMap<String, ModelProfileSection>,
    provider: ProviderKind,
    file_max_tokens: Option<u64>,
    compaction: &CompactionSection,
    startup: &ResolvedModel,
    startup_compaction: Option<ProjectCompaction>,
) -> Result<ModelRegistry, SettingsError> {
    let mut registry = ModelRegistry::default();
    for (profile_name, section) in profiles {
        if profile_name.trim().is_empty() {
            return Err(SettingsError::Model(
                "modelProfiles names must not be blank".to_string(),
            ));
        }
        if section.provider.unwrap_or(provider) != provider {
            tracing::warn!(
                target: "kuncode::runtime",
                profile = %profile_name,
                "model profile targets another provider; not switchable this run",
            );
            continue;
        }
        let entry = registry_entry(provider, &section.name, section.max_tokens, compaction)
            .map_err(|error| attribute_to_profile(profile_name, error))?;
        registry.insert(profile_name.clone(), entry);
    }
    if provider == ProviderKind::DeepSeek {
        for id in known_model_ids() {
            match registry_entry(provider, id, file_max_tokens, compaction) {
                Ok(entry) => registry.insert(id.to_string(), entry),
                Err(error) => tracing::warn!(
                    target: "kuncode::runtime",
                    model = id,
                    reason = %error,
                    "built-in model excluded from the switch registry",
                ),
            }
        }
    }
    registry.insert(
        startup.name.clone(),
        ModelRegistryEntry {
            model_name: startup.name.clone(),
            max_tokens: startup.max_tokens,
            compaction: startup_compaction,
        },
    );
    Ok(registry)
}

/// Resolves one registry entry the same way the startup model is resolved:
/// name and budget through [`resolve_model`], then the shared compaction
/// section re-bound to that model's capability profile and budget.
fn registry_entry(
    provider: ProviderKind,
    model_name: &str,
    max_tokens: Option<u64>,
    compaction: &CompactionSection,
) -> Result<ModelRegistryEntry, SettingsError> {
    let resolved = resolve_model(provider, Some(model_name), None, max_tokens)?;
    let compaction = parse_compaction(
        compaction,
        resolved.profile,
        resolved.max_tokens,
        resolved.max_tokens_note(),
    )?;
    Ok(ModelRegistryEntry {
        model_name: resolved.name,
        max_tokens: resolved.max_tokens,
        compaction,
    })
}

/// Prefixes an entry-level diagnostic with the profile that declared it: the
/// shared `model`/`compaction` sections the message cites are valid on their
/// own, so an unattributed message would point the user at the wrong place.
fn attribute_to_profile(profile: &str, error: SettingsError) -> SettingsError {
    match error {
        SettingsError::Model(message) => {
            SettingsError::Model(format!("modelProfiles.{profile}: {message}"))
        }
        SettingsError::Compaction(message) => {
            SettingsError::Compaction(format!("modelProfiles.{profile}: {message}"))
        }
        SettingsError::CompactionContextLimit => SettingsError::Compaction(format!(
            "modelProfiles.{profile}: compaction contextLimit is required because this model has no capability profile"
        )),
        other => other,
    }
}

/// One model choice validated against known provider capabilities.
///
/// Every surface that selects a model — the settings file, the environment
/// overrides, the `--model` flag, and an eventual `/model` command — funnels
/// through [`resolve_model`] into this type.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedModel {
    /// Effective model identifier.
    pub(crate) name: String,
    /// Effective provider output budget for an ordinary turn.
    pub(crate) max_tokens: u64,
    /// Capability profile, when the provider has one for this model.
    profile: Option<DeepSeekModelProfile>,
    /// Whether `max_tokens` is the conservative built-in default (no explicit
    /// budget and no capability profile to consult).
    conservative_default: bool,
}

impl ResolvedModel {
    /// Names where a defaulted budget came from: when a compaction mismatch
    /// error cites it, "16384" must be traceable to something the user can
    /// act on.
    fn max_tokens_note(&self) -> &'static str {
        if self.conservative_default {
            " (the built-in default for a model without a capability profile; set model.maxTokens to override)"
        } else {
            ""
        }
    }
}

/// Resolves a model request into a validated model choice.
///
/// `requested` is the highest-precedence explicit request (a CLI flag or
/// environment override today, a `/model` argument later); `file_name` and
/// `file_max_tokens` come from the settings file.
///
/// # Errors
///
/// Returns [`SettingsError::Model`] when no name is available for a provider
/// without a built-in default, the name is blank, or the output budget
/// violates the provider range or the model's capability profile.
pub(crate) fn resolve_model(
    provider: ProviderKind,
    requested: Option<&str>,
    file_name: Option<&str>,
    file_max_tokens: Option<u64>,
) -> Result<ResolvedModel, SettingsError> {
    let name = match (requested, file_name, provider) {
        (Some(name), _, _) => name.to_string(),
        (None, Some(name), _) => name.to_string(),
        (None, None, ProviderKind::DeepSeek) => DEEPSEEK_V4_FLASH_MODEL_ID.to_string(),
        // No cross-provider default exists: falling back to the DeepSeek model
        // id would send it to the other provider and fail at the first request.
        (None, None, ProviderKind::OpenAi) => {
            return Err(SettingsError::Model(format!(
                "provider \"{}\" requires an explicit model name",
                ProviderKind::OpenAi.as_str()
            )));
        }
    };
    if name.trim().is_empty() {
        return Err(SettingsError::Model(
            "model name must not be blank".to_string(),
        ));
    }
    let profile = if provider == ProviderKind::DeepSeek {
        model_profile(&name)
    } else {
        None
    };
    let max_tokens = file_max_tokens.unwrap_or_else(|| {
        profile.map_or(
            // No capability profile to consult (an unknown DeepSeek id or a
            // non-DeepSeek provider): default conservatively — every current
            // OpenAI model accepts 16k output, while the agent default of 32k
            // exceeds several of them and 400s the first request.
            CONSERVATIVE_DEFAULT_MAX_TOKENS,
            DeepSeekModelProfile::default_max_tokens,
        )
    });
    validate_model_max_tokens(max_tokens, profile)?;
    Ok(ResolvedModel {
        name,
        max_tokens,
        profile,
        conservative_default: file_max_tokens.is_none() && profile.is_none(),
    })
}

fn parse_compaction(
    section: &CompactionSection,
    profile: Option<DeepSeekModelProfile>,
    max_tokens: u64,
    max_tokens_note: &str,
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
            "reservedOutput {reserved_output} must equal model maxTokens {max_tokens}{max_tokens_note}"
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

/// Output budget when no capability profile is available (unknown DeepSeek ids
/// and every non-DeepSeek model). Deliberately below the agent's own default:
/// several current OpenAI models cap output at 16k, and a budget the provider
/// rejects fails every request outright — a smaller turn budget merely finishes
/// a long answer over more turns.
const CONSERVATIVE_DEFAULT_MAX_TOKENS: u64 = 16_384;

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
        let result =
            load_project_settings_from(&dir, ModelOverrides::default(), ProjectTrust::Trusted);
        let _ = fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn missing_file_is_default_not_error() {
        let dir = std::env::temp_dir().join(format!("kuncode-absent-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("temp dir");

        let loaded =
            load_project_settings_from(&dir, ModelOverrides::default(), ProjectTrust::Untrusted)
                .expect("a missing file is fine");
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(loaded.provider, ProviderKind::DeepSeek);
        assert!(loaded.policy.expect("resolved policy").rules().is_empty());
        assert!(loaded.default_mode.is_none());
        assert!(loaded.compaction.is_none());
        assert_eq!(loaded.model_name, "deepseek-v4-flash");
        assert_eq!(loaded.max_tokens, 65_536);
        assert_eq!(loaded.max_iterations, 50);
        assert_eq!(loaded.todo_reminder_interval, Some(3));
        assert_eq!(
            loaded.model_registry.names(),
            vec!["deepseek-v4-pro", "deepseek-v4-flash"]
        );
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
    fn deepseek_model_env_override_does_not_apply_to_openai() {
        // A stale `DEEPSEEK_MODEL` export must neither rename an OpenAI model…
        let dir = unique_dir("openai-stale-deepseek-env");
        fs::write(
            dir.join(".kuncode/settings.json"),
            r#"{ "model": { "provider": "openai", "name": "gpt-test" } }"#,
        )
        .expect("write settings");
        let overrides = ModelOverrides {
            cli: None,
            universal: None,
            deepseek: Some("deepseek-v4-flash"),
        };
        let loaded = load_project_settings_from(&dir, overrides, ProjectTrust::Trusted)
            .expect("loads with the file's own model");
        assert_eq!(loaded.model_name, "gpt-test");

        // …nor bypass the explicit-model-name requirement.
        fs::write(
            dir.join(".kuncode/settings.json"),
            r#"{ "model": { "provider": "openai" } }"#,
        )
        .expect("write settings");
        let error = load_project_settings_from(&dir, overrides, ProjectTrust::Trusted)
            .expect_err("the missing-name guard still fires");
        assert!(matches!(error, SettingsError::Model(_)));

        // KUNCODE_MODEL stays a universal override.
        let universal = ModelOverrides {
            cli: None,
            universal: Some("gpt-override"),
            deepseek: Some("deepseek-v4-flash"),
        };
        let loaded = load_project_settings_from(&dir, universal, ProjectTrust::Trusted)
            .expect("universal override names the model");
        assert_eq!(loaded.model_name, "gpt-override");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cli_model_override_wins_over_environment_and_file() {
        let dir = unique_dir("cli-model-override");
        fs::write(
            dir.join(".kuncode/settings.json"),
            r#"{ "model": { "name": "file-model" } }"#,
        )
        .expect("write settings");

        let overrides = ModelOverrides {
            cli: Some("cli-model"),
            universal: Some("env-model"),
            deepseek: Some("deepseek-env-model"),
        };
        let loaded = load_project_settings_from(&dir, overrides, ProjectTrust::Trusted)
            .expect("the flag names the model");
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(loaded.model_name, "cli-model");
    }

    #[test]
    fn cli_model_override_satisfies_the_openai_name_requirement() {
        let dir = unique_dir("cli-model-openai");
        fs::write(
            dir.join(".kuncode/settings.json"),
            r#"{ "model": { "provider": "openai" } }"#,
        )
        .expect("write settings");

        let overrides = ModelOverrides {
            cli: Some("gpt-cli"),
            universal: None,
            deepseek: None,
        };
        let loaded = load_project_settings_from(&dir, overrides, ProjectTrust::Trusted)
            .expect("the flag names the model explicitly");
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(loaded.model_name, "gpt-cli");
    }

    #[test]
    fn blank_cli_model_override_is_an_error() {
        let dir = unique_dir("cli-model-blank");
        fs::create_dir_all(dir.join(".kuncode")).expect("temp dir");

        let overrides = ModelOverrides {
            cli: Some(" "),
            universal: None,
            deepseek: None,
        };
        let error = load_project_settings_from(&dir, overrides, ProjectTrust::Trusted)
            .expect_err("a blank model name must fail");
        let _ = fs::remove_dir_all(&dir);

        assert!(matches!(error, SettingsError::Model(_)));
    }

    #[test]
    fn openai_provider_without_a_model_name_is_an_error() {
        // There is no cross-provider default: silently sending the DeepSeek
        // default model id to api.openai.com would 404 on the first request.
        let error = load_json("openai-unnamed", r#"{ "model": { "provider": "openai" } }"#)
            .expect_err("provider openai must require an explicit model name");

        assert!(matches!(error, SettingsError::Model(_)));
    }

    #[test]
    fn non_builtin_model_defaults_to_a_conservative_output_budget() {
        // No capability profile → 16k, not the 32k agent default that several
        // OpenAI models reject outright.
        let loaded = load_json(
            "openai-budget",
            r#"{ "model": { "provider": "openai", "name": "gpt-test" } }"#,
        )
        .expect("loads");
        assert_eq!(loaded.max_tokens, CONSERVATIVE_DEFAULT_MAX_TOKENS);

        let unknown_deepseek = load_json(
            "deepseek-unknown-budget",
            r#"{ "model": { "name": "deepseek-custom" } }"#,
        )
        .expect("loads");
        assert_eq!(unknown_deepseek.max_tokens, CONSERVATIVE_DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn budget_mismatch_error_names_the_defaulted_max_tokens_source() {
        // A config written against the old 32768 default must fail with an
        // error that explains where the new 16384 figure comes from.
        let error = load_json(
            "budget-note",
            r#"{ "model": { "name": "deepseek-custom" },
                 "compaction": { "mode": "enabled", "contextLimit": 131072, "reservedOutput": 32768 } }"#,
        )
        .expect_err("reservedOutput no longer matches the defaulted maxTokens");

        let text = error.to_string();
        assert!(
            text.contains("16384"),
            "cites the effective default: {text}"
        );
        assert!(
            text.contains("capability profile"),
            "explains the default's origin and the fix: {text}"
        );
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

        assert_eq!(loaded.model_name, "deepseek-v4-flash");
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
    fn registry_lists_profiles_then_builtins_then_startup() {
        let loaded = load_json(
            "registry-order",
            r#"{ "modelProfiles": {
                "fast": { "name": "deepseek-v4-flash", "maxTokens": 32768 }
            } }"#,
        )
        .expect("loads");

        // The startup model (flash) is already listed as a built-in id, so it
        // adds no extra entry.
        assert_eq!(
            loaded.model_registry.names(),
            vec!["fast", "deepseek-v4-pro", "deepseek-v4-flash"]
        );
        let fast = loaded.model_registry.get("fast").expect("profile entry");
        assert_eq!(fast.model_name, "deepseek-v4-flash");
        assert_eq!(fast.max_tokens, 32_768);
    }

    #[test]
    fn registry_lists_an_openai_startup_model() {
        let loaded = load_json(
            "registry-openai-startup",
            r#"{ "model": { "provider": "openai", "name": "gpt-test", "maxTokens": 8192 } }"#,
        )
        .expect("loads");

        assert_eq!(loaded.model_registry.names(), vec!["gpt-test"]);
    }

    #[test]
    fn a_profile_shadowing_a_builtin_id_wins() {
        let loaded = load_json(
            "registry-shadow",
            r#"{ "modelProfiles": {
                "deepseek-v4-pro": { "name": "deepseek-v4-pro", "maxTokens": 1024 }
            } }"#,
        )
        .expect("loads");

        let entry = loaded.model_registry.get("deepseek-v4-pro").expect("entry");
        assert_eq!(entry.max_tokens, 1_024);
    }

    #[test]
    fn a_profile_for_another_provider_is_not_switchable() {
        let loaded = load_json(
            "registry-cross-provider",
            r#"{ "modelProfiles": {
                "gpt": { "provider": "openai", "name": "gpt-test" }
            } }"#,
        )
        .expect("a foreign-provider profile is skipped, not an error");

        assert!(loaded.model_registry.get("gpt").is_none());
    }

    #[test]
    fn profile_compaction_binds_to_the_profile_model() {
        let loaded = load_json(
            "registry-compaction",
            r#"{
                "modelProfiles": { "pro": { "name": "deepseek-v4-pro" } },
                "compaction": { "mode": "enabled" }
            }"#,
        )
        .expect("loads");

        let entry = loaded.model_registry.get("pro").expect("profile entry");
        assert!(entry.compaction.is_some());
    }

    #[test]
    fn profile_budget_errors_name_the_profile() {
        let error = load_json(
            "registry-bad-budget",
            r#"{ "modelProfiles": {
                "fast": { "name": "deepseek-v4-flash", "maxTokens": 0 }
            } }"#,
        )
        .expect_err("a zero budget must fail");

        assert!(matches!(error, SettingsError::Model(_)));
        assert!(error.to_string().contains("modelProfiles.fast"));
    }

    #[test]
    fn profile_for_unprofiled_model_with_active_compaction_names_the_profile() {
        let error = load_json(
            "registry-no-context-limit",
            r#"{
                "modelProfiles": { "big": { "name": "some-custom-model" } },
                "compaction": { "mode": "enabled" }
            }"#,
        )
        .expect_err("an unprofiled model under active compaction needs contextLimit");

        assert!(matches!(error, SettingsError::Compaction(_)));
        assert!(error.to_string().contains("modelProfiles.big"));
    }

    #[test]
    fn profile_names_must_not_be_blank() {
        let error = load_json(
            "registry-blank-name",
            r#"{ "modelProfiles": { " ": { "name": "deepseek-v4-flash" } } }"#,
        )
        .expect_err("a blank profile name must fail");

        assert!(matches!(error, SettingsError::Model(_)));
    }

    #[test]
    fn profile_requires_a_model_name() {
        let error = load_json(
            "registry-missing-name",
            r#"{ "modelProfiles": { "fast": {} } }"#,
        )
        .expect_err("a profile without a model name must fail");

        assert!(matches!(error, SettingsError::Parse(_)));
    }

    #[test]
    fn profile_schema_is_closed() {
        let error = load_json(
            "registry-unknown-field",
            r#"{ "modelProfiles": {
                "fast": { "name": "deepseek-v4-flash", "budget": 1 }
            } }"#,
        )
        .expect_err("an unknown profile field must fail");

        assert!(matches!(error, SettingsError::Parse(_)));
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

        let loaded = load_project_settings_from(
            &dir,
            ModelOverrides {
                cli: None,
                universal: Some("deepseek-v4-pro"),
                deepseek: None,
            },
            ProjectTrust::Trusted,
        )
        .expect("environment override selects a known model");
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(loaded.model_name, "deepseek-v4-pro");
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
