//! Assembly of a ready-to-run agent from parsed CLI args + project settings.
//!
//! [`CliRuntime`] is the single place that turns the parsed [`Cli`] and the
//! on-disk project settings into the configured pieces a run needs — model,
//! tool registry, system prompt, permission policy/mode. Both run paths (the
//! one-shot renderer in `main` and the [`tui`](crate::tui)) consume it and
//! differ *only* in the frontend's approval resolver + [`AgentObserver`], which they
//! pass to [`into_runner`](CliRuntime::into_runner). This keeps the assembly —
//! and the CLI's business decisions (identity prompt, todo-reminder cadence) —
//! out of `main`, and gives each frontend a single argument instead of a long
//! positional list.

use std::sync::Arc;

use kuncode_agent::observer::{AgentObserver, CompositeObserver};
use kuncode_agent::permission::{ApprovalResolver, CanonicalPath, PermissionMode, PolicySet};
use kuncode_agent::registry::ToolRegistry;
use kuncode_agent::runner::{
    AgentCompactionConfigError, AgentConfig, AgentRunner, AgentRunnerBuildError,
};
use kuncode_agent::session::AgentSession;
use kuncode_agent::session_store::{
    NewSession, SessionId, SessionStore, SessionSummary, session_store_path,
    turso::TursoSessionStore,
};
use kuncode_agent::system_prompt::{
    EnvironmentSection, IdentitySection, InstructionsSection, SystemPrompt, ToolsSection,
};
use kuncode_agent::workspace::Workspace;
use kuncode_core::completion::{CompletionModel, RetryModel, RetryPolicy};
use kuncode_core::providers::{
    any_chat::{AnyChatClient, AnyChatCompletionModel},
    deepseek::DeepSeekClient,
    openai::OpenAiClient,
};

use crate::config::{PermissionFlags, resolve_permissions};
use crate::instructions::load_instructions;
use crate::settings::{
    ProjectSettings, ProjectTrust, ProviderKind, SettingsError, load_project_settings,
};
use crate::{Cli, logging::LoggingObserver};

/// Identity and behavioral instructions rendered as the first system-prompt
/// block. Folds in guidance to maintain a plan via `todo_write`.
const IDENTITY: &str = "You are kuncode, a coding agent operating in the user's \
shell. Use the available tools when needed. For multi-step work, maintain a plan \
with todo_write and keep it current, marking steps completed as you finish them. \
Keep working until the task is done, then give a short, direct final answer.";

/// The assembled, frontend-agnostic pieces of one agent run.
///
/// Holds everything a [`runner`](Self::into_runner) needs except the frontend's
/// observer + approver, plus the bits a frontend renders directly
/// ([`model_name`](Self::model_name), [`mode`](Self::mode)). Generic over the
/// model so a test or a future provider can supply its own `M`; [`assemble`]
/// pins it to the configured [`AnyChatCompletionModel`] wrapped in a
/// [`RetryModel`] so transient provider failures are retried transparently.
///
/// [`assemble`]: Self::assemble
pub struct CliRuntime<M> {
    model: M,
    summary_model: M,
    registry: ToolRegistry,
    config: AgentConfig,
    system_prompt: SystemPrompt,
    policy: PolicySet,
    mode: PermissionMode,
    model_name: String,
    project_root: std::path::PathBuf,
    session_store: Option<Arc<dyn SessionStore>>,
    persistence_error: Option<String>,
    resume_target: Option<SessionId>,
    /// Provider client retained for mid-session model switches; cloning shares
    /// the underlying connection pool.
    client: AnyChatClient,
    /// Workspace trust as granted at startup, reused verbatim by switches.
    trust: ProjectTrust,
    /// Provider the retained client talks to; a switch must not cross it.
    provider: ProviderKind,
}

