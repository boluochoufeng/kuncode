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
fn completed_stream<T>(response: CompletionResponse<T>) -> CompletionStream {
    let CompletionResponse { choice, usage, .. } = response;
    Box::pin(futures_util::stream::once(async move {
        Ok(StreamEvent::Completed {
            content: choice,
            usage,
            finish_reason: FinishReason::Stop,
        })
    }))
}

/// A model that streams reasoning + text deltas before the final answer, for
/// asserting the runner forwards render deltas and still finalizes with
/// `Assistant`.
#[derive(Clone)]
struct DeltaModel;

impl kuncode_core::completion::CompletionModel for DeltaModel {
    type Response = Value;
    type Client = ();

    fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
        Self
    }

    async fn completion(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        unimplemented!("delta model only streams")
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionStream, CompletionError> {
        let events = vec![
            Ok(StreamEvent::ReasoningDelta("hmm".to_string())),
            Ok(StreamEvent::TextDelta("Hel".to_string())),
            Ok(StreamEvent::TextDelta("lo".to_string())),
            Ok(StreamEvent::Completed {
                content: NonEmptyVec::new(AssistantContent::text("Hello")),
                usage: Usage::default(),
                finish_reason: FinishReason::Stop,
            }),
        ];
        Ok(Box::pin(futures_util::stream::iter(events)))
    }
}

#[tokio::test]
async fn streaming_forwards_deltas_then_finalizes_with_assistant() {
    let observer = Arc::new(CollectingObserver::default());
    let runner = AgentRunner::new(DeltaModel, ToolRegistry::new()).with_observer(observer.clone());
    let mut session = AgentSession::new();

    runner
        .run_turn(&mut session, "hi")
        .await
        .expect("agent run should complete");

    let events = observer.events();
    let kinds: Vec<_> = events.iter().map(|e| event_label(&e.kind)).collect();
    assert_eq!(
        kinds,
        vec![
            "model_start",
            "reasoning_delta",
            "text_delta",
            "text_delta",
            "assistant",
        ],
    );

    // Deltas carry the streamed fragments; the final Assistant carries the
    // authoritative assembled text.
    let text_deltas: Vec<&str> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text_deltas, ["Hel", "lo"]);
    assert!(matches!(
        &events[4].kind,
        EventKind::Assistant { text, tool_calls } if text == "Hello" && tool_calls.is_empty()
    ));
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
