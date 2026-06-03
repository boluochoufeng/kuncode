use std::io::{self, Write};

use kuncode_agent::{
    registry::ToolRegistry,
    runner::{AgentConfig, AgentRunner},
    session::AgentSession,
    workspace::Workspace,
};
use kuncode_core::{
    completion::CompletionModel,
    providers::deepseek::{DeepSeekClient, DeepSeekCompletionModel},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    let workspace = Workspace::from_current_dir().await?;
    let cwd = workspace.root().display();
    let system_prompt = format!(
        "You are kuncode, a coding agent operating in the user's shell. \
Operate under cwd {}. Use the available tools when needed. Keep working until \
the task is done, then give a short, direct final answer.",
        cwd
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
    let runner = AgentRunner::with_config(model, registry, config);

    let mut session = AgentSession::new();
    let initial_prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    if !initial_prompt.trim().is_empty() {
        let turn = runner.run_turn(&mut session, initial_prompt).await?;
        println!("\n{}", turn.final_text(&session));
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

        match runner.run_turn(&mut session, input.to_string()).await {
            Ok(turn) => println!("\n{}", turn.final_text(&session)),
            Err(err) => eprintln!("error: {err}"),
        }
    }

    Ok(())
}
