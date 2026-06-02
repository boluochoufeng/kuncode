use kuncode_agent::{
    registry::ToolRegistry,
    runner::{AgentConfig, AgentRunner},
    workspace::Workspace,
};
use kuncode_core::{
    completion::CompletionModel,
    providers::deepseek::{DeepSeekClient, DeepSeekCompletionModel},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    let prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    if prompt.trim().is_empty() {
        eprintln!("usage: kuncode <prompt>");
        std::process::exit(2);
    }

    let workspace = Workspace::from_current_dir().await?;
    let system_prompt = format!(
        "You are kuncode, a coding agent operating in the user's shell. \
Use `read_file`, `write_file`, `edit_file`, and `glob` for workspace file \
operations under {}; use `bash` for commands. Prefer dedicated file tools over \
shell text manipulation. Keep working until the task is done, then give a \
short, direct final answer.",
        workspace.root().display()
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
    let run = runner.run(prompt).await?;
    println!("{}", run.final_text());

    Ok(())
}
