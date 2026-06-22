//! The `todo_write` tool: maintain the session task plan.
//!
//! Whole-list overwrite: each call submits the complete plan, replacing the
//! previous one. The tool holds no state of its own — it writes through the
//! [`TodoHandle`](crate::todo::TodoHandle) on the [`ToolContext`], which the
//! runner wires to the current session's plan — so a single registered instance
//! is safely shared across sessions.

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    permission::{PermissionAction, PermissionRequest},
    todo::TodoItem,
    tool::{ToolContext, ToolOutput, TypedTool, definition_for},
};

const DESCRIPTION: &str = "\
Create and manage a structured task plan for the current coding session. Use it \
to break a multi-step task into ordered steps and track progress as you go.

Submit the COMPLETE list every call: it replaces the previous plan wholesale \
(an empty list clears it). Each item has `content` (imperative, e.g. \"Add \
tests\"), `active_form` (present continuous shown while running, e.g. \"Adding \
tests\"), and `status` (pending | in_progress | completed). Keep at most one \
item in_progress at a time; mark a step completed before starting the next.";

/// Arguments for [`TodoWrite`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoWriteArgs {
    /// The complete task plan. Replaces the previous plan entirely.
    pub todos: Vec<TodoItem>,
}

/// Confirmation echoed back to the model: the plan as stored after the write.
#[derive(Debug, Serialize)]
pub struct TodoWriteOutput {
    /// The stored plan after this overwrite.
    pub todos: Vec<TodoItem>,
}

/// Overwrites the session task plan. See the [module docs](self).
#[derive(Clone, Debug)]
pub struct TodoWrite {
    definition: ToolDefinition,
}

impl TodoWrite {
    /// Creates the tool. Holds only its cached definition; the plan it writes
    /// lives on the session, reached via [`ToolContext::todos`].
    pub fn new() -> Self {
        Self {
            definition: definition_for::<TodoWriteArgs>("todo_write", DESCRIPTION),
        }
    }
}

impl Default for TodoWrite {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TypedTool for TodoWrite {
    type Args = TodoWriteArgs;
    type Output = TodoWriteOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn permission(&self, args: &TodoWriteArgs, _ctx: &ToolContext) -> PermissionRequest {
        // `Meta`: writes only the in-memory session plan, no external side
        // effect, so it is allow-by-default with nothing to scope a rule to.
        PermissionRequest::new(
            "todo_write",
            PermissionAction::Meta,
            None,
            format!("Update task plan ({} tasks)", args.todos.len()),
        )
    }

    async fn run(&self, args: TodoWriteArgs, ctx: &ToolContext) -> ToolOutput<TodoWriteOutput> {
        match ctx.todos.replace(args.todos) {
            // Echo the stored plan so the model gets an explicit confirmation of
            // the current state it can reason about next.
            Ok(()) => ToolOutput::success(TodoWriteOutput {
                todos: ctx.todos.snapshot(),
            }),
            // Validation failures are model-recoverable: report them so the model
            // can fix the list and resubmit.
            Err(err) => ToolOutput::failure("invalid_arguments", err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::{TodoHandle, TodoStatus};
    use crate::tool::Tool;

    fn ctx_with_plan() -> (ToolContext, TodoHandle) {
        let handle = TodoHandle::default();
        let ctx = ToolContext::new().with_todos(handle.clone());
        (ctx, handle)
    }

    #[tokio::test]
    async fn overwrites_session_plan_and_echoes_it() {
        let (ctx, handle) = ctx_with_plan();
        let tool = TodoWrite::new();

        let output = tool
            .call(
                serde_json::json!({
                    "todos": [
                        { "content": "Step one", "active_form": "Doing step one", "status": "completed" },
                        { "content": "Step two", "active_form": "Doing step two", "status": "in_progress" }
                    ]
                }),
                &ctx,
            )
            .await
            .expect("no harness error");

        assert!(output.ok);
        // The write landed on the session handle the runner would read.
        let stored = handle.snapshot();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[1].status, TodoStatus::InProgress);
        // The output echoes the stored plan.
        let data = output.data.expect("data present");
        assert_eq!(data["todos"][0]["content"], "Step one");
        assert_eq!(handle.generation(), 1);
    }

    #[tokio::test]
    async fn rejects_two_in_progress_without_touching_the_plan() {
        let (ctx, handle) = ctx_with_plan();
        let tool = TodoWrite::new();

        let output = tool
            .call(
                serde_json::json!({
                    "todos": [
                        { "content": "a", "active_form": "a…", "status": "in_progress" },
                        { "content": "b", "active_form": "b…", "status": "in_progress" }
                    ]
                }),
                &ctx,
            )
            .await
            .expect("no harness error");

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "invalid_arguments"
        );
        // The rejected write left the plan empty and the generation at zero.
        assert!(handle.snapshot().is_empty());
        assert_eq!(handle.generation(), 0);
    }

    #[tokio::test]
    async fn empty_list_clears_the_plan() {
        let (ctx, handle) = ctx_with_plan();
        let tool = TodoWrite::new();
        handle
            .replace(vec![TodoItem {
                content: "old".to_string(),
                active_form: "old…".to_string(),
                status: TodoStatus::Pending,
            }])
            .expect("seed plan");

        let output = tool
            .call(serde_json::json!({ "todos": [] }), &ctx)
            .await
            .expect("no harness error");

        assert!(output.ok);
        assert!(handle.snapshot().is_empty());
    }

    #[test]
    fn permission_is_meta_and_unscoped() {
        let tool = TodoWrite::new();
        let prepared = Tool::prepare(
            &tool,
            serde_json::json!({ "todos": [] }),
            &ToolContext::new(),
        )
        .expect("valid args");
        assert_eq!(prepared.request.action, PermissionAction::Meta);
        assert!(prepared.request.resource.is_none());
    }
}