impl CliRuntime<RetryModel<AnyChatCompletionModel>> {
    /// Builds the runtime from parsed CLI args and the project settings file.
    ///
    /// Resolves permissions from built-in ∪ project file ∪ CLI flags (mode
    /// precedence CLI > project > Default), assembles the system prompt from its
    /// identity/environment/tools sections, and wires the configured model
    /// (name precedence `--model` > environment > settings file) + the default
    /// workspace tool registry.
    ///
    /// # Errors
    ///
    /// Fails if the current directory is not a usable workspace, the project
    /// settings or resolved permissions are invalid, active compaction cannot
    /// be bound to the selected model, or the provider client cannot be built
    /// from its fixed credential environment. Failure to open the optional session
    /// store is retained as degraded persistence state rather than failing assembly.
    pub async fn assemble(cli: &Cli) -> Result<Self, Box<dyn std::error::Error>> {
        let workspace = Workspace::from_current_dir().await?;
        tracing::debug!(
            target: "kuncode::runtime",
            project_root = %workspace.root().display(),
            "workspace resolved",
        );

        // The merge is pure and tested in `config`; loading the project file
        // (I/O) stays in `settings`.
        let project_trust = if cli.trust_project {
            ProjectTrust::Trusted
        } else {
            ProjectTrust::Untrusted
        };
        let project = load_project_settings(workspace.root(), project_trust, cli.model.as_deref())?;
        let model_name = project.model_name.clone();
        // Captured before `resolve_permissions` consumes the settings below.
        let provider = project.provider;
        let config = agent_config(&project)?;
        let client = provider_client(&project)?;
        let flags = PermissionFlags {
            allow: &cli.allow,
            ask: &cli.ask,
            deny: &cli.deny,
            mode: cli.mode.as_deref(),
        };
        let permission_root = CanonicalPath::from_absolute(workspace.root())?;
        let resolved = resolve_permissions(project, &flags, permission_root)?;
        if resolved.ignored_project_relaxations > 0 {
            tracing::warn!(
                target: "kuncode::authorization",
                ignored_relaxations = resolved.ignored_project_relaxations,
                "untrusted project permission relaxations were ignored; use --trust-project only after reviewing the workspace",
            );
        }
        tracing::info!(
            target: "kuncode::runtime",
            model = %model_name,
            permission_mode = ?resolved.mode,
            max_iterations = config.max_iterations,
            max_tokens = ?config.max_tokens,
            compaction_enabled = config.compaction.is_some(),
            "runtime settings resolved",
        );

        // Read once here, not per request: the system message is the cached
        // request prefix, so editing an instruction file mid-session must not
        // invalidate the transcript's KV cache. The instructions render last so
        // the project's own rules are the final word of the prompt.
        let instructions = load_instructions(workspace.root(), std::env::home_dir().as_deref());
        tracing::info!(
            target: "kuncode::runtime",
            instruction_documents = instructions.len(),
            "project instructions resolved",
        );

        // Built before `workspace` is moved into the registry below.
        let system_prompt = SystemPrompt::new(vec![
            Box::new(IdentitySection::new(IDENTITY)),
            Box::new(EnvironmentSection::new(workspace.root().to_path_buf())),
            Box::new(ToolsSection),
            Box::new(InstructionsSection::new(instructions)),
        ]);

        let project_root = workspace.root().to_path_buf();
        // Persistence discovery is non-fatal for CLI startup. Retaining the
        // reason lets the session warn once and deny lossy compaction without
        // preventing ordinary in-memory turns.
        let (session_store, persistence_error): (Option<Arc<dyn SessionStore>>, Option<String>) =
            match std::env::home_dir() {
                Some(home) => match TursoSessionStore::open(session_store_path(&home)).await {
                    Ok(store) => {
                        tracing::debug!(
                            target: "kuncode::persistence",
                            "session store opened",
                        );
                        (Some(Arc::new(store)), None)
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: "kuncode::persistence",
                            diagnostic_chars = error.to_string().chars().count(),
                            "session store unavailable",
                        );
                        (None, Some(error.to_string()))
                    }
                },
                None => {
                    tracing::warn!(
                        target: "kuncode::persistence",
                        "session store unavailable because home directory is missing",
                    );
                    (None, Some("home directory unavailable".to_string()))
                }
            };
        // Normal turns inherit the default retry budget. Semantic summaries use
        // a separate one-retry wrapper so their fallback latency is bounded
        // independently of ordinary model calls.
        let provider_model = AnyChatCompletionModel::make(&client, model_name.clone());
        let model = RetryModel::with_policy(provider_model.clone(), RetryPolicy::default());
        let summary_model = RetryModel::with_policy(provider_model, summary_retry_policy());
        let registry = ToolRegistry::with_default_workspace_tools(workspace)?;

