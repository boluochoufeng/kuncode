mod approver;
mod observer;
mod settings;

use std::{
    io::{self, Write},
    sync::Arc,
};

use clap::Parser;
use kuncode_agent::{
    error::AgentError,
    permission::{PermissionMode, PermissionPolicy, Rule, RuleOrigin, parse_rule},
    registry::ToolRegistry,
    runner::{AgentConfig, AgentRunner},
    session::AgentSession,
    workspace::Workspace,
};
use kuncode_core::{
    completion::CompletionModel,
    providers::deepseek::{DeepSeekClient, DeepSeekCompletionModel},
};
use tokio_util::sync::CancellationToken;

use crate::{approver::TerminalApprover, settings::load_project_settings};

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
    let cwd = workspace.root().display().to_string();

    // Assemble the permission policy: built-in deny ∪ project file ∪ CLI flags.
    let project = load_project_settings(workspace.root())?;
    let mut policy = PermissionPolicy::builtin();
    policy.append(project.policy);
    append_flag_rules(&mut policy, &cli)?;

    // Mode precedence: CLI flag > project file > Default.
    let mode = match &cli.mode {
        Some(name) => {
            PermissionMode::parse(name).ok_or_else(|| format!("invalid --mode `{name}`"))?
        }
        None => project.default_mode.unwrap_or_default(),
    };

    let system_prompt = format!(
        "You are kuncode, a coding agent operating in the user's shell. \
Operate under cwd {cwd}. Use the available tools when needed. Keep working until \
the task is done, then give a short, direct final answer."
    );

    let client = DeepSeekClient::from_env()?;
    let model_name =
        std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".to_string());
    let model = DeepSeekCompletionModel::make(&client, model_name);

    let registry = ToolRegistry::with_default_workspace_tools(workspace);
    let config = AgentConfig {
        system_prompt: Some(system_prompt),
        ..AgentConfig::default()
    };
    let runner = AgentRunner::with_config(model, registry, config)
        .with_policy(policy)
        .with_approver(Arc::new(TerminalApprover))
        .with_observer(Arc::new(observer::CliObserver));

    let mut session = AgentSession::with_mode(mode);

    let initial_prompt = cli.prompt.join(" ");
    if !initial_prompt.trim().is_empty() {
        match run_turn(&runner, &mut session, initial_prompt).await {
            Ok(text) => println!("\n{text}"),
            Err(TurnError::Cancelled) => eprintln!("\n^C cancelled"),
            Err(TurnError::Agent(err)) => return Err(err.into()),
        }
        return Ok(());
    }

    println!("kuncode interactive session. Type `exit` or `quit` to leave.");
    loop {
        print!("> ");
        io::stdout().flush()?;

        let mut input = String::new();
        if io::stdin().read_line(&mut input)? == 0 {
            break;
        }

        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if matches!(input, "exit" | "quit") {
            break;
        }

        match run_turn(&runner, &mut session, input.to_string()).await {
            Ok(text) => println!("\n{text}"),
            Err(TurnError::Cancelled) => eprintln!("\n^C cancelled"),
            Err(TurnError::Agent(err)) => eprintln!("error: {err}"),
        }
    }

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

/// Appends `--allow` / `--ask` / `--deny` flag rules onto the policy lists.
fn append_flag_rules(policy: &mut PermissionPolicy, cli: &Cli) -> Result<(), String> {
    push_flag_rules(&mut policy.allow, &cli.allow)?;
    push_flag_rules(&mut policy.ask, &cli.ask)?;
    push_flag_rules(&mut policy.deny, &cli.deny)?;
    Ok(())
}

fn push_flag_rules(target: &mut Vec<Rule>, rules: &[String]) -> Result<(), String> {
    for rule in rules {
        let parsed = parse_rule(rule, RuleOrigin::CliFlag)
            .map_err(|err| format!("invalid rule `{rule}`: {err}"))?;
        target.extend(parsed);
    }
    Ok(())
}
