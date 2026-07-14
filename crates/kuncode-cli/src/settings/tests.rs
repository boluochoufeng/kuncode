use super::*;
use kuncode_agent::compaction::budget::CompactionMode;
use std::{fs, path::PathBuf};

fn unique_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kuncode-settings-{}-{tag}", std::process::id()));
    fs::create_dir_all(dir.join(".kuncode")).expect("temp dir");
    dir
}

fn load_json(tag: &str, json: &str) -> Result<ProjectSettings, SettingsError> {
    let dir = unique_dir(tag);
    fs::write(dir.join(".kuncode/settings.json"), json).expect("write settings");
    let result = load_project_settings_from(&dir, None);
    let _ = fs::remove_dir_all(&dir);
    result
}

#[test]
fn missing_file_is_default_not_error() {
    let dir = std::env::temp_dir().join(format!("kuncode-absent-{}", std::process::id()));

    let loaded = load_project_settings_from(&dir, None).expect("a missing file is fine");

    assert!(loaded.policy.deny.is_empty());
    assert!(loaded.default_mode.is_none());
    assert!(loaded.compaction.is_none());
    assert_eq!(loaded.model_name, "deepseek-v4-pro");
    assert_eq!(loaded.max_tokens, 65_536);
    assert_eq!(loaded.max_iterations, 50);
    assert_eq!(loaded.todo_reminder_interval, Some(3));
}

#[test]
fn loads_rules_and_mode() {
    let loaded = load_json(
        "permissions-ok",
        r#"{ "permissions": {
            "allow": ["Read", "Bash(cargo *)"],
            "deny": ["Bash(curl *)"],
            "defaultMode": "acceptEdits"
        } }"#,
    )
    .expect("loads");

    assert_eq!(loaded.policy.allow.len(), 3);
    assert_eq!(loaded.policy.deny.len(), 1);
    assert_eq!(loaded.default_mode, Some(PermissionMode::AcceptEdits));
}

#[test]
fn absent_compaction_is_not_installed() {
    let loaded = load_json("compaction-absent", "{}").expect("loads");

    assert!(loaded.compaction.is_none());
}

#[test]
fn compaction_mode_defaults_to_disabled() {
    let loaded = load_json(
        "compaction-mode-default",
        r#"{ "compaction": { "contextLimit": 131072 } }"#,
    )
    .expect("loads");

    assert!(loaded.compaction.is_none());
}

#[test]
fn disabled_compaction_is_not_installed() {
    let loaded = load_json(
        "compaction-disabled",
        r#"{ "compaction": { "mode": "disabled" } }"#,
    )
    .expect("loads");

    assert!(loaded.compaction.is_none());
}

#[test]
fn shadow_compaction_uses_runtime_defaults() {
    let loaded = load_json(
        "compaction-defaults",
        r#"{ "compaction": { "mode": "shadow" } }"#,
    )
    .expect("loads");

    let compaction = loaded.compaction.expect("compaction enabled");
    assert_eq!(compaction.policy.mode(), CompactionMode::Shadow);
    assert_eq!(compaction.policy.context_limit(), 400_000);
    assert_eq!(compaction.policy.reserved_output(), 65_536);
    assert_eq!(compaction.policy.safety_margin(), 16_384);
    assert_eq!(compaction.policy.target_ratio(), 0.50);
    assert_eq!(compaction.policy.soft_threshold(), 0.75);
    assert_eq!(compaction.policy.hard_threshold(), 0.90);
    assert_eq!(compaction.policy.recent_ratio(), 0.10);
    assert_eq!(compaction.summary_max_tokens.get(), 16_384);
}

