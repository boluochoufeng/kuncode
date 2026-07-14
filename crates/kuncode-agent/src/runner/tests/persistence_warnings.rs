use super::support::{
    AgentRunner, AgentSession, Arc, AssistantContent, CollectingObserver, EventKind, FakeModel,
    ToolRegistry, response,
};

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
