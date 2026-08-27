//! Task-store tools through the full authorization pipeline.
//!
//! Seed tasks are written with `std::fs` under fixed ids: the scripted
//! [`FakeModel`] cannot reference an id generated at run time.

use std::fs;
use std::path::{Path, PathBuf};

use super::support::{
    AgentRunner, AgentSession, ApproveAll, Arc, AssistantContent, FakeModel, ToolRegistry,
    response, tool_result_text,
};
use crate::{
    permission::PermissionMode,
    tasks::{Task, TaskStatus},
    workspace::Workspace,
};

fn unique_root(tag: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("kuncode-runner-tasks-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    root
}

fn seed(root: &Path, id: &str, status: TaskStatus, blocked_by: &[&str]) {
    fs::create_dir_all(root).expect("tasks root");
    let task = Task {
        id: id.to_string(),
        subject: format!("subject of {id}"),
        description: String::new(),
        status,
        owner: None,
        blocked_by: blocked_by.iter().map(|dep| dep.to_string()).collect(),
    };
    fs::write(
        root.join(format!("{id}.json")),
        serde_json::to_vec_pretty(&task).expect("serializes"),
    )
    .expect("seed task");
}

async fn registry_with_tasks(root: &Path) -> ToolRegistry {
    let workspace = Workspace::from_current_dir()
        .await
        .expect("current directory should be a valid workspace");
    let mut registry =
        ToolRegistry::with_default_workspace_tools(workspace).expect("built-in profiles are valid");
    registry
        .register_task_store_tools(root.to_path_buf())
        .expect("task profiles are valid");
    registry
}

fn claim_call(call_id: &str, task_id: &str) -> AssistantContent {
    AssistantContent::tool_call(
        call_id,
        "claim_task",
        serde_json::json!({ "task_id": task_id }),
    )
}

#[tokio::test]
async fn plan_mode_denies_create_task() {
    let root = unique_root("plan-create");
    let registry = registry_with_tasks(&root).await;

    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_create",
            "create_task",
            serde_json::json!({ "subject": "write the report" }),
        )),
        response(AssistantContent::text("done")),
    ]);
    // An approving resolver proves the denial comes from Plan mode's
    // TaskWrite rule, not from a resolver that never got asked.
    let runner = AgentRunner::new(model, registry).with_approval_resolver(Arc::new(ApproveAll));
    let mut session = AgentSession::with_mode(PermissionMode::Plan);

    let turn = runner
        .run_turn(&mut session, "track this work")
        .await
        .expect("agent run should complete");

    assert_eq!(turn.final_text(&session), "done");
    let result = tool_result_text(&session, 2);
    assert!(
        result.contains("permission_denied"),
        "create result: {result}"
    );
    assert!(!root.exists(), "a plan turn must leave no trace on disk");
}

#[tokio::test]
async fn create_task_needs_no_approval_in_default_mode() {
    let root = unique_root("default-create");
    let registry = registry_with_tasks(&root).await;

    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_create",
            "create_task",
            serde_json::json!({ "subject": "write the report" }),
        )),
        response(AssistantContent::text("created")),
    ]);
    // Deliberately no approval resolver: the broker fails closed, so this
    // passing proves the TaskWrite/Allow profile never reaches a prompt.
    let runner = AgentRunner::new(model, registry);
    let mut session = AgentSession::with_mode(PermissionMode::Default);

    let turn = runner
        .run_turn(&mut session, "track this work")
        .await
        .expect("agent run should complete");
    let stored = fs::read_dir(&root)
        .expect("tasks root exists")
        .filter_map(|entry| entry.ok())
        .count();
    let _ = fs::remove_dir_all(&root);

    assert_eq!(turn.final_text(&session), "created");
    assert_eq!(stored, 1, "exactly one task document on disk");
}

#[tokio::test]
async fn claim_is_blocked_until_the_prerequisite_completes() {
    let root = unique_root("dependency-flow");
    seed(&root, "task_aaaaaaaa", TaskStatus::Pending, &[]);
    seed(
        &root,
        "task_bbbbbbbb",
        TaskStatus::Pending,
        &["task_aaaaaaaa"],
    );
    let registry = registry_with_tasks(&root).await;

    let model = FakeModel::new([
        response(claim_call("call_1", "task_bbbbbbbb")),
        response(claim_call("call_2", "task_aaaaaaaa")),
        response(AssistantContent::tool_call(
            "call_3",
            "complete_task",
            serde_json::json!({ "task_id": "task_aaaaaaaa" }),
        )),
        response(claim_call("call_4", "task_bbbbbbbb")),
        response(AssistantContent::text("all yours")),
    ]);
    let runner = AgentRunner::new(model, registry);
    let mut session = AgentSession::with_mode(PermissionMode::Default);

    let turn = runner
        .run_turn(&mut session, "work through the graph")
        .await
        .expect("agent run should complete");

    assert_eq!(turn.final_text(&session), "all yours");
    // Blocked claim names the incomplete prerequisite.
    let blocked = tool_result_text(&session, 2);
    assert!(blocked.contains("not_claimable"), "blocked: {blocked}");
    assert!(blocked.contains("task_aaaaaaaa"), "blocked: {blocked}");
    // Completion reports the task it unblocked.
    let completed = tool_result_text(&session, 6);
    assert!(completed.contains("task_bbbbbbbb"), "unlocked: {completed}");
    // Final disk state: both claimed by the fixed owner, one completed.
    let a: Task =
        serde_json::from_slice(&fs::read(root.join("task_aaaaaaaa.json")).expect("task a stored"))
            .expect("task a parses");
    let b: Task =
        serde_json::from_slice(&fs::read(root.join("task_bbbbbbbb.json")).expect("task b stored"))
            .expect("task b parses");
    let _ = fs::remove_dir_all(&root);
    assert_eq!(a.status, TaskStatus::Completed);
    assert_eq!(a.owner.as_deref(), Some("agent"));
    assert_eq!(b.status, TaskStatus::InProgress);
    assert_eq!(b.owner.as_deref(), Some("agent"));
}

#[tokio::test]
async fn list_tasks_works_in_plan_mode_without_a_resolver() {
    let root = unique_root("plan-list");
    seed(&root, "task_aaaaaaaa", TaskStatus::Pending, &[]);
    let registry = registry_with_tasks(&root).await;

    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_list",
            "list_tasks",
            serde_json::json!({}),
        )),
        response(AssistantContent::text("done")),
    ]);
    // No resolver: Read/Allow must never prompt, and Plan mode keeps the
    // store readable for read-only turns.
    let runner = AgentRunner::new(model, registry);
    let mut session = AgentSession::with_mode(PermissionMode::Plan);

    let turn = runner
        .run_turn(&mut session, "what work is stored?")
        .await
        .expect("agent run should complete");
    let _ = fs::remove_dir_all(&root);

    assert_eq!(turn.final_text(&session), "done");
    let result = tool_result_text(&session, 2);
    assert!(result.contains("task_aaaaaaaa"), "listing: {result}");
}
