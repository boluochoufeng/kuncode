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
    NewSession, SessionStore, session_store_path, turso::TursoSessionStore,
};
use kuncode_agent::system_prompt::{
    EnvironmentSection, IdentitySection, SystemPrompt, ToolsSection,
};
use kuncode_agent::workspace::Workspace;
use kuncode_core::completion::{CompletionModel, RetryModel, RetryPolicy};
use kuncode_core::providers::deepseek::{DeepSeekClient, DeepSeekCompletionModel};

use crate::config::{PermissionFlags, resolve_permissions};
use crate::settings::{ProjectSettings, ProjectTrust, load_project_settings};
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
/// pins it to the CLI's [`DeepSeekCompletionModel`] wrapped in a
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
}

impl CliRuntime<RetryModel<DeepSeekCompletionModel>> {
    /// Builds the runtime from parsed CLI args and the project settings file.
    ///
    /// Resolves permissions from built-in ∪ project file ∪ CLI flags (mode
    /// precedence CLI > project > Default), assembles the system prompt from its
    /// identity/environment/tools sections, and wires the DeepSeek model + the
    /// default workspace tool registry.
    ///
    /// # Errors
    ///
    /// Fails if the current directory is not a usable workspace, the project
    /// settings or resolved permissions are invalid, active compaction cannot
    /// be bound to the selected model, or the DeepSeek client cannot be built
    /// from the environment. Failure to open the optional session store is
    /// retained as degraded persistence state rather than failing assembly.
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
        let project = load_project_settings(workspace.root(), project_trust)?;
        let model_name = project.model_name.clone();
        let config = agent_config(&project)?;
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

        // Built before `workspace` is moved into the registry below.
        let system_prompt = SystemPrompt::new(vec![
            Box::new(IdentitySection::new(IDENTITY)),
            Box::new(EnvironmentSection::new(workspace.root().to_path_buf())),
            Box::new(ToolsSection),
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
        let client = DeepSeekClient::from_env()?;
        // Normal turns inherit the default retry budget. Semantic summaries use
        // a separate one-retry wrapper so their fallback latency is bounded
        // independently of ordinary model calls.
        let provider = DeepSeekCompletionModel::make(&client, model_name.clone());
        let model = RetryModel::with_policy(provider.clone(), RetryPolicy::default());
        let summary_model = RetryModel::with_policy(provider, summary_retry_policy());
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
        })
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

    /// Creates a session and attempts to establish its durable identity.
    ///
    /// Store-open and session-creation failures do not prevent session
    /// construction. They are recorded on the returned session so observers can
    /// report the degradation and persistence-dependent compaction fails closed.
    pub async fn session(&self) -> AgentSession {
        let mut session = AgentSession::with_mode(self.mode);
        match (&self.session_store, &self.persistence_error) {
            (Some(store), _) => match session
                .start_durable_session(store.as_ref(), NewSession::new(self.project_root.clone()))
                .await
            {
                Ok(()) => tracing::info!(
                    target: "kuncode::persistence",
                    session_id = session
                        .session_id()
                        .map_or("-", kuncode_agent::session_store::SessionId::as_str),
                    "durable session started",
                ),
                Err(error) => {
                    tracing::warn!(
                        target: "kuncode::persistence",
                        diagnostic_chars = error.to_string().chars().count(),
                        "durable session creation failed",
                    );
                    session.mark_persistence_failed(error.to_string());
                }
            },
            (None, Some(error)) => session.mark_persistence_failed(error.clone()),
            (None, None) => {}
        }
        session
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
    use crate::settings::{ProjectSettings, load_project_settings_from};
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
            load_project_settings_from(&dir, None, ProjectTrust::Untrusted).expect("load settings");
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
}
