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

use std::collections::HashMap;
use std::sync::Arc;

use kuncode_agent::agent_type::AgentTypeCatalog;
use kuncode_agent::memory::{MemoryCatalog, memory_root};
use kuncode_agent::observer::{AgentObserver, CompositeObserver};
use kuncode_agent::permission::{ApprovalResolver, CanonicalPath, PermissionMode, PolicySet};
use kuncode_agent::registry::ToolRegistry;
use kuncode_agent::runner::{
    AgentCompactionConfigError, AgentConfig, AgentRunner, AgentRunnerBuildError, SubagentModel,
};
use kuncode_agent::session::AgentSession;
use kuncode_agent::session_store::{
    NewSession, SessionId, SessionStore, SessionSummary, session_store_path,
    turso::TursoSessionStore,
};
use kuncode_agent::skill::SkillCatalog;
use kuncode_agent::system_prompt::{
    EnvironmentSection, IdentitySection, InstructionsSection, MemorySection, PromptSection,
    SkillsSection, SystemPrompt, ToolsSection,
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
    ModelRegistry, ProjectSettings, ProjectTrust, ProviderKind, load_project_settings,
};
use crate::{Cli, logging::LoggingObserver};

/// The one model type the CLI actually runs: the configured provider model
/// wrapped in transparent retries. Concrete on purpose — a `/model` switch
/// rebuilds the turn and summary models with two different retry policies,
/// which a generic `M::make` cannot express — and named once so frontend
/// helpers take `&CliRunner` instead of re-growing generic bounds.
pub(crate) type CliModel = RetryModel<AnyChatCompletionModel>;

