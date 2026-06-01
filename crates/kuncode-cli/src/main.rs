use kuncode_agent::{
    registry::ToolRegistry,
    runner::{AgentConfig, AgentRunner},
    tool::bash::Bash,
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

    let workspace = std::env::current_dir()?;
    let system_prompt = format!(
        "You are kuncode, a coding agent operating in the user's shell. \
Use the `bash` tool to inspect and act on {}; prefer concrete commands over \
guessing. Keep working until the task is done, then give a short, direct final \
answer.",
        workspace.display()
    );

    let client = DeepSeekClient::from_env()?;
    let model_name =
        std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".to_string());
    let model = DeepSeekCompletionModel::make(&client, model_name);

    let mut registry = ToolRegistry::new();
    registry.register(Bash::new());

    let config = AgentConfig {
        system_prompt: Some(system_prompt),
        ..AgentConfig::default()
    };
    let runner = AgentRunner::with_config(model, registry, config);
    let run = runner.run(prompt).await?;
    println!("{}", run.final_text());

    Ok(())
}