        Ok(Self {
            model,
            summary_model,
            registry,
            config,
            system_prompt,
            policy: resolved.policy,
            mode: resolved.mode,
            model_name,
            project_root,
            session_store,
            persistence_error,
            resume_target: None,
            client,
            trust: project_trust,
            provider,
        })
    }

    /// Snapshot for mid-session `/model` switches; call before
    /// [`into_runner`](Self::into_runner) consumes the runtime.
    pub(crate) fn model_switcher(&self) -> ModelSwitcher {
        ModelSwitcher {
            client: self.client.clone(),
            project_root: self.project_root.clone(),
            trust: self.trust,
            provider: self.provider,
            base_config: self.config.clone(),
        }
    }
}

/// Everything a mid-session `/resume` needs after
/// [`into_runner`](CliRuntime::into_runner) consumed the runtime: the store
/// handle for listing and rebuilding this project's sessions, plus the startup
/// permission mode a rebuilt session begins from.
pub(crate) struct SessionResumer {
    store: Option<Arc<dyn SessionStore>>,
    persistence_error: Option<String>,
    project_root: std::path::PathBuf,
    mode: PermissionMode,
}

impl SessionResumer {
    /// Lists this project's stored sessions, most recently updated first.
    ///
    /// # Errors
    /// Fails when the session store is unavailable or the listing query fails;
    /// resume flows need the real reason instead of an empty list.
    pub(crate) async fn list(
        &self,
        limit: usize,
    ) -> Result<Vec<SessionSummary>, Box<dyn std::error::Error>> {
        let store = self.available_store()?;
        Ok(store.list_sessions(&self.project_root, limit).await?)
    }

    /// Rebuilds the stored session `id` at the startup permission mode.
    ///
    /// # Errors
    /// Fails when the store is unavailable or the rebuild rejects the id — the
    /// user asked for a specific history, so failure must not be silent.
    pub(crate) async fn resume(
        &self,
        id: SessionId,
    ) -> Result<AgentSession, Box<dyn std::error::Error>> {
        let store = self.available_store()?;
        let session =
            AgentSession::resume_durable_session(store.as_ref(), id.clone(), self.mode).await?;
        tracing::info!(
            target: "kuncode::persistence",
            session_id = id.as_str(),
            messages = session.messages().len(),
            "session resumed",
        );
        Ok(session)
    }

    fn available_store(&self) -> Result<&Arc<dyn SessionStore>, Box<dyn std::error::Error>> {
        match (&self.store, &self.persistence_error) {
            (Some(store), _) => Ok(store),
            (None, Some(reason)) => Err(format!("session store unavailable: {reason}").into()),
            (None, None) => Err("session store unavailable".into()),
        }
    }
}

/// Everything a mid-session `/model` switch needs after
/// [`into_runner`](CliRuntime::into_runner) consumed the runtime. All parts
/// are model-independent: the provider client (and its connection pool) is
/// reused; only the model, its output budget, and the compaction binding are
/// rebuilt per switch.
pub(crate) struct ModelSwitcher {
    client: AnyChatClient,
    project_root: std::path::PathBuf,
    trust: ProjectTrust,
    provider: ProviderKind,
    base_config: AgentConfig,
}

/// The validated products of one switch, applied atomically by the caller.
pub(crate) struct ModelSwitch {
    pub(crate) model: RetryModel<AnyChatCompletionModel>,
    pub(crate) summary_model: RetryModel<AnyChatCompletionModel>,
    pub(crate) config: AgentConfig,
    pub(crate) model_name: String,
}

impl ModelSwitcher {
    /// Model names worth offering in a selection list: the active provider's
    /// built-in profiles. Empty for providers without built-ins (the picker
    /// still lists the currently active model).
    pub(crate) fn known_models(&self) -> Vec<String> {
        match self.provider {
            ProviderKind::DeepSeek => kuncode_core::providers::deepseek::known_model_ids()
                .map(str::to_string)
                .collect(),
            ProviderKind::OpenAi => Vec::new(),
        }
    }