#[test]
fn partial_settings_override_only_selected_defaults() {
    let loaded = load_json(
        "partial-defaults",
        r#"{
            "model": { "maxTokens": 32768 },
            "agent": { "maxIterations": 12, "todoReminderInterval": null },
            "compaction": {
                "mode": "shadow",
                "reservedOutput": 32768
            }
        }"#,
    )
    .expect("loads");

    assert_eq!(loaded.model_name, "deepseek-v4-pro");
    assert_eq!(loaded.max_tokens, 32_768);
    assert_eq!(loaded.max_iterations, 12);
    assert_eq!(loaded.todo_reminder_interval, None);
    let compaction = loaded.compaction.expect("compaction enabled");
    assert_eq!(compaction.policy.context_limit(), 400_000);
    assert_eq!(compaction.policy.reserved_output(), 32_768);
    assert_eq!(compaction.policy.safety_margin(), 16_384);
    assert_eq!(compaction.summary_max_tokens.get(), 16_384);
}

#[test]
fn enabled_compaction_loads_all_camel_case_fields() {
    let loaded = load_json(
        "compaction-full",
        r#"{
        "model": { "maxTokens": 8192 },
        "compaction": {
            "mode": "enabled",
            "contextLimit": 65536,
            "reservedOutput": 8192,
            "safetyMargin": 2048,
            "summaryMaxTokens": 1024,
            "targetRatio": 0.4,
            "softThreshold": 0.7,
            "hardThreshold": 0.85,
            "recentRatio": 0.2
        } }"#,
    )
    .expect("loads");

    let compaction = loaded.compaction.expect("compaction enabled");
    assert_eq!(compaction.policy.mode(), CompactionMode::Enabled);
    assert_eq!(compaction.policy.context_limit(), 65_536);
    assert_eq!(compaction.policy.reserved_output(), 8_192);
    assert_eq!(compaction.policy.safety_margin(), 2_048);
    assert_eq!(compaction.policy.target_ratio(), 0.4);
    assert_eq!(compaction.policy.soft_threshold(), 0.7);
    assert_eq!(compaction.policy.hard_threshold(), 0.85);
    assert_eq!(compaction.policy.recent_ratio(), 0.2);
    assert_eq!(compaction.summary_max_tokens.get(), 1_024);
}

#[test]
fn invalid_compaction_mode_is_an_error() {
    let result = load_json(
        "compaction-mode-invalid",
        r#"{ "compaction": { "mode": "automatic", "contextLimit": 65536 } }"#,
    );

    assert!(matches!(result, Err(SettingsError::CompactionMode(_))));
}

#[test]
fn unknown_compaction_field_is_an_error() {
    let result = load_json(
        "compaction-unknown-field",
        r#"{ "compaction": { "mode": "enabled", "contextLimit": 65536, "softRatio": 0.7 } }"#,
    );

    assert!(matches!(result, Err(SettingsError::Parse(_))));
}

#[test]
fn unknown_model_active_compaction_requires_context_limit() {
    let result = load_json(
        "compaction-context-missing",
        r#"{
            "model": { "name": "custom-model" },
            "compaction": { "mode": "enabled" }
        }"#,
    );

    assert!(matches!(result, Err(SettingsError::CompactionContextLimit)));
}

#[test]
fn model_environment_override_drives_known_defaults() {
    let dir = unique_dir("model-env-override");
    fs::write(
        dir.join(".kuncode/settings.json"),
        r#"{
            "model": { "name": "custom-model", "maxTokens": 8192 },
            "compaction": { "mode": "enabled" }
        }"#,
    )
    .expect("write settings");

    let loaded = load_project_settings_from(&dir, Some("deepseek-v4-flash"))
        .expect("environment override selects a known model");
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(loaded.model_name, "deepseek-v4-flash");
    assert_eq!(loaded.max_tokens, 8_192);
    assert_eq!(
        loaded
            .compaction
            .expect("compaction enabled")
            .policy
            .context_limit(),
        400_000
    );
}

#[test]
fn compaction_reserved_output_must_match_model_max_tokens() {
    let result = load_json(
        "compaction-output-mismatch",
        r#"{
            "model": { "maxTokens": 32768 },
            "compaction": {
                "mode": "enabled",
                "reservedOutput": 65536
            }
        }"#,
    );

    assert!(matches!(result, Err(SettingsError::Compaction(_))));
}