/// The runner every CLI frontend drives. See [`CliModel`].
pub(crate) type CliRunner = AgentRunner<CliModel>;

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
    /// Switchable models resolved at startup; `/model` consults only this.
    model_registry: ModelRegistry,
    /// Per-agent-type model overrides prebuilt from the registry; a `/model`
    /// switch leaves them pinned, while non-overridden types keep inheriting
    /// whatever the turn model currently is.
    subagent_models: HashMap<String, SubagentModel<M>>,
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
        let model_registry = project.model_registry.clone();
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
        let home = std::env::home_dir();
        let instructions = load_instructions(workspace.root(), home.as_deref());
        tracing::info!(
            target: "kuncode::runtime",
            instruction_documents = instructions.len(),
            "project instructions resolved",
        );

        // Same startup-frozen contract as the instructions: the catalog is a
        // prompt prefix, so skills added mid-session appear on the next start.
        let skill_catalog =
            SkillCatalog::scan(&kuncode_roots(workspace.root(), home.as_deref(), "skills"));
        tracing::info!(
            target: "kuncode::runtime",
            skills = skill_catalog.len(),
            "skill catalog resolved",
        );

        // Memory mirrors the skill contract: a startup-frozen index in the
        // prompt, full documents on demand. No home directory means no memory
        // root, and the feature silently stays unregistered — the session
        // store's degradation.
        let memory_root = home
            .as_deref()
            .map(|home| memory_root(home, workspace.root()));
        let memory_catalog = memory_root
            .as_deref()
            .map(MemoryCatalog::scan)
            .unwrap_or_default();
        tracing::info!(
            target: "kuncode::runtime",
            memories = memory_catalog.len(),
            "memory catalog resolved",
        );

        // Custom agent types are frozen at startup for the same reason: the
        // type list renders into the `task` tool definition, which is part of
        // the cached prompt prefix.
        let agent_types =
            AgentTypeCatalog::scan(&kuncode_roots(workspace.root(), home.as_deref(), "agents"));
        tracing::info!(
            target: "kuncode::runtime",
            custom_agent_types = agent_types.custom_len(),
            "agent type catalog resolved",
        );
        let subagent_models = subagent_model_table(&agent_types, &model_registry, &client);

        // Built before `workspace` is moved into the registry below.
        let mut sections: Vec<Box<dyn PromptSection>> = vec![
            Box::new(IdentitySection::new(IDENTITY)),
            Box::new(EnvironmentSection::new(workspace.root().to_path_buf())),
            Box::new(ToolsSection),
        ];
        if !skill_catalog.is_empty() {
            sections.push(Box::new(SkillsSection::new(skill_catalog.summaries())));
        }
        // An empty index omits itself, so the section is pushed unconditionally.
        sections.push(Box::new(MemorySection::new(memory_catalog.summaries())));
        sections.push(Box::new(InstructionsSection::new(instructions)));
        let system_prompt = SystemPrompt::new(sections);

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
        let mut registry = ToolRegistry::with_default_workspace_tools(workspace)?;
        // The default registration already advertises the built-in agent
        // types; only custom definitions change the `task` description, so
        // the slot is replaced (in place) just for them.
        if agent_types.custom_len() > 0 {
            registry.register_task_tool(Arc::new(agent_types))?;
        }
        // Advertised only when there is something to load; an empty catalog
        // would make the tool pure prompt noise.
        if !skill_catalog.is_empty() {
            registry.register_skill_tool(Arc::new(skill_catalog))?;
        }
        // Unlike `load_skill`, an empty catalog does not make the memory pair
        // noise: `write_memory` is how the first memory comes to exist, and a
        // memory written this session must be loadable in it.
        if let Some(root) = memory_root {
            registry.register_memory_tools(root)?;
        }

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
            model_registry,
            subagent_models,
        })
    }

    /// Snapshot for mid-session `/model` switches; call before
    /// [`into_runner`](Self::into_runner) consumes the runtime.
    pub(crate) fn model_switcher(&self) -> ModelSwitcher {
        ModelSwitcher {
            client: self.client.clone(),
            base_config: self.config.clone(),
            registry: self.model_registry.clone(),
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
/// [`into_runner`](CliRuntime::into_runner) consumed the runtime: the retained
/// provider client (and its connection pool) plus the startup-resolved model
/// registry. A switch is a registry lookup — the settings file is never
/// re-read, so an edit made mid-session can neither block the switch nor slip
/// unrelated changes into the running session.
pub(crate) struct ModelSwitcher {
    client: AnyChatClient,
    base_config: AgentConfig,
    registry: ModelRegistry,
}

/// The validated products of one switch, applied atomically by the caller.
pub(crate) struct ModelSwitch {
    pub(crate) model: RetryModel<AnyChatCompletionModel>,
    pub(crate) summary_model: RetryModel<AnyChatCompletionModel>,
    pub(crate) config: AgentConfig,
    pub(crate) model_name: String,
}

impl ModelSwitcher {
    /// Names worth offering in a selection list: every registry entry, in
    /// registry order — named profiles, provider built-ins, startup model.
    pub(crate) fn known_models(&self) -> Vec<String> {
        self.registry.names()
    }

    /// Resolves a switch to `name` — a profile name or registered model id —
    /// constructing the new model pair and configuration without touching any
    /// live state.
    ///
    /// Only model facts move: name, output budget, compaction binding.
    /// Permissions and mode are deliberately untouched — session-scoped
    /// approval grants live in the runner and must survive a switch.
    ///
    /// # Errors
    ///
    /// Fails when `name` is not in the startup registry, or the entry's
    /// compaction policy cannot be bound into a runtime config.
    pub(crate) fn switch(&self, name: &str) -> Result<ModelSwitch, ModelSwitchError> {
        let Some(entry) = self.registry.get(name) else {
            return Err(ModelSwitchError::UnknownModel {
                requested: name.to_string(),
                available: self.registry.names(),
            });
        };
        let compaction = entry
            .compaction
            .map(|settings| settings.into_runtime(&entry.model_name))
            .transpose()?;
        let mut config = self.base_config.clone();
        config.max_tokens = Some(entry.max_tokens);
        config.compaction = compaction;
        let provider_model = AnyChatCompletionModel::make(&self.client, entry.model_name.clone());
        let model = RetryModel::with_policy(provider_model.clone(), RetryPolicy::default());
        let summary_model = RetryModel::with_policy(provider_model, summary_retry_policy());
        Ok(ModelSwitch {
            model,
            summary_model,
            config,
            model_name: entry.model_name.clone(),
        })
    }
}

/// Why a `/model` switch was rejected; the live runner stays untouched.
#[derive(Debug)]
pub(crate) enum ModelSwitchError {
    /// The requested name is not in the startup registry.
    UnknownModel {
        /// The name as the user typed it.
        requested: String,
        /// Every name the registry does offer.
        available: Vec<String>,
    },
    /// The rebound compaction runtime is invalid for the new model.
    Compaction(AgentCompactionConfigError),
}

impl From<AgentCompactionConfigError> for ModelSwitchError {
    fn from(error: AgentCompactionConfigError) -> Self {
        Self::Compaction(error)
    }
}

impl std::fmt::Display for ModelSwitchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownModel {
                requested,
                available,
            } => write!(
                f,
                "unknown model or profile `{requested}`; available: {}",
                available.join(", ")
            ),
            Self::Compaction(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ModelSwitchError {}

/// `.kuncode/<leaf>` roots ordered least to most specific — the user-global
/// directory first, the workspace's own second — mirroring the `AGENTS.md`
/// precedence so a project entry overrides a global one of the same name.
/// Shared by the skill (`skills`) and agent-type (`agents`) catalogs.
fn kuncode_roots(
    root: &std::path::Path,
    home: Option<&std::path::Path>,
    leaf: &str,
) -> Vec<std::path::PathBuf> {
    let mut roots = Vec::with_capacity(2);
    if let Some(home) = home {
        roots.push(home.join(".kuncode").join(leaf));
    }
    let workspace_root = root.join(".kuncode").join(leaf);
    // Running kuncode inside the global directory itself would otherwise scan
    // the same entries twice, with the duplicate winning as "more specific".
    if !roots.contains(&workspace_root) {
        roots.push(workspace_root);
    }
    roots
}

/// Prebuilds the per-agent-type model table from the startup model registry.
///
/// Only custom types can request a model (the built-ins carry none), and the
/// requested name is a registry lookup — a profile name or a registered model
/// id. An unknown name degrades that type to inheriting the turn model, with
/// a warning: one wrong name in one agent file must not fail startup, and the
/// delegation itself still works.
fn subagent_model_table(
    agent_types: &AgentTypeCatalog,
    registry: &crate::settings::ModelRegistry,
    client: &AnyChatClient,
) -> HashMap<String, SubagentModel<CliModel>> {
    let mut table = HashMap::new();
    for agent_type in agent_types.types() {
        let Some(requested) = agent_type.model() else {
            continue;
        };
        let Some(entry) = registry.get(requested) else {
            tracing::warn!(
                target: "kuncode::subagent",
                agent_type = %agent_type.name(),
                model = %requested,
                "agent type requests an unknown model or profile; it will inherit the turn model",
            );
            continue;
        };
        let model = RetryModel::with_policy(
            AnyChatCompletionModel::make(client, entry.model_name.clone()),
            RetryPolicy::default(),
        );
        tracing::info!(
            target: "kuncode::subagent",
            agent_type = %agent_type.name(),
            model = %entry.model_name,
            max_tokens = entry.max_tokens,
            "agent type model override resolved",
        );
        table.insert(
            agent_type.name().to_string(),
            SubagentModel {
                model,
                max_tokens: entry.max_tokens,
                model_name: entry.model_name.clone(),
            },
        );
    }
    table
}

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
            .with_subagent_models(self.subagent_models)
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

    /// A switcher whose registry comes from loading the given settings JSON —
    /// the same path assembly takes — over a DeepSeek test client.
    fn switcher_from(tag: &str, json: &str) -> ModelSwitcher {
        let dir = std::env::temp_dir().join(format!("kuncode-switch-{}-{tag}", std::process::id()));
        fs::create_dir_all(dir.join(".kuncode")).expect("temp dir");
        fs::write(dir.join(".kuncode/settings.json"), json).expect("write settings");
        let settings =
            load_project_settings_from(&dir, ModelOverrides::default(), ProjectTrust::Untrusted)
                .expect("load settings");
        let _ = fs::remove_dir_all(&dir);
        let client = AnyChatClient::DeepSeek(
            kuncode_core::providers::deepseek::DeepSeekClient::new("test-key").expect("client"),
        );
        ModelSwitcher {
            client,
            base_config: AgentConfig {
                max_iterations: 7,
                ..AgentConfig::default()
            },
            registry: settings.model_registry,
        }
    }

    #[test]
    fn switch_rebinds_budget_and_compaction_to_the_new_model() {
        let switcher = switcher_from(
            "rebind",
            r#"{
                "model": { "maxTokens": 8192 },
                "compaction": { "mode": "enabled", "contextLimit": 131072, "reservedOutput": 8192 }
            }"#,
        );

        let switch = switcher
            .switch("deepseek-v4-pro")
            .expect("a built-in model switches");

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

    #[test]
    fn switch_by_profile_name_uses_the_profile_budget() {
        let switcher = switcher_from(
            "profile",
            r#"{ "modelProfiles": {
                "fast": { "name": "deepseek-v4-flash", "maxTokens": 32768 }
            } }"#,
        );

        let switch = switcher.switch("fast").expect("a profile name switches");

        assert_eq!(switch.model_name, "deepseek-v4-flash");
        assert_eq!(switch.config.max_tokens, Some(32_768));
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
    fn switch_rejects_a_name_missing_from_the_registry() {
        let switcher = switcher_from("unknown", "{}");

        let error = switch_error(&switcher, "no-such-model");

        assert!(matches!(error, ModelSwitchError::UnknownModel { .. }));
        // The rejection carries the actual options, not just a refusal.
        assert!(error.to_string().contains("deepseek-v4-pro"));
    }

    #[test]
    fn known_models_lists_profiles_then_builtins() {
        let switcher = switcher_from(
            "known-models",
            r#"{ "modelProfiles": {
                "fast": { "name": "deepseek-v4-flash", "maxTokens": 32768 }
            } }"#,
        );

        assert_eq!(
            switcher.known_models(),
            vec!["fast", "deepseek-v4-pro", "deepseek-v4-flash"]
        );
    }

    #[test]
    fn subagent_model_table_resolves_profiles_and_skips_unknown_names() {
        let dir = std::env::temp_dir().join(format!("kuncode-sub-models-{}", std::process::id()));
        fs::create_dir_all(dir.join(".kuncode")).expect("temp dir");
        fs::write(
            dir.join(".kuncode/settings.json"),
            r#"{ "modelProfiles": {
                "fast": { "name": "deepseek-v4-flash", "maxTokens": 32768 }
            } }"#,
        )
        .expect("write settings");
        let settings =
            load_project_settings_from(&dir, ModelOverrides::default(), ProjectTrust::Untrusted)
                .expect("load settings");
        let agents = dir.join("agents");
        fs::create_dir_all(&agents).expect("agents dir");
        fs::write(agents.join("explore.md"), "---\nmodel: fast\n---\nExplore.")
            .expect("definition");
        fs::write(agents.join("broken.md"), "---\nmodel: nope\n---\nBroken.").expect("definition");
        let catalog = AgentTypeCatalog::scan(std::slice::from_ref(&agents));
        let _ = fs::remove_dir_all(&dir);
        let client = AnyChatClient::DeepSeek(
            kuncode_core::providers::deepseek::DeepSeekClient::new("test-key").expect("client"),
        );

        let table = subagent_model_table(&catalog, &settings.model_registry, &client);

        let explore = table.get("explore").expect("profile-backed override");
        assert_eq!(explore.model_name, "deepseek-v4-flash");
        assert_eq!(explore.max_tokens, 32_768);
        // An unknown name degrades to inheritance; built-ins never override.
        assert!(!table.contains_key("broken"));
        assert!(!table.contains_key("general"));
        assert!(!table.contains_key("fork"));
    }

    #[test]
    fn known_models_for_openai_lists_the_startup_model() {
        let switcher = switcher_from(
            "known-models-openai",
            r#"{ "model": { "provider": "openai", "name": "gpt-test", "maxTokens": 8192 } }"#,
        );

        assert_eq!(switcher.known_models(), vec!["gpt-test"]);
    }
}