    /// Resolves and validates a switch to `name`, constructing the new model
    /// pair and configuration without touching any live state.
    ///
    /// Only the *model facts* of the reloaded settings are used — name,
    /// output budget, compaction binding. Permissions and mode are
    /// deliberately not re-resolved: session-scoped approval grants live in
    /// the runner and must survive a switch.
    ///
    /// # Errors
    ///
    /// Fails when the settings reload rejects the name (blank, budget or
    /// compaction constraints), the rebound compaction config is invalid, or
    /// the settings file now selects a different provider than the retained
    /// client was built for.
    pub(crate) fn switch(&self, name: &str) -> Result<ModelSwitch, ModelSwitchError> {
        let project = load_project_settings(&self.project_root, self.trust, Some(name))?;
        if project.provider != self.provider {
            return Err(ModelSwitchError::ProviderChanged {
                active: self.provider,
                requested: project.provider,
            });
        }
        let compaction = project
            .compaction
            .map(|settings| settings.into_runtime(&project.model_name))
            .transpose()?;
        let mut config = self.base_config.clone();
        config.max_tokens = Some(project.max_tokens);
        config.compaction = compaction;
        let provider_model = AnyChatCompletionModel::make(&self.client, project.model_name.clone());
        let model = RetryModel::with_policy(provider_model.clone(), RetryPolicy::default());
        let summary_model = RetryModel::with_policy(provider_model, summary_retry_policy());
        Ok(ModelSwitch {
            model,
            summary_model,
            config,
            model_name: project.model_name,
        })
    }
}

/// Why a `/model` switch was rejected; the live runner stays untouched.
#[derive(Debug)]
pub(crate) enum ModelSwitchError {
    /// The settings reload rejected the requested model.
    Settings(SettingsError),
    /// The rebound compaction runtime is invalid for the new model.
    Compaction(AgentCompactionConfigError),
    /// The settings file now selects a different provider than the retained
    /// client; switching providers needs a restart.
    ProviderChanged {
        /// Provider the running client was built for.
        active: ProviderKind,
        /// Provider the settings file selects now.
        requested: ProviderKind,
    },
}

impl From<SettingsError> for ModelSwitchError {
    fn from(error: SettingsError) -> Self {
        Self::Settings(error)
    }
}

impl From<AgentCompactionConfigError> for ModelSwitchError {
    fn from(error: AgentCompactionConfigError) -> Self {
        Self::Compaction(error)
    }
}

impl std::fmt::Display for ModelSwitchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Settings(error) => error.fmt(f),
            Self::Compaction(error) => error.fmt(f),
            Self::ProviderChanged { active, requested } => write!(
                f,
                "provider changed on disk ({} -> {}); restart kuncode to apply",
                active.as_str(),
                requested.as_str()
            ),
        }
    }
}

impl std::error::Error for ModelSwitchError {}

fn provider_client(project: &ProjectSettings) -> Result<AnyChatClient, Box<dyn std::error::Error>> {
    match project.provider {
        ProviderKind::DeepSeek => Ok(AnyChatClient::DeepSeek(DeepSeekClient::from_env()?)),
        ProviderKind::OpenAi => Ok(AnyChatClient::OpenAi(OpenAiClient::from_env()?)),
    }
}

fn agent_config(project: &ProjectSettings) -> Result<AgentConfig, AgentCompactionConfigError> {
    let compaction = project
        .compaction
        .map(|settings| settings.into_runtime(&project.model_name))
        .transpose()?;
    Ok(AgentConfig {
        max_iterations: project.max_iterations,
        max_tokens: Some(project.max_tokens),
        todo_reminder_interval: project.todo_reminder_interval,
        compaction,
        ..AgentConfig::default()
    })
}

fn summary_retry_policy() -> RetryPolicy {
    // Compaction owns its own fallback behavior, so repeated summary attempts
    // are capped here instead of inheriting the normal-turn retry count.
    RetryPolicy {
        max_retries: 1,
        ..RetryPolicy::default()
    }
}

impl<M: CompletionModel> CliRuntime<M> {
    /// The model identifier, for the frontend to display.
    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    /// The resolved permission mode, for the frontend to display.
    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    /// Lists this project's stored sessions, most recently updated first.
    ///
    /// # Errors
    /// Fails when the session store is unavailable or the listing query fails;
    /// resume flows need the real reason instead of an empty list.
    pub async fn list_sessions(
        &self,
        limit: usize,
    ) -> Result<Vec<SessionSummary>, Box<dyn std::error::Error>> {
        self.session_resumer().list(limit).await
    }

    /// Requests that [`session`](Self::session) resume this stored session
    /// instead of creating a new one.
    pub fn set_resume_target(&mut self, id: SessionId) {
        self.resume_target = Some(id);
    }

