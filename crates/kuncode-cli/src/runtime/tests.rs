use super::*;
use crate::settings::load_project_settings;
use std::fs;

fn compaction_settings(tag: &str) -> crate::settings::ProjectCompaction {
    let dir = std::env::temp_dir().join(format!("kuncode-runtime-{}-{tag}", std::process::id()));
    fs::create_dir_all(dir.join(".kuncode")).expect("temp dir");
    fs::write(
        dir.join(".kuncode/settings.json"),
        r#"{ "compaction": { "mode": "enabled", "contextLimit": 131072 } }"#,
    )
    .expect("write settings");
    let settings = load_project_settings(&dir).expect("load settings");
    let _ = fs::remove_dir_all(&dir);
    settings.compaction.expect("active compaction")
}

#[test]
fn absent_compaction_keeps_agent_default_disabled() {
    let config = agent_config(None, "deepseek-v4-flash").expect("valid agent config");

    assert!(config.compaction.is_none());
}

#[test]
fn active_compaction_is_bound_to_runtime_model_name() {
    let settings = compaction_settings("model-binding");

    let error = agent_config(Some(settings), " ").expect_err("blank runtime model must fail");

    assert_eq!(error, AgentCompactionConfigError::BlankModelId);
}

#[test]
fn active_compaction_is_installed_for_concrete_model() {
    let settings = compaction_settings("model-enabled");

    let config = agent_config(Some(settings), "deepseek-v4-flash").expect("valid runtime model");

    assert!(config.compaction.is_some());
}

#[test]
fn semantic_summary_retries_at_most_once() {
    let policy = summary_retry_policy();

    assert_eq!(policy.max_retries, 1);
}
