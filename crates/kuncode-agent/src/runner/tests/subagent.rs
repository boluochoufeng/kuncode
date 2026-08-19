//! Delegation through the `task` tool: context isolation, permission
//! inheritance, and usage accounting of nested agent loops.

use super::support::{
    AgentRunner, AgentSession, ApproveAll, Arc, AssistantContent, CollectingObserver, EventKind,
    FakeModel, ToolRegistry, event_label, response, tool_result_text,
};
use crate::{
    agent_type::AgentTypeCatalog,
    permission::PermissionMode,
    system_prompt::{IdentitySection, SystemPrompt},
    workspace::Workspace,
};
use kuncode_core::completion::Message;

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
async fn a_fork_delegation_inherits_the_conversation_without_the_pending_batch() {
    let model = FakeModel::new([
        // Turn 1: an ordinary exchange the fork must inherit.
        response(AssistantContent::text("noted: the answer is 42")),
        // Turn 2: delegate to a fork.
        response(AssistantContent::tool_call(
            "call_task",
            "task",
            serde_json::json!({
                "description": "Recall the answer",
                "prompt": "State the answer we discussed.",
                "agent_type": "fork"
            }),
        )),
        // Subagent: reports from the inherited context.
        response(AssistantContent::text("FORK REPORT: 42")),
        // Parent: final answer.
        response(AssistantContent::text("done")),
    ]);
    let runner = AgentRunner::new(model.clone(), default_registry().await)
        .with_approval_resolver(Arc::new(ApproveAll));
    let mut session = AgentSession::new();

    runner
        .run_turn(&mut session, "remember: the answer is 42")
        .await
        .expect("first turn should complete");
    let turn = runner
        .run_turn(&mut session, "delegate the recall")
        .await
        .expect("delegating turn should complete");

    assert_eq!(turn.final_text(&session), "done");
    let result = tool_result_text(&session, 4);
    assert!(result.contains("FORK REPORT: 42"), "task result: {result}");

    let requests = model.requests();
    assert_eq!(requests.len(), 4);
    // The fork starts from its framing system message plus the parent
    // conversation — both prior turns and the delegating user message — with
    // the in-flight tool batch trimmed and the delegated prompt appended.
    let sub_history = &requests[2].chat_history;
    assert_eq!(sub_history.len(), 5);
    assert!(matches!(
        sub_history.first(),
        Message::System { content } if content.contains("forked subagent")
    ));
    let serialized = serde_json::to_string(sub_history).expect("test history serializes");
    assert!(
        serialized.contains("remember: the answer is 42"),
        "{serialized}"
    );
    assert!(
        serialized.contains("noted: the answer is 42"),
        "{serialized}"
    );
    assert!(
        serialized.contains("State the answer we discussed."),
        "{serialized}"
    );
    // The assistant message carrying the unpaired task call was trimmed.
    assert!(!serialized.contains("call_task"), "{serialized}");
    // A fork still cannot delegate further.
    assert!(!requests[2].tools.iter().any(|tool| tool.name == "task"));
    // The parent transcript keeps only its own messages: two turns of
    // user/assistant plus the paired task result.
    assert_eq!(session.messages().len(), 6);
}

#[tokio::test]
async fn a_custom_agent_type_narrows_tools_and_appends_its_instructions() {
    // A scanned catalog with one custom read-only type.
    let root =
        std::env::temp_dir().join(format!("kuncode-runner-agent-type-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temp dir");
    std::fs::write(
        root.join("explore.md"),
        "---\ndescription: Read-only exploration.\ntools: read_file, grep\n---\nONLY REPORT; NEVER EDIT.",
    )
    .expect("definition");
    let catalog = AgentTypeCatalog::scan(std::slice::from_ref(&root));
    let _ = std::fs::remove_dir_all(&root);
    let mut registry = default_registry().await;
    registry
        .register_task_tool(Arc::new(catalog))
        .expect("task profile is valid");

    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_task",
            "task",
            serde_json::json!({
                "description": "Map the crates",
                "prompt": "List the crates.",
                "agent_type": "explore"
            }),
        )),
        response(AssistantContent::text("EXPLORED")),
        response(AssistantContent::text("done")),
    ]);
    let runner = AgentRunner::new(model.clone(), registry)
        .with_system_prompt(SystemPrompt::new(vec![Box::new(IdentitySection::new(
            "PARENT PROMPT",
        ))]))
        .with_approval_resolver(Arc::new(ApproveAll));
    let mut session = AgentSession::new();

    let turn = runner
        .run_turn(&mut session, "go")
        .await
        .expect("agent run should complete");

    assert_eq!(turn.final_text(&session), "done");
    let requests = model.requests();
    assert_eq!(requests.len(), 3);
    // The subagent sees exactly the whitelisted tools, in registry order.
    let sub_tools: Vec<&str> = requests[1]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(sub_tools, ["read_file", "grep"]);
    // Its system prompt is the parent's whole prompt plus the type's
    // instructions; the parent's own requests never carry those instructions.
    let sub_system = match sub_history_system(&requests[1].chat_history) {
        Some(content) => content,
        None => panic!("subagent request should carry a system message"),
    };
    assert!(sub_system.contains("PARENT PROMPT"), "{sub_system}");
    assert!(
        sub_system.contains("ONLY REPORT; NEVER EDIT."),
        "{sub_system}"
    );
    let parent_system = match sub_history_system(&requests[0].chat_history) {
        Some(content) => content,
        None => panic!("parent request should carry a system message"),
    };
    assert!(!parent_system.contains("ONLY REPORT"), "{parent_system}");
}

/// The system message content of one recorded request, when present.
fn sub_history_system(
    history: &kuncode_core::non_empty_vec::NonEmptyVec<Message>,
) -> Option<String> {
    history.iter().find_map(|message| match message {
        Message::System { content } => Some(content.clone()),
        _ => None,
    })
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