    /// Snapshot for mid-session `/resume`; like
    /// [`model_switcher`](CliRuntime::model_switcher), call before
    /// [`into_runner`](Self::into_runner) consumes the runtime.
    pub(crate) fn session_resumer(&self) -> SessionResumer {
        SessionResumer {
            store: self.session_store.clone(),
            persistence_error: self.persistence_error.clone(),
            project_root: self.project_root.clone(),
            mode: self.mode,
        }
    }

    /// Creates a session — or rebuilds the requested resume target — and
    /// schedules its durable identity.
    ///
    /// For a new session, durable creation is deferred: the runner creates the
    /// store row right before the first journaled message, so a session that
    /// never exchanges one is never persisted. A store-open failure is
    /// recorded on the returned session so observers can report the
    /// degradation and persistence-dependent compaction fails closed.
    /// Resuming is different — the user asked for a specific history, so any
    /// failure to rebuild it is an error rather than a silently empty session.
    ///
    /// # Errors
    /// Fails only when a resume target is set and the store or the rebuild
    /// rejects it (deferring on a freshly built session cannot fail).
    pub async fn session(&self) -> Result<AgentSession, Box<dyn std::error::Error>> {
        if let Some(id) = &self.resume_target {
            return self.session_resumer().resume(id.clone()).await;
        }
        let mut session = AgentSession::with_mode(self.mode);
        match (&self.session_store, &self.persistence_error) {
            (Some(_), _) => {
                session.defer_durable_session(NewSession::new(self.project_root.clone()))?;
            }
            (None, Some(error)) => session.mark_persistence_failed(error.clone()),
            (None, None) => {}
        }
        Ok(session)
    }

    /// Consumes the runtime into a configured [`AgentRunner`], wiring the
    /// frontend's `approver` + `observer`. This is the single assembly of the
    /// `with_*` chain both run paths share.
    pub fn into_runner(
        self,
        approver: Arc<dyn ApprovalResolver>,
        observer: Arc<dyn AgentObserver>,
    ) -> Result<AgentRunner<M>, AgentRunnerBuildError> {
        let observer = Arc::new(CompositeObserver(vec![
            observer,
            Arc::new(LoggingObserver) as Arc<dyn AgentObserver>,
        ]));
        let runner = AgentRunner::try_with_config(self.model, self.registry, self.config)?
            .with_summary_model(self.summary_model)
            .with_system_prompt(self.system_prompt)
            .with_policy(self.policy)?
            .with_approval_resolver(approver)
            .with_observer(observer);
        Ok(if let Some(store) = self.session_store {
            runner.with_session_store(store)
        } else {
            runner
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{ModelOverrides, ProjectSettings, load_project_settings_from};
    use std::fs;

    fn compaction_settings(tag: &str) -> ProjectSettings {
        let dir =
            std::env::temp_dir().join(format!("kuncode-runtime-{}-{tag}", std::process::id()));
        fs::create_dir_all(dir.join(".kuncode")).expect("temp dir");
        fs::write(
            dir.join(".kuncode/settings.json"),
            r#"{
            "model": { "maxTokens": 8192 },
            "compaction": {
                "mode": "enabled",
                "contextLimit": 131072,
                "reservedOutput": 8192
            } }"#,
        )
        .expect("write settings");
        let settings =
            load_project_settings_from(&dir, ModelOverrides::default(), ProjectTrust::Untrusted)
                .expect("load settings");
        let _ = fs::remove_dir_all(&dir);
        settings
    }

    #[test]
    fn absent_compaction_keeps_agent_default_disabled() {
        let project = ProjectSettings::default();
        let config = agent_config(&project).expect("valid agent config");

        assert!(config.compaction.is_none());
        assert_eq!(config.max_tokens, Some(65_536));
        assert_eq!(config.max_iterations, 50);
        assert_eq!(config.todo_reminder_interval, Some(3));
    }

    #[test]
    fn active_compaction_is_bound_to_runtime_model_name() {
        let mut settings = compaction_settings("model-binding");
        settings.model_name = " ".to_string();

        let error = agent_config(&settings).expect_err("blank runtime model must fail");

        assert_eq!(error, AgentCompactionConfigError::BlankModelId);
    }

    #[test]
    fn active_compaction_is_installed_for_concrete_model() {
        let settings = compaction_settings("model-enabled");

        let config = agent_config(&settings).expect("valid runtime model");

        assert!(config.compaction.is_some());
        assert_eq!(config.max_tokens, Some(8_192));
    }

    #[test]
    fn semantic_summary_retries_at_most_once() {
        let policy = summary_retry_policy();

        assert_eq!(policy.max_retries, 1);
    }

    /// A switcher over a temp project dir with the given settings JSON. The
    /// returned dir must outlive the [`ModelSwitcher::switch`] call — it reads
    /// the file at call time.
    fn switcher_over(tag: &str, json: &str) -> (ModelSwitcher, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("kuncode-switch-{}-{tag}", std::process::id()));
        fs::create_dir_all(dir.join(".kuncode")).expect("temp dir");
        fs::write(dir.join(".kuncode/settings.json"), json).expect("write settings");
        let client = AnyChatClient::DeepSeek(
            kuncode_core::providers::deepseek::DeepSeekClient::new("test-key").expect("client"),
        );
        let switcher = ModelSwitcher {
            client,
            project_root: dir.clone(),
            trust: ProjectTrust::Untrusted,
            provider: ProviderKind::DeepSeek,
            base_config: AgentConfig {
                max_iterations: 7,
                ..AgentConfig::default()
            },
        };
        (switcher, dir)
    }

    #[test]
    fn switch_rebinds_budget_and_compaction_to_the_new_model() {
        let (switcher, dir) = switcher_over(
            "rebind",
            r#"{
                "model": { "maxTokens": 8192 },
                "compaction": { "mode": "enabled", "contextLimit": 131072, "reservedOutput": 8192 }
            }"#,
        );

        let switch = switcher
            .switch("deepseek-v4-pro")
            .expect("a known model switches");
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(switch.model_name, "deepseek-v4-pro");
        assert_eq!(switch.config.max_tokens, Some(8_192));
        assert_eq!(
            switch
                .config
                .compaction
                .expect("compaction stays active")
                .model_id(),
            "deepseek-v4-pro",
        );
        // Only the model facts move; the rest of the config is the base's.
        assert_eq!(switch.config.max_iterations, 7);
    }

