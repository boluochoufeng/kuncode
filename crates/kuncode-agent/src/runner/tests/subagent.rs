//! Delegation through the `task` tool: context isolation, permission
//! inheritance, and usage accounting of nested agent loops.

use super::support::{
    AgentRunner, AgentSession, ApproveAll, Arc, AssistantContent, CollectingObserver, EventKind,
    FakeModel, ToolRegistry, event_label, response, tool_result_text,
};
use crate::{permission::PermissionMode, workspace::Workspace};

async fn default_registry() -> ToolRegistry {
    let workspace = Workspace::from_current_dir()
        .await
        .expect("current directory should be a valid workspace");
    ToolRegistry::with_default_workspace_tools(workspace).expect("built-in profiles are valid")
}

fn task_call(id: &str) -> AssistantContent {
    AssistantContent::tool_call(
        id,
        "task",
        serde_json::json!({
            "description": "Inspect workspace",
            "prompt": "Count the crates and report their names."
        }),
    )
}

#[tokio::test]
async fn task_runs_a_nested_loop_and_only_its_report_reaches_the_parent() {
    let model = FakeModel::new([
        // Parent: delegate.
        response(task_call("call_task")),
        // Subagent: one tool call, then its report.
        response(AssistantContent::tool_call(
            "call_sub_bash",
            "bash",
            serde_json::json!({ "cmd": "printf sub" }),
        )),
        response(AssistantContent::text("SUB REPORT")),
        // Parent: final answer.
        response(AssistantContent::text("done")),
    ]);
    let observer = Arc::new(CollectingObserver::default());
    let runner = AgentRunner::new(model.clone(), default_registry().await)
        .with_approval_resolver(Arc::new(ApproveAll))
        .with_observer(observer.clone());
    let mut session = AgentSession::new();

    let turn = runner
        .run_turn(&mut session, "go")
        .await
        .expect("agent run should complete");

    assert_eq!(turn.final_text(&session), "done");
    // The parent transcript holds only its own four messages; nothing of the
    // subagent's conversation leaked in.
    assert_eq!(session.messages().len(), 4);
    let result = tool_result_text(&session, 2);
    assert!(result.contains("SUB REPORT"), "task result: {result}");
    assert!(result.contains("\"iterations\":2"), "task result: {result}");

    // Four model calls total; the parent turn's usage covers all of them
    // (each scripted response reports total_tokens = 3).
    assert_eq!(turn.iterations, 2);
    assert_eq!(turn.usage.total_tokens, 12);

    let requests = model.requests();
    assert_eq!(requests.len(), 4);
    // The subagent starts from a fresh transcript holding only the prompt.
    assert_eq!(requests[1].chat_history.len(), 1);
    // Its tool list is the parent's minus `task`: no infinite delegation.
    let sub_tools: Vec<&str> = requests[1]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert!(!sub_tools.contains(&"task"));
    assert!(sub_tools.contains(&"bash"));
    let parent_tools: Vec<&str> = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert!(parent_tools.contains(&"task"));
    // The parent's follow-up request carries the paired task result, not the
    // subagent's intermediate messages.
    assert_eq!(requests[3].chat_history.len(), 3);

    // The frontend sees the delegation as one ordinary tool call; none of the
    // subagent's own model/tool events surface.
    let events = observer.events();
    let labels: Vec<_> = events.iter().map(|e| event_label(&e.kind)).collect();
    assert_eq!(
        labels,
        vec![
            "model_start",
            "assistant",
            "tool_start",
            "tool_end",
            "model_start",
            "assistant",
        ],
    );
    assert!(matches!(
        &events[2].kind,
        EventKind::ToolStart { tool, summary, .. }
            if tool == "task" && summary == "Task: Inspect workspace"
    ));
}

#[tokio::test]
async fn plan_mode_denies_delegation_itself() {
    let model = FakeModel::new([
        response(task_call("call_task")),
        response(AssistantContent::text("done")),
    ]);
    let runner = AgentRunner::new(model.clone(), default_registry().await)
        .with_approval_resolver(Arc::new(ApproveAll));
    let mut session = AgentSession::with_mode(PermissionMode::Plan);

    let turn = runner
        .run_turn(&mut session, "go")
        .await
        .expect("agent run should complete");

    assert_eq!(turn.final_text(&session), "done");
    let result = tool_result_text(&session, 2);
    assert!(
        result.contains("permission_denied"),
        "task result: {result}"
    );
    // No subagent model call happened: both requests belong to the parent.
    assert_eq!(model.requests().len(), 2);
}

#[tokio::test]
async fn subagent_inherits_the_parent_permission_mode() {
    let model = FakeModel::new([
        response(task_call("call_task")),
        // Subagent tries bash; inherited dont-ask must deny it without
        // consulting the (approving) resolver.
        response(AssistantContent::tool_call(
            "call_sub_bash",
            "bash",
            serde_json::json!({ "cmd": "printf sub" }),
        )),
        response(AssistantContent::text("could not run bash")),
        response(AssistantContent::text("done")),
    ]);
    let runner = AgentRunner::new(model.clone(), default_registry().await)
        .with_approval_resolver(Arc::new(ApproveAll));
    let mut session = AgentSession::with_mode(PermissionMode::DontAsk);

    let turn = runner
        .run_turn(&mut session, "go")
        .await
        .expect("agent run should complete");

    assert_eq!(turn.final_text(&session), "done");
    let requests = model.requests();
    assert_eq!(requests.len(), 4);
    // The subagent's second request pairs its bash call with a denial: the
    // parent's mode reached the nested loop. Were the mode not inherited, the
    // ApproveAll resolver would have let bash execute.
    let sub_followup = &requests[2].chat_history;
    let denied = sub_followup.iter().any(|message| {
        serde_json::to_string(message)
            .expect("test message serializes")
            .contains("permission_denied")
    });
    assert!(denied, "sub follow-up: {sub_followup:?}");
}
