mod approver;
mod config;
mod observer;
mod settings;
mod tui;

use std::{
    io::{self, IsTerminal},
    sync::Arc,
};

use clap::Parser;
use kuncode_agent::{
    error::AgentError,
    registry::ToolRegistry,
    runner::{AgentConfig, AgentRunner},
    session::AgentSession,
    system_prompt::{EnvironmentSection, IdentitySection, SystemPrompt, ToolsSection},
    workspace::Workspace,
};
use kuncode_core::{
    completion::CompletionModel,
    providers::deepseek::{DeepSeekClient, DeepSeekCompletionModel},
};
use tokio_util::sync::CancellationToken;

use crate::{
    approver::TerminalApprover,
    config::{PermissionFlags, resolve_permissions},
    settings::load_project_settings,
};

/// kuncode — a coding agent operating in your shell.
#[derive(Parser, Debug)]
#[command(name = "kuncode", about = "A coding agent in your shell")]
struct Cli {
    /// Allow rule, e.g. `Bash(cargo *)` or `Read` (repeatable).
    #[arg(long = "allow", value_name = "RULE")]
    allow: Vec<String>,
    /// Always-ask rule, e.g. `Edit(.env)` (repeatable).
    #[arg(long = "ask", value_name = "RULE")]
    ask: Vec<String>,
    /// Deny rule, e.g. `Bash(curl *)` (repeatable).
    #[arg(long = "deny", value_name = "RULE")]
    deny: Vec<String>,
    /// Permission mode: `default`, `accept-edits`, or `bypass`.
    #[arg(long = "mode", value_name = "MODE")]
    mode: Option<String>,
    /// Prompt to run. Omit to start an interactive session.
    #[arg(trailing_var_arg = true)]
    prompt: Vec<String>,
}

/// Identity and behavioral instructions rendered as the first system-prompt
/// block. Folds in guidance to maintain a plan via `todo_write`.
const IDENTITY: &str = "You are kuncode, a coding agent operating in the user's \
shell. Use the available tools when needed. For multi-step work, maintain a plan \
with todo_write and keep it current, marking steps completed as you finish them. \
Keep working until the task is done, then give a short, direct final answer.";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    // Permission decisions log to `kuncode::permission` at INFO; surface them
    // with e.g. `RUST_LOG=kuncode::permission=info`.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(io::stderr)
        .init();

    let cli = Cli::parse();

    let workspace = Workspace::from_current_dir().await?;

    // Resolve permissions from built-in ∪ project file ∪ CLI flags, with mode
    // precedence CLI > project > Default. The merge is pure and tested in `config`;
    // loading the project file (I/O) stays in `settings`.
    let project = load_project_settings(workspace.root())?;
    let flags = PermissionFlags {
        allow: &cli.allow,
        ask: &cli.ask,
        deny: &cli.deny,
        mode: cli.mode.as_deref(),
    };
    let resolved = resolve_permissions(project, &flags)?;

    // Assembled at request time from these sections (identity, environment,
    // tools). Built before `workspace` is moved into the registry below; moved
    // into whichever run path executes.
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
        // Nudge the model to keep its plan current after a few quiet calls; the
        // library leaves this off, the CLI opts in.
        todo_reminder_interval: Some(3),
        ..AgentConfig::default()
    };

    let initial_prompt = cli.prompt.join(" ");

    // A prompt on argv (or a non-TTY pipe) runs one-shot on the plain
    // line-by-line renderer; only the bare interactive session enters the TUI.
    if !initial_prompt.trim().is_empty() {
        let runner = AgentRunner::with_config(model, registry, config)
            .with_system_prompt(system_prompt)
            .with_policy(resolved.policy)
            .with_approver(Arc::new(TerminalApprover))
            .with_observer(Arc::new(observer::CliObserver));
        let mut session = AgentSession::with_mode(resolved.mode);

        match run_turn(&runner, &mut session, initial_prompt).await {
            Ok(text) => println!("\n{text}"),
            Err(TurnError::Cancelled) => eprintln!("\n^C cancelled"),
            Err(TurnError::Agent(err)) => return Err(err.into()),
        }
        return Ok(());
    }

    // The TUI needs a real terminal; a no-prompt invocation in a pipe (no TTY)
    // can't drive raw mode, so guide the user to the one-shot form instead of
    // failing inside terminal setup.
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!(
            "kuncode: 交互模式需要终端。用 `kuncode \"<任务>\"` 传入一次性任务,或在终端中直接运行。"
        );
        return Ok(());
    }

    // Interactive: hand the assembled runner pieces to the TUI, which wraps them
    // with its own observer + approver before driving the event loop.
    tui::run(
        model,
        registry,
        config,
        system_prompt,
        resolved.policy,
        resolved.mode,
        model_name,
    )
    .await?;
    Ok(())
}

/// A turn either produced final text, was cancelled (Ctrl-C / abort), or failed.
enum TurnError {
    Cancelled,
    Agent(AgentError),
}

/// Runs one turn with a Ctrl-C-wired cancellation token, so an interrupt aborts
/// the current turn and (in the REPL) returns to the prompt instead of killing
/// the process.
async fn run_turn<M: CompletionModel>(
    runner: &AgentRunner<M>,
    session: &mut AgentSession,
    input: String,
) -> Result<String, TurnError> {
    let cancel = CancellationToken::new();
    let guard = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        })
    };

    let result = runner.run_turn_with(session, input, cancel).await;
    guard.abort(); // Stop listening for Ctrl-C once the turn is done.

    match result {
        Ok(turn) => Ok(turn.final_text(session)),
        Err(AgentError::Cancelled) => Err(TurnError::Cancelled),
        Err(err) => Err(TurnError::Agent(err)),
    }
}
