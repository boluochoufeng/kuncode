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
use kuncode_agent::runner::{AgentConfig, AgentRunner};
use kuncode_agent::session::AgentSession;
use kuncode_agent::system_prompt::{
    EnvironmentSection, IdentitySection, SystemPrompt, ToolsSection,
};
use kuncode_agent::workspace::Workspace;
use kuncode_core::completion::CompletionModel;
use kuncode_core::providers::deepseek::{DeepSeekClient, DeepSeekCompletionModel};

use crate::Cli;
use crate::config::{PermissionFlags, resolve_permissions};
use crate::settings::load_project_settings;

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
/// pins it to the CLI's [`DeepSeekCompletionModel`].
///
/// [`assemble`]: Self::assemble
pub struct CliRuntime<M> {
    model: M,
    registry: ToolRegistry,
    config: AgentConfig,
    system_prompt: SystemPrompt,
    policy: PermissionPolicy,
    mode: PermissionMode,
    model_name: String,
}

impl CliRuntime<DeepSeekCompletionModel> {
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
    /// settings file is malformed, a permission rule is invalid, or the DeepSeek
    /// client cannot be built from the environment.
    pub async fn assemble(cli: &Cli) -> Result<Self, Box<dyn std::error::Error>> {
        let workspace = Workspace::from_current_dir().await?;

        // The merge is pure and tested in `config`; loading the project file
        // (I/O) stays in `settings`.
        let project = load_project_settings(workspace.root())?;
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

        let client = DeepSeekClient::from_env()?;
        let model_name =
            std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".to_string());
        let model = DeepSeekCompletionModel::make(&client, model_name.clone());
        let registry = ToolRegistry::with_default_workspace_tools(workspace);
        let config = AgentConfig {
            // Nudge the model to keep its plan current after a few quiet calls;
            // the library leaves this off, the CLI opts in.
            todo_reminder_interval: Some(3),
            ..AgentConfig::default()
        };

        Ok(Self {
            model,
            registry,
            config,
            system_prompt,
            policy: resolved.policy,
            mode: resolved.mode,
            model_name,
        })
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

    /// A fresh session seeded with the resolved permission mode.
    pub fn session(&self) -> AgentSession {
        AgentSession::with_mode(self.mode)
    }

    /// Consumes the runtime into a configured [`AgentRunner`], wiring the
    /// frontend's `approver` + `observer`. This is the single assembly of the
    /// `with_*` chain both run paths share.
    pub fn into_runner(
        self,
        approver: Arc<dyn Approver>,
        observer: Arc<dyn AgentObserver>,
    ) -> AgentRunner<M> {
        AgentRunner::with_config(self.model, self.registry, self.config)
            .with_system_prompt(self.system_prompt)
            .with_policy(self.policy)
            .with_approver(approver)
            .with_observer(observer)
    }
}
