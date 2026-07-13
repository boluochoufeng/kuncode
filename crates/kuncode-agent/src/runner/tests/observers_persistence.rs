#[tokio::test]
async fn cancellable_yields_none_when_cancelled_while_pending() {
    let cancel = CancellationToken::new();
    // Cancel from inside the racing future, then never return: the cancel
    // branch is the only one that can resolve, so the race ends in `None`.
    let fut = {
        let cancel = cancel.clone();
        async move {
            cancel.cancel();
            std::future::pending::<i32>().await
        }
    };
    assert_eq!(cancellable(&cancel, fut).await, None);
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
        // A connection-level failure surfaces as the outer `Err`, exactly as
        // `completion` fails.
        Err(CompletionError::ResponseError("boom".to_string()))
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
        EventKind::TextDelta { .. } => "text_delta",
        EventKind::ReasoningDelta { .. } => "reasoning_delta",
        EventKind::Assistant { .. } => "assistant",
        EventKind::ToolStart { .. } => "tool_start",
        EventKind::ToolEnd { .. } => "tool_end",
        EventKind::Error { .. } => "error",
        EventKind::TodoUpdate { .. } => "todo_update",
        EventKind::Warning { .. } => "warning",
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

/// A degraded session store warns exactly once — at the end of the turn
/// whose pushes hit the failure — and never again on later turns (the
/// take-and-clear contract), while the turns themselves stay unaffected.
/// `iteration` is `None`: the failure belongs to no model call.
#[tokio::test]
async fn persistence_failure_emits_warning_once() {
    let model = FakeModel::new([
        response(AssistantContent::text("first")),
        response(AssistantContent::text("second")),
    ]);
    let observer = Arc::new(CollectingObserver::default());
    let runner = AgentRunner::new(model, ToolRegistry::new()).with_observer(observer.clone());
    let mut session = AgentSession::new();
    session.mark_persistence_failed("disk on fire");

    runner
        .run_turn(&mut session, "hi")
        .await
        .expect("first turn should complete despite degraded persistence");
    runner
        .run_turn(&mut session, "again")
        .await
        .expect("second turn should complete");

    let warnings: Vec<_> = observer
        .events()
        .into_iter()
        .filter(|e| matches!(e.kind, EventKind::Warning { .. }))
        .collect();
    assert_eq!(warnings.len(), 1, "one failure, one warning");
    assert!(matches!(
        &warnings[0].kind,
        EventKind::Warning { message } if message.contains("disk on fire")
    ));
    assert_eq!(warnings[0].iteration, None);
}

/// With no observer there is nowhere to deliver the one-shot report, so
/// the runner must NOT drain it — the error stays in the session for a
/// later observer-bearing runner instead of vanishing into a no-op emit.
#[tokio::test]
async fn persistence_failure_survives_observerless_runner() {
    let model = FakeModel::new([response(AssistantContent::text("done"))]);
    let runner = AgentRunner::new(model, ToolRegistry::new());
    let mut session = AgentSession::new();
    session.mark_persistence_failed("disk on fire");

    runner
        .run_turn(&mut session, "hi")
        .await
        .expect("turn should complete");

    assert!(
        session.take_persistence_error().is_some(),
        "the un-reported error must remain takeable"
    );
}