#[test]
fn known_model_limits_reject_oversized_operational_budgets() {
    let context = load_json(
        "model-context-oversized",
        r#"{
            "compaction": { "mode": "enabled", "contextLimit": 1000001 }
        }"#,
    );
    let output = load_json(
        "model-output-oversized",
        r#"{ "model": { "maxTokens": 384001 } }"#,
    );

    assert!(matches!(context, Err(SettingsError::Compaction(_))));
    assert!(matches!(output, Err(SettingsError::Model(_))));
}

#[test]
fn unknown_top_level_and_section_fields_are_errors() {
    let top_level = load_json("unknown-top-level", r#"{ "modle": {} }"#);
    let model = load_json("unknown-model-field", r#"{ "model": { "id": "x" } }"#);
    let agent = load_json("unknown-agent-field", r#"{ "agent": { "iterations": 3 } }"#);

    assert!(matches!(top_level, Err(SettingsError::Parse(_))));
    assert!(matches!(model, Err(SettingsError::Parse(_))));
    assert!(matches!(agent, Err(SettingsError::Parse(_))));
}

#[test]
fn invalid_agent_defaults_are_errors() {
    let iterations = load_json(
        "agent-iterations-zero",
        r#"{ "agent": { "maxIterations": 0 } }"#,
    );
    let reminder = load_json(
        "agent-reminder-zero",
        r#"{ "agent": { "todoReminderInterval": 0 } }"#,
    );

    assert!(matches!(iterations, Err(SettingsError::Agent(_))));
    assert!(matches!(reminder, Err(SettingsError::Agent(_))));
}

#[test]
fn invalid_compaction_window_is_an_error() {
    let result = load_json(
        "compaction-window-invalid",
        r#"{ "compaction": {
            "mode": "enabled",
            "contextLimit": 8192,
            "reservedOutput": 4096,
            "safetyMargin": 4096
        } }"#,
    );

    assert!(matches!(result, Err(SettingsError::Compaction(_))));
}

#[test]
fn invalid_reserved_output_provider_range_is_an_error() {
    let zero = load_json(
        "compaction-output-zero",
        r#"{ "compaction": {
            "mode": "enabled",
            "contextLimit": 65536,
            "reservedOutput": 0
        } }"#,
    );
    let oversized = load_json(
        "compaction-output-oversized",
        r#"{ "compaction": {
            "mode": "enabled",
            "contextLimit": 8589934592,
            "reservedOutput": 4294967296
        } }"#,
    );

    assert!(matches!(zero, Err(SettingsError::Compaction(_))));
    assert!(matches!(oversized, Err(SettingsError::Compaction(_))));
}

#[test]
fn invalid_compaction_ratios_are_an_error() {
    let result = load_json(
        "compaction-ratios-invalid",
        r#"{ "compaction": {
            "mode": "shadow",
            "contextLimit": 65536,
            "targetRatio": 0.8,
            "softThreshold": 0.7
        } }"#,
    );

    assert!(matches!(result, Err(SettingsError::Compaction(_))));
}

#[test]
fn invalid_summary_budget_is_an_error() {
    let result = load_json(
        "compaction-summary-invalid",
        r#"{ "compaction": {
            "mode": "enabled",
            "contextLimit": 65536,
            "summaryMaxTokens": 0
        } }"#,
    );

    assert!(matches!(result, Err(SettingsError::Compaction(_))));
}

#[test]
fn oversized_summary_budget_is_an_error() {
    let result = load_json(
        "compaction-summary-oversized",
        r#"{ "compaction": {
            "mode": "enabled",
            "contextLimit": 65536,
            "summaryMaxTokens": 4294967296
        } }"#,
    );

    assert!(matches!(result, Err(SettingsError::Compaction(_))));
}

#[test]
fn malformed_json_is_an_error() {
    let result = load_json("malformed", "{ not json");

    assert!(matches!(result, Err(SettingsError::Parse(_))));
}

#[test]
fn bad_rule_is_an_error() {
    let result = load_json(
        "permission-rule-invalid",
        r#"{ "permissions": { "deny": ["Bash("] } }"#,
    );

    assert!(matches!(result, Err(SettingsError::Rule(_, _))));
}
