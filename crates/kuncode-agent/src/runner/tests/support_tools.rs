use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use kuncode_core::completion::{
    AssistantContent, CompletionError, CompletionRequest, CompletionResponse, CompletionStream,
    FinishReason, Message, StreamEvent, ToolDefinition, ToolResultContent, Usage, UserContent,
};
use kuncode_core::non_empty_vec::NonEmptyVec;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{AgentConfig, AgentRunner, TODO_REMINDER, cancellation::cancellable};
use crate::{
    error::AgentError,
    hook::{
        Hook, PostToolCx, PostToolOutcome, PreToolCx, PreToolOutcome, ScriptedHook, StopCx,
        StopOutcome,
    },
    observer::{AgentEvent, AgentObserver, CompositeObserver, EventKind},
    permission::{
        ApprovalOutcome, PermissionAction, PermissionPolicy, PermissionRequest, RuleOrigin,
        ScriptedApprover, parse_rule,
    },
    registry::ToolRegistry,
    session::AgentSession,
    session_store::{NewSession, Seq, SessionId, SessionStore, sqlite::SqliteSessionStore},
    system_prompt::{IdentitySection, SystemPrompt},
    test_support::TestDir,
    tool::{
        Tool, ToolContext, ToolError, ToolOutput, TypedTool, bash::Bash, definition_for,
        todo_write::TodoWrite,
    },
};

/// A tool whose `run` never completes — used to test that a cancellation
/// token interrupts an in-flight tool call. It is a `Read` so the gate
/// allows it straight through to execution with no approval prompt.
struct HangTool {
    definition: ToolDefinition,
}

#[derive(Deserialize, JsonSchema)]
struct HangArgs {}

impl HangTool {
    fn new() -> Self {
        Self {
            definition: definition_for::<HangArgs>("hang", "Never returns"),
        }
    }
}

#[async_trait]
impl TypedTool for HangTool {
    type Args = HangArgs;
    type Output = Value;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn permission(&self, _args: &HangArgs, _ctx: &ToolContext) -> PermissionRequest {
        PermissionRequest::new("hang", PermissionAction::Read, None, "hang")
    }

    async fn run(&self, _args: HangArgs, ctx: &ToolContext) -> ToolOutput<Value> {
        // Cancel from inside the running tool, then never return: this
        // deterministically drives the runner's execute-stage `select!` to
        // the cancellation branch without pre-cancelling the token (which
        // would also race the model stage).
        ctx.cancel.cancel();
        std::future::pending().await
    }
}

/// A model whose `completion` never returns — used to test that a
/// cancellation token interrupts an in-flight *model* request, not only a
/// tool approval/execution.
#[derive(Clone, Default)]
struct HangModel;

impl kuncode_core::completion::CompletionModel for HangModel {
    type Response = Value;
    type Client = ();

    fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
        Self
    }

    async fn completion(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        std::future::pending().await
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionStream, CompletionError> {
        // Hang while establishing the stream so cancellation tests still
        // race a never-resolving model call.
        std::future::pending().await
    }
}

/// Extracts the text of the tool-result user message at `index`.
fn tool_result_text(session: &AgentSession, index: usize) -> String {
    match &session.messages()[index] {
        Message::User { content } => {
            let UserContent::ToolResult(result) = content.first() else {
                panic!("expected tool result content at {index}");
            };
            let ToolResultContent::Text(text) = result.content.first();
            text.text_ref().to_string()
        }
        other => panic!("expected tool result user message at {index}, got {other:?}"),
    }
}

/// Extracts the tool-call id the tool-result user message at `index` answers.
fn tool_result_id(session: &AgentSession, index: usize) -> String {
    match &session.messages()[index] {
        Message::User { content } => {
            let UserContent::ToolResult(result) = content.first() else {
                panic!("expected tool result content at {index}");
            };
            result.id.clone()
        }
        other => panic!("expected tool result user message at {index}, got {other:?}"),
    }
}

async fn bash() -> Bash {
    Bash::from_current_dir()
        .await
        .expect("current directory should be a valid workspace")
}