    /// `expect_err` without requiring `Debug` on the switch payload (the
    /// model handles hold credentials and deliberately stay un-`Debug`).
    fn switch_error(switcher: &ModelSwitcher, name: &str) -> ModelSwitchError {
        match switcher.switch(name) {
            Err(error) => error,
            Ok(_) => panic!("switch to `{name}` should have been rejected"),
        }
    }

    #[test]
    fn switch_rejects_a_provider_change_on_disk() {
        let (mut switcher, dir) = switcher_over("provider-drift", "{}");
        switcher.provider = ProviderKind::OpenAi;

        let error = switch_error(&switcher, "deepseek-v4-pro");
        let _ = fs::remove_dir_all(&dir);

        assert!(matches!(error, ModelSwitchError::ProviderChanged { .. }));
        assert!(error.to_string().contains("restart"));
    }

    #[test]
    fn switch_rejects_a_blank_model_name() {
        let (switcher, dir) = switcher_over("blank", "{}");

        let error = switch_error(&switcher, " ");
        let _ = fs::remove_dir_all(&dir);

        assert!(matches!(
            error,
            ModelSwitchError::Settings(SettingsError::Model(_))
        ));
    }

    #[test]
    fn known_models_lists_the_deepseek_builtins() {
        let (switcher, dir) = switcher_over("known-models", "{}");

        let models = switcher.known_models();
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(models, vec!["deepseek-v4-pro", "deepseek-v4-flash"]);
    }

    #[test]
    fn known_models_is_empty_for_providers_without_builtins() {
        let (mut switcher, dir) = switcher_over("known-models-openai", "{}");
        switcher.provider = ProviderKind::OpenAi;

        let models = switcher.known_models();
        let _ = fs::remove_dir_all(&dir);

        assert!(models.is_empty());
    }

    #[test]
    fn switch_to_an_unprofiled_model_with_active_compaction_requires_context_limit() {
        // Valid at startup: the flash profile supplies contextLimit. The
        // unprofiled target has no default, so the switch must be rejected.
        let (switcher, dir) = switcher_over(
            "no-context-limit",
            r#"{ "compaction": { "mode": "enabled" } }"#,
        );

        let error = switch_error(&switcher, "some-custom-model");
        let _ = fs::remove_dir_all(&dir);

        assert!(matches!(
            error,
            ModelSwitchError::Settings(SettingsError::CompactionContextLimit)
        ));
    }
}
