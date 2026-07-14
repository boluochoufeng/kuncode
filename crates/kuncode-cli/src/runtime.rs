//! Assembly of a ready-to-run agent from parsed CLI args + project settings.
//!
//! [`CliRuntime`] is the single place that turns the parsed [`Cli`] and the
//! on-disk project settings into the configured pieces a run needs — model,
//! tool registry, system prompt, permission policy/mode. Both run paths (the
//! one-shot renderer in `main` and the [`tui`](crate::tui)) consume it and
//! differ *only* in the frontend's [`Approver`] + [`AgentObserver`], which they
//! pass to [`into_runner`](CliRuntime::into_runner). This keeps the assembly —
//! and the CLI's business decisions (identity prompt, todo-reminder cadence) —
//! out of `main`, and gives each frontend a single argument instead of a long
//! positional list.

use std::sync::Arc;

use kuncode_agent::observer::AgentObserver;
use kuncode_agent::permission::{Approver, PermissionMode, PermissionPolicy};
use kuncode_agent::registry::ToolRegistry;
use kuncode_agent::runner::{AgentCompactionConfigError, AgentConfig, AgentRunner};
use kuncode_agent::session::AgentSession;
use kuncode_agent::session_store::{
    NewSession, SessionStore, session_store_path, sqlite::SqliteSessionStore,
};
use kuncode_agent::system_prompt::{
    EnvironmentSection, IdentitySection, SystemPrompt, ToolsSection,
};
use kuncode_agent::workspace::Workspace;
use kuncode_core::completion::{CompletionModel, RetryModel, RetryPolicy};
use kuncode_core::providers::deepseek::{DeepSeekClient, DeepSeekCompletionModel};

use crate::Cli;
use crate::config::{PermissionFlags, resolve_permissions};
use crate::settings::{ProjectCompaction, load_project_settings};

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
    policy: PermissionPolicy,
    mode: PermissionMode,
    model_name: String,
    project_root: std::path::PathBuf,
    session_store: Option<Arc<SqliteSessionStore>>,
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

        // The merge is pure and tested in `config`; loading the project file
        // (I/O) stays in `settings`.
        let project = load_project_settings(workspace.root())?;
        let project_compaction = project.compaction;
        let flags = PermissionFlags {
            allow: &cli.allow,
            ask: &cli.ask,
            deny: &cli.deny,
            mode: cli.mode.as_deref(),
        };
        let resolved = resolve_permissions(project, &flags)?;

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
        let (session_store, persistence_error) = match std::env::home_dir() {
            Some(home) => match SqliteSessionStore::open(session_store_path(&home)).await {
                Ok(store) => (Some(Arc::new(store)), None),
                Err(error) => (None, Some(error.to_string())),
            },
            None => (None, Some("home directory unavailable".to_string())),
        };
        let client = DeepSeekClient::from_env()?;
        let model_name =
            std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".to_string());
        // Normal turns inherit the default retry budget. Semantic summaries use
        // a separate one-retry wrapper so their fallback latency is bounded
        // independently of ordinary model calls.
        let provider = DeepSeekCompletionModel::make(&client, model_name.clone());
        let model = RetryModel::with_policy(provider.clone(), RetryPolicy::default());
        let summary_model = RetryModel::with_policy(provider, summary_retry_policy());
        let registry = ToolRegistry::with_default_workspace_tools(workspace);
        let config = agent_config(project_compaction, &model_name)?;

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

fn agent_config(
    project_compaction: Option<ProjectCompaction>,
    model_name: &str,
) -> Result<AgentConfig, AgentCompactionConfigError> {
    // The same allowance must cap provider output and remain unavailable to the
    // input window; otherwise accounting could admit a request with no room for
    // the response it asks the provider to generate.
    let max_tokens = project_compaction
        .map(ProjectCompaction::reserved_output)
        .or(AgentConfig::default().max_tokens);
    let compaction = project_compaction
        .map(|settings| settings.into_runtime(model_name))
        .transpose()?;
    Ok(AgentConfig {
        max_tokens,
        todo_reminder_interval: Some(3),
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
                Ok(()) => {}
                Err(error) => session.mark_persistence_failed(error.to_string()),
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
        approver: Arc<dyn Approver>,
        observer: Arc<dyn AgentObserver>,
    ) -> AgentRunner<M> {
        let runner = AgentRunner::with_config(self.model, self.registry, self.config)
            .with_summary_model(self.summary_model)
            .with_system_prompt(self.system_prompt)
            .with_policy(self.policy)
            .with_approver(approver)
            .with_observer(observer);
        if let Some(store) = self.session_store {
            let store: Arc<dyn SessionStore> = store;
            runner.with_session_store(store)
        } else {
            runner
        }
    }
}

#[cfg(test)]
#[path = "runtime/tests.rs"]
mod tests;
