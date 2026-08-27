//! Memory tools through the full authorization pipeline.

use std::fs;
use std::path::{Path, PathBuf};

use super::support::{
    AgentRunner, AgentSession, ApproveAll, Arc, AssistantContent, FakeModel, ToolRegistry,
    response, tool_result_text,
};
use crate::{memory::document_path, permission::PermissionMode, workspace::Workspace};

fn unique_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "kuncode-runner-memory-{}-{tag}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    root
}

async fn registry_with_memory(root: &Path) -> ToolRegistry {
    let workspace = Workspace::from_current_dir()
        .await
        .expect("current directory should be a valid workspace");
    let mut registry =
        ToolRegistry::with_default_workspace_tools(workspace).expect("built-in profiles are valid");
    registry
        .register_memory_tools(root.to_path_buf())
        .expect("memory profiles are valid");
    registry
}

#[tokio::test]
async fn load_memory_needs_no_approval_and_still_works_in_plan_mode() {
    let root = unique_root("load");
    fs::create_dir_all(&root).expect("memory root");
    fs::write(
        document_path(&root, "terse"),
        "---\nname: terse\ndescription: Style preference.\n---\nKeep answers short.",
    )
    .expect("memory document");
    let registry = registry_with_memory(&root).await;

    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_memory",
            "load_memory",
            serde_json::json!({ "name": "terse" }),
        )),
        response(AssistantContent::text("done")),
    ]);
    // Deliberately no approval resolver: the broker fails closed, so this
    // passing proves the Read/Allow profile never reaches an approval prompt —
    // and Plan mode proves memory recall stays available to read-only turns.
    let runner = AgentRunner::new(model.clone(), registry);
    let mut session = AgentSession::with_mode(PermissionMode::Plan);

    let turn = runner
        .run_turn(&mut session, "how should I answer?")
        .await
        .expect("agent run should complete");
    let _ = fs::remove_dir_all(&root);

    assert_eq!(turn.final_text(&session), "done");
    let result = tool_result_text(&session, 2);
    assert!(
        result.contains("Keep answers short."),
        "memory result: {result}"
    );
}

#[tokio::test]
async fn plan_mode_denies_write_memory() {
    let root = unique_root("plan-write");
    let registry = registry_with_memory(&root).await;

    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_memory",
            "write_memory",
            serde_json::json!({
                "name": "terse",
                "description": "Style preference.",
                "body": "Keep answers short.",
            }),
        )),
        response(AssistantContent::text("done")),
    ]);
    // An approving resolver proves the denial comes from Plan mode's Edit
    // rule, not from a resolver that never got asked.
    let runner = AgentRunner::new(model, registry).with_approval_resolver(Arc::new(ApproveAll));
    let mut session = AgentSession::with_mode(PermissionMode::Plan);

    let turn = runner
        .run_turn(&mut session, "remember this")
        .await
        .expect("agent run should complete");

    assert_eq!(turn.final_text(&session), "done");
    let result = tool_result_text(&session, 2);
    assert!(
        result.contains("permission_denied"),
        "write result: {result}"
    );
    assert!(!document_path(&root, "terse").exists());
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn write_memory_round_trips_with_approval() {
    let root = unique_root("approved-write");
    let registry = registry_with_memory(&root).await;

    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_memory",
            "write_memory",
            serde_json::json!({
                "name": "terse",
                "description": "Style preference.",
                "type": "user",
                "body": "Keep answers short.",
            }),
        )),
        response(AssistantContent::text("saved")),
    ]);
    let runner = AgentRunner::new(model, registry).with_approval_resolver(Arc::new(ApproveAll));
    let mut session = AgentSession::with_mode(PermissionMode::Default);

    let turn = runner
        .run_turn(&mut session, "remember this")
        .await
        .expect("agent run should complete");

    assert_eq!(turn.final_text(&session), "saved");
    let stored = fs::read_to_string(document_path(&root, "terse")).expect("document stored");
    let _ = fs::remove_dir_all(&root);
    let parsed = crate::frontmatter::parse(&stored);
    assert_eq!(parsed.field("name"), Some("terse"));
    assert_eq!(parsed.field("type"), Some("user"));
    assert_eq!(parsed.body(), "Keep answers short.\n");
}
