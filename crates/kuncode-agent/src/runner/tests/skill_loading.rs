//! On-demand skill loading through the full authorization pipeline.

use std::fs;

use super::support::{
    AgentRunner, AgentSession, Arc, AssistantContent, FakeModel, ToolRegistry, response,
    tool_result_text,
};
use crate::{permission::PermissionMode, skill::SkillCatalog, workspace::Workspace};

#[tokio::test]
async fn load_skill_needs_no_approval_and_still_works_in_plan_mode() {
    let root = std::env::temp_dir().join(format!("kuncode-runner-skill-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("deploy")).expect("skill dir");
    fs::write(
        root.join("deploy").join("SKILL.md"),
        "---\nname: deploy\ndescription: Ship safely.\n---\nAlways run tests first.",
    )
    .expect("skill document");
    let catalog = SkillCatalog::scan(std::slice::from_ref(&root));

    let workspace = Workspace::from_current_dir()
        .await
        .expect("current directory should be a valid workspace");
    let mut registry =
        ToolRegistry::with_default_workspace_tools(workspace).expect("built-in profiles are valid");
    registry
        .register_skill_tool(Arc::new(catalog))
        .expect("skill profile is valid");

    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_skill",
            "load_skill",
            serde_json::json!({ "name": "deploy" }),
        )),
        response(AssistantContent::text("done")),
    ]);
    // Deliberately no approval resolver: the broker fails closed, so this
    // passing proves the Read/Allow profile never reaches an approval prompt —
    // and Plan mode proves skill reading stays available to read-only turns.
    let runner = AgentRunner::new(model.clone(), registry);
    let mut session = AgentSession::with_mode(PermissionMode::Plan);

    let turn = runner
        .run_turn(&mut session, "prepare the deploy")
        .await
        .expect("agent run should complete");
    let _ = fs::remove_dir_all(&root);

    assert_eq!(turn.final_text(&session), "done");
    let result = tool_result_text(&session, 2);
    assert!(
        result.contains("Always run tests first."),
        "skill result: {result}"
    );
}
