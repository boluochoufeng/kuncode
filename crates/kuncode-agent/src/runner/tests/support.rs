pub(super) use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

pub(super) use async_trait::async_trait;
pub(super) use kuncode_core::completion::{
    AssistantContent, CompletionError, CompletionRequest, CompletionResponse, CompletionStream,
    FinishReason, Message, StreamEvent, ToolDefinition, ToolResultContent, Usage, UserContent,
};
pub(super) use kuncode_core::non_empty_vec::NonEmptyVec;
pub(super) use schemars::JsonSchema;
pub(super) use serde::Deserialize;
pub(super) use serde_json::Value;
pub(super) use tokio_util::sync::CancellationToken;

pub(super) use super::super::{
    AgentCompactionConfig, AgentConfig, AgentRunner, TODO_REMINDER, cancellation::cancellable,
};
pub(super) use crate::{
    compaction::{
        CompactionError, GroupTokenEstimator,
        budget::{
            CompactionConfig, CompactionMode, TokenCountPrecision, TokenEstimate,
            TokenEstimationError, TokenEstimator,
        },
        protocol::ProtocolGroup,
    },
    error::AgentError,
    hook::{
        AuthorizationHookFailure, Hook, PostToolCx, PostToolOutcome, PreToolCx, PreToolOutcome,
        ScriptedHook, StopCx, StopOutcome,
    },
    observer::{AgentEvent, AgentObserver, CompositeObserver, EventKind},
    permission::{
        ApprovalChallenge, ApprovalResolution, ApprovalResolver, CanonicalPath, CanonicalToolInput,
        PermissionCheckSpec, PermissionNamespace, PermissionTarget, PolicyEffect, PolicyOrigin,
        PolicyScope, PolicySet, ProfileDefault, ScriptedApprovalResolver, ToolDisplay,
        ToolPermissionProfile,
    },
    registry::ToolRegistry,
    session::AgentSession,
    session_store::{NewSession, Seq, SessionId, SessionStore, turso::TursoSessionStore},
    system_prompt::{IdentitySection, SystemPrompt},
    test_support::TestDir,
    tool::{
        ExecutedInvocation, PreparationContext, PreparedInvocation, Tool, ToolContext, ToolError,
        ToolOutput, ToolPreparation, TypedPreparation, TypedTool, bash::Bash, definition_for,
        exact_typed_preparation, todo_write::TodoWrite,
    },
};

/// A tool whose `run` never completes — used to test that a cancellation
/// token interrupts an in-flight tool call. It is a `Read` so the gate
/// allows it straight through to execution with no approval prompt.
pub(super) struct HangTool {
    definition: ToolDefinition,
}

#[derive(Deserialize, JsonSchema)]
pub(super) struct HangArgs {}

impl HangTool {
    pub(super) fn new() -> Self {
        Self {
            definition: definition_for::<HangArgs>("hang", "Never returns"),
        }
    }
}

#[async_trait]
impl TypedTool for HangTool {
    type Args = HangArgs;
    type Prepared = HangArgs;
    type Output = Value;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare_typed(
        &self,
        args: HangArgs,
        canonical_input: CanonicalToolInput,
        _ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
        exact_typed_preparation(
            "hang",
            args,
            canonical_input,
            ToolDisplay::new("Run hanging test tool"),
        )
    }

