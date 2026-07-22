mod approver;
mod config;
mod logging;
mod observer;
mod runtime;
mod settings;
mod tui;
mod view;

use std::{
    io::{self, IsTerminal},
    sync::Arc,
};

use clap::Parser;
use kuncode_agent::{error::AgentError, runner::AgentRunner, session::AgentSession};
use kuncode_core::completion::CompletionModel;
use tokio_util::sync::CancellationToken;

use crate::{approver::TerminalApprover, runtime::CliRuntime};

/// kuncode — a coding agent operating in your shell.
///
/// Parsing lives here (a `main` concern); turning these args into a configured
/// run is [`CliRuntime::assemble`]. Fields read by the runtime are
/// `pub(crate)`; `prompt` stays private (only `main` dispatches on it).
#[derive(Parser, Debug)]
#[command(name = "kuncode", about = "A coding agent in your shell")]
pub(crate) struct Cli {
    /// Allow rule, e.g. `Bash(cargo *)` or `Read(./src/**)` (repeatable).
    #[arg(long = "allow", value_name = "RULE")]
    pub(crate) allow: Vec<String>,
    /// Always-ask rule, e.g. `Edit(.env)` (repeatable).
    #[arg(long = "ask", value_name = "RULE")]
    pub(crate) ask: Vec<String>,
    /// Deny rule, e.g. `Bash(curl *)` (repeatable).
    #[arg(long = "deny", value_name = "RULE")]
    pub(crate) deny: Vec<String>,
    /// Permission mode: default, accept-edits, plan, bypass-permissions, or dont-ask.
    #[arg(long = "mode", value_name = "MODE")]
    pub(crate) mode: Option<String>,
    /// Trust this workspace's permission relaxations for the current process.
    #[arg(long)]
    pub(crate) trust_project: bool,
    /// User-level provider profile selected for this run.
    #[arg(long, value_name = "PROFILE")]
    pub(crate) profile: Option<String>,
    /// Model identifier overriding profile and trusted project defaults.
    #[arg(long, value_name = "MODEL")]
    pub(crate) model: Option<String>,
    /// Prompt to run. Omit to start an interactive session.
    #[arg(trailing_var_arg = true)]
    prompt: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    // Keep the non-blocking writer alive until every async task has stopped so
    // shutdown flushes the final turn/error records before the process exits.
    let _logging_guard = logging::init(std::env::current_dir().ok().as_deref());
    logging::install_panic_hook();

    let result = run().await;
    if let Err(error) = &result {
        tracing::error!(
            target: "kuncode::runtime",
            diagnostic_chars = error.to_string().chars().count(),
            "kuncode exited with an error",
        );
    }
    result
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    tracing::info!(
        target: "kuncode::runtime",
        version = env!("CARGO_PKG_VERSION"),
        interactive = cli.prompt.is_empty(),
        "kuncode started",
    );

    // All assembly (workspace, settings, permissions, prompt, model, tools)
    // lives in `CliRuntime`; `main` only parses, dispatches, and owns the
    // one-shot turn's terminal line.
    let runtime = CliRuntime::assemble(&cli).await?;

    let initial_prompt = cli.prompt.join(" ");

    // A prompt on argv (or a non-TTY pipe) runs one-shot on the plain
    // line-by-line renderer; only the bare interactive session enters the TUI.
    if !initial_prompt.trim().is_empty() {
        let mut session = runtime.session().await;
        let runner =
            runtime.into_runner(Arc::new(TerminalApprover), Arc::new(observer::CliObserver))?;

        match run_turn(&runner, &mut session, initial_prompt).await {
            Ok(text) => println!("\n{text}"),
            Err(TurnError::Cancelled) => eprintln!("\n^C cancelled"),
            Err(TurnError::Agent(err)) => return Err(err.into()),
        }
        tracing::info!(target: "kuncode::runtime", "kuncode stopped");
        return Ok(());
    }

    // The TUI needs a real terminal; a no-prompt invocation in a pipe (no TTY)
    // can't drive raw mode, so guide the user to the one-shot form instead of
    // failing inside terminal setup.
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!(
            "kuncode: 交互模式需要终端。用 `kuncode \"<任务>\"` 传入一次性任务,或在终端中直接运行。"
        );
        tracing::info!(
            target: "kuncode::runtime",
            reason = "interactive_terminal_unavailable",
            "kuncode stopped",
        );
        return Ok(());
    }

    // Interactive: the TUI wraps the runtime with its own observer + approver
    // before driving the event loop.
    tui::run(runtime).await?;
    tracing::info!(target: "kuncode::runtime", "kuncode stopped");
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
    logging::log_prompt_preview(&input);
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
