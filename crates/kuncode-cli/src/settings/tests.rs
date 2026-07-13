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
    let result = load_project_settings(&dir);
    let _ = fs::remove_dir_all(&dir);
    result
}

#[test]
fn missing_file_is_default_not_error() {
    let dir = std::env::temp_dir().join(format!("kuncode-absent-{}", std::process::id()));

    let loaded = load_project_settings(&dir).expect("a missing file is fine");

    assert!(loaded.policy.deny.is_empty());
    assert!(loaded.default_mode.is_none());
    assert!(loaded.compaction.is_none());
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
        r#"{ "compaction": { "mode": "shadow", "contextLimit": 131072 } }"#,
    )
    .expect("loads");

    let compaction = loaded.compaction.expect("compaction enabled");
    assert_eq!(compaction.policy.mode(), CompactionMode::Shadow);
    assert_eq!(compaction.policy.context_limit(), 131_072);
    assert_eq!(compaction.policy.reserved_output(), 32_768);
    assert_eq!(compaction.policy.safety_margin(), 4_096);
    assert_eq!(compaction.policy.target_ratio(), 0.50);
    assert_eq!(compaction.policy.soft_threshold(), 0.75);
    assert_eq!(compaction.policy.hard_threshold(), 0.90);
    assert_eq!(compaction.policy.recent_ratio(), 0.10);
    assert_eq!(compaction.summary_max_tokens.get(), 4_096);
}

#[test]
fn enabled_compaction_loads_all_camel_case_fields() {
    let loaded = load_json(
        "compaction-full",
        r#"{ "compaction": {
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
fn active_compaction_requires_context_limit() {
    let result = load_json(
        "compaction-context-missing",
        r#"{ "compaction": { "mode": "enabled" } }"#,
    );

    assert!(matches!(result, Err(SettingsError::CompactionContextLimit)));
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