    async fn run_prepared(&self, _prepared: HangArgs, ctx: &ToolContext) -> ToolOutput<Value> {
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
pub(super) struct HangModel;

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
pub(super) fn tool_result_text(session: &AgentSession, index: usize) -> String {
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
pub(super) fn tool_result_id(session: &AgentSession, index: usize) -> String {
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

pub(super) async fn bash() -> Bash {
    Bash::from_current_dir()
        .await
        .expect("current directory should be a valid workspace")
}

/// Registers the built-in shell adapter with its trusted namespace profile.
pub(super) async fn register_bash(registry: &mut ToolRegistry) {
    registry
        .register_with_profile(
            bash().await,
            ToolPermissionProfile::new(
                "bash",
                [(PermissionNamespace::Bash, ProfileDefault::RequireApproval)],
                true,
            )
            .expect("valid bash profile"),
        )
        .expect("bash registration succeeds");
}

/// Registers the task-plan tool with its trusted allow-by-default profile.
pub(super) fn register_todo(registry: &mut ToolRegistry) {
    registry
        .register_with_profile(
            TodoWrite::new(),
            ToolPermissionProfile::new(
                "todo_write",
                [(PermissionNamespace::TodoWrite, ProfileDefault::Allow)],
                false,
            )
            .expect("valid todo profile"),
        )
        .expect("todo registration succeeds");
}

/// Approval resolver used only by tests that are about non-permission behavior.
pub(super) struct ApproveAll;

#[async_trait]
impl ApprovalResolver for ApproveAll {
    async fn resolve(&self, _challenge: &ApprovalChallenge) -> ApprovalResolution {
        ApprovalResolution::Approve { persistence: None }
    }
}

/// Persists the engine's first exact session-Allow template, then denies any
/// unexpected later prompt so tests can prove the mutation was sufficiently narrow.
#[derive(Default)]
pub(super) struct RememberExactOnce {
    calls: AtomicUsize,
}

impl RememberExactOnce {
    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ApprovalResolver for RememberExactOnce {
    async fn resolve(&self, challenge: &ApprovalChallenge) -> ApprovalResolution {
        if self.calls.fetch_add(1, Ordering::SeqCst) != 0 {
            return ApprovalResolution::Deny { persistence: None };
        }
        let persistence = challenge
            .mutation_options()
            .iter()
            .find(|option| {
                option.effect() == PolicyEffect::Allow && option.scope() == PolicyScope::Session
            })
            .map(|option| option.id().clone());
        ApprovalResolution::Approve { persistence }
    }
}

/// Canonical workspace anchor shared by typed-policy runner tests.
pub(super) fn workspace_root() -> CanonicalPath {
    let current = std::env::current_dir().expect("test current directory");
    let canonical = std::fs::canonicalize(current).expect("canonical test current directory");
    CanonicalPath::from_absolute(&canonical).expect("UTF-8 absolute test root")
}

/// Empty typed policy anchored to the active test workspace.
pub(super) fn empty_policy() -> PolicySet {
    PolicySet::new(workspace_root())
}

#[derive(Clone, Default)]
pub(super) struct FakeModel {
    responses: Arc<Mutex<VecDeque<CompletionResponse<Value>>>>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl FakeModel {
    pub(super) fn new(responses: impl IntoIterator<Item = CompletionResponse<Value>>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into_iter().collect())),
            requests: Arc::default(),
        }
    }

    pub(super) fn requests(&self) -> Vec<CompletionRequest> {
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
        request: CompletionRequest,
    ) -> Result<CompletionStream, CompletionError> {
        // Mirror `completion`: record the request, pop the queued response,
        // and replay it as a single terminal `Completed` event so the runner
        // exercises its streaming path against the same scripted responses.
        self.requests.lock().expect("requests lock").push(request);
        let response = self
            .responses
            .lock()
            .expect("responses lock")
            .pop_front()
            .expect("fake response queued");
        Ok(completed_stream(response))
    }
}

/// Replays a [`CompletionResponse`] as a one-event stream ending in
/// [`StreamEvent::Completed`], for test models that script whole responses.
/// `finish_reason` is irrelevant — the runner branches on the content.
pub(super) fn completed_stream<T>(response: CompletionResponse<T>) -> CompletionStream {
    let CompletionResponse { choice, usage, .. } = response;
    Box::pin(futures_util::stream::once(async move {
        Ok(StreamEvent::Completed {
            content: choice,
            usage,
            finish_reason: FinishReason::Stop,
        })
    }))
}

pub(super) fn response(content: AssistantContent) -> CompletionResponse<Value> {
    response_many(vec![content])
}

/// A response whose assistant message carries several content blocks (e.g.
/// multiple tool calls in one turn).
pub(super) fn response_many(contents: Vec<AssistantContent>) -> CompletionResponse<Value> {
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

/// Records every event so a test can assert on the full stream.
#[derive(Default)]
pub(super) struct CollectingObserver {
    events: Mutex<Vec<AgentEvent>>,
}

impl AgentObserver for CollectingObserver {
    fn on_event(&self, event: &AgentEvent) {
        self.events.lock().expect("events lock").push(event.clone());
    }
}

impl CollectingObserver {
    pub(super) fn events(&self) -> Vec<AgentEvent> {
        self.events.lock().expect("events lock").clone()
    }
}

/// An observer that always panics, to prove the composite isolates it.
pub(super) struct PanicObserver;

impl AgentObserver for PanicObserver {
    fn on_event(&self, _event: &AgentEvent) {
        panic!("observer blew up");
    }
}

/// A model whose `completion` fails, to exercise the model-stage error path.
#[derive(Clone, Default)]
pub(super) struct ErrModel;

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
        // A connection-level failure surfaces as the outer `Err`, exactly as
        // `completion` fails.
        Err(CompletionError::ResponseError("boom".to_string()))
    }
}

/// A raw [`Tool`] whose `call` returns a harness-level [`ToolError`] — the
/// `AgentError::Tool` path, distinct from a model-recoverable failure. A
/// `Read` action so the gate lets it through to execution unprompted.
pub(super) struct BrokenTool {
    definition: ToolDefinition,
}

impl BrokenTool {
    pub(super) fn new() -> Self {
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

    async fn prepare(
        self: Arc<Self>,
        args: Value,
        _ctx: &PreparationContext,
    ) -> Result<ToolPreparation, ToolOutput> {
        let target = PermissionTarget::exact_tool("broken")
            .map_err(|error| ToolOutput::failure("invalid_arguments", error.to_string()))?;
        Ok(ToolPreparation::new(
            CanonicalToolInput::new(args),
            Box::new(BrokenInvocation),
            NonEmptyVec::new(PermissionCheckSpec::new(target)),
            ToolDisplay::new("Run broken test tool"),
        ))
    }
}

struct BrokenInvocation;

#[async_trait]
impl PreparedInvocation for BrokenInvocation {
    async fn execute(self: Box<Self>, _ctx: &ToolContext) -> Result<ExecutedInvocation, ToolError> {
        Err(ToolError::Internal("kaboom".to_string()))
    }
}

/// Stable label for an [`EventKind`], for asserting on the sequence shape.
pub(super) fn event_label(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::ModelStart => "model_start",
        EventKind::TextDelta { .. } => "text_delta",
        EventKind::ReasoningDelta { .. } => "reasoning_delta",
        EventKind::Assistant { .. } => "assistant",
        EventKind::ToolStart { .. } => "tool_start",
        EventKind::ToolEnd { .. } => "tool_end",
        EventKind::Error { .. } => "error",
        EventKind::TodoUpdate { .. } => "todo_update",
        EventKind::Warning { .. } => "warning",
        EventKind::CompactionStarted { .. } => "compaction_started",
        EventKind::CompactionCompleted { .. } => "compaction_completed",
        EventKind::CompactionSkipped { .. } => "compaction_skipped",
        EventKind::CompactionObserved { .. } => "compaction_observed",
        EventKind::CompactionFailed { .. } => "compaction_failed",
    }
}

/// The tool_call ids the transcript's tool_result messages answer, in order.
pub(super) fn tool_result_ids(session: &AgentSession) -> Vec<String> {
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

pub(super) fn configured_runner(model: FakeModel, mode: CompactionMode) -> AgentRunner<FakeModel> {
    let policy =
        CompactionConfig::new(mode, 1_000, 100, 0).expect("test context window should be valid");
    let compaction = AgentCompactionConfig::new(policy, "test-model", 128)
        .expect("test compaction runtime should be valid");
    AgentRunner::with_config(
        model,
        ToolRegistry::new(),
        AgentConfig {
            max_tokens: Some(100),
            compaction: Some(compaction),
            ..AgentConfig::default()
        },
    )
}

pub(super) struct FixedRunnerGroupEstimator(pub(super) u64);

#[async_trait]
impl GroupTokenEstimator for FixedRunnerGroupEstimator {
    async fn estimate(&self, _group: &ProtocolGroup) -> Result<u64, CompactionError> {
        Ok(self.0)
    }
}

#[derive(Default)]
pub(super) struct RequestShapeEstimator {
    calls: AtomicUsize,
}

impl RequestShapeEstimator {
    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl TokenEstimator for RequestShapeEstimator {
    async fn estimate(
        &self,
        request: &CompletionRequest,
    ) -> Result<TokenEstimate, TokenEstimationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let tokens = if request
            .chat_history
            .iter()
            .any(crate::compaction::summary::is_compacted_context_message)
        {
            300
        } else {
            700
        };
        Ok(TokenEstimate::new(tokens, TokenCountPrecision::Exact))
    }
}

pub(super) const LARGE_RESULT_BYTES: usize = 9_500;

#[derive(Default)]
pub(super) struct ScriptedRequestEstimator;

#[async_trait]
impl TokenEstimator for ScriptedRequestEstimator {
    async fn estimate(
        &self,
        request: &CompletionRequest,
    ) -> Result<TokenEstimate, TokenEstimationError> {
        let mut result_count = 0_u64;
        let mut marker = false;
        let mut large_result = false;
        for message in request.chat_history.iter() {
            let Message::User { content } = message else {
                continue;
            };
            for result in content.iter().filter_map(|block| match block {
                UserContent::ToolResult(result) => Some(result),
                UserContent::Text(_) => None,
            }) {
                result_count += 1;
                let ToolResultContent::Text(text) = result.content.first();
                large_result |= text.text_ref().len() > 8_192;
                marker |= serde_json::from_str::<Value>(text.text_ref())
                    .ok()
                    .is_some_and(|value| value.get("artifact_id").is_some());
            }
        }
        let tokens = if marker {
            300
        } else if request.chat_history.len() == 1 && result_count == 1 && large_result {
            9_000
        } else {
            match result_count {
                0 | 1 => 300,
                _ => 700,
            }
        };
        Ok(TokenEstimate::new(tokens, TokenCountPrecision::Exact))
    }
}

pub(super) struct CountingPostHook(pub(super) Arc<AtomicUsize>);

#[async_trait]
impl Hook for CountingPostHook {
    async fn post_tool_use(&self, _cx: &PostToolCx<'_>) -> PostToolOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        PostToolOutcome::Proceed
    }
}

pub(super) struct CancelInPreHook(pub(super) CancellationToken);

#[async_trait]
impl Hook for CancelInPreHook {
    async fn pre_tool_use(
        &self,
        _cx: &PreToolCx<'_>,
    ) -> Result<PreToolOutcome, AuthorizationHookFailure> {
        self.0.cancel();
        std::future::pending().await
    }
}

pub(super) struct CancelInPostHook(pub(super) CancellationToken);

#[async_trait]
impl Hook for CancelInPostHook {
    async fn post_tool_use(&self, _cx: &PostToolCx<'_>) -> PostToolOutcome {
        self.0.cancel();
        std::future::pending().await
    }
}

pub(super) struct AlwaysContinueHook;

#[async_trait]
impl Hook for AlwaysContinueHook {
    async fn stop(&self, _cx: &StopCx<'_>) -> StopOutcome {
        StopOutcome::Continue {
            message: "keep going".to_string(),
        }
    }
}

pub(super) fn user_text(session: &AgentSession, index: usize) -> Option<String> {
    match &session.messages()[index] {
        Message::User { content } => match content.first() {
            UserContent::Text(text) => Some(text.text_ref().to_string()),
            UserContent::ToolResult(_) => None,
        },
        _ => None,
    }
}

pub(super) fn is_tool_result(session: &AgentSession, index: usize) -> bool {
    matches!(
        &session.messages()[index],
        Message::User { content } if matches!(content.first(), UserContent::ToolResult(_))
    )
}

pub(super) fn reminder_count(session: &AgentSession) -> usize {
    session
        .messages()
        .iter()
        .filter(|message| match message {
            Message::User { content } => matches!(
                content.first(),
                UserContent::Text(text) if text.text_ref() == TODO_REMINDER
            ),
            _ => false,
        })
        .count()
}
