use super::AgentCompactionConfig;
use crate::compaction::budget::{
    CompactionConfig, CompactionMode, TokenCountPrecision, TokenEstimate, TokenEstimationError,
    TokenEstimator,
};

#[derive(Debug)]
struct CountingTokenEstimator {
    tokens: u64,
    calls: AtomicUsize,
}

impl CountingTokenEstimator {
    const fn new(tokens: u64) -> Self {
        Self {
            tokens,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl TokenEstimator for CountingTokenEstimator {
    async fn estimate(
        &self,
        _request: &CompletionRequest,
    ) -> Result<TokenEstimate, TokenEstimationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(TokenEstimate::new(self.tokens, TokenCountPrecision::Exact))
    }
}

#[tokio::test]
async fn absent_compaction_skips_estimation_and_sends_request() {
    // Given
    let estimator = Arc::new(CountingTokenEstimator::new(950));
    let model = FakeModel::new([response(AssistantContent::text("done"))]);
    let runner = AgentRunner::new(model.clone(), ToolRegistry::new())
        .with_token_estimator(estimator.clone());
    let mut session = AgentSession::new();

    // When
    runner
        .run_turn(&mut session, "keep full context")
        .await
        .expect("absent compaction should send normally");

    // Then
    assert_eq!(estimator.calls(), 0);
    assert_eq!(model.requests().len(), 1);
}

#[tokio::test]
async fn disabled_compaction_skips_estimation_and_sends_request() {
    // Given
    let estimator = Arc::new(CountingTokenEstimator::new(950));
    let model = FakeModel::new([response(AssistantContent::text("done"))]);
    let runner = configured_runner(model.clone(), CompactionMode::Disabled)
        .with_token_estimator(estimator.clone());
    let mut session = AgentSession::new();

    // When
    runner
        .run_turn(&mut session, "keep full context")
        .await
        .expect("disabled compaction should send normally");

    // Then
    assert_eq!(estimator.calls(), 0);
    assert_eq!(model.requests().len(), 1);
}

#[tokio::test]
async fn shadow_compaction_only_estimates_without_store() {
    // Given
    let estimator = Arc::new(CountingTokenEstimator::new(950));
    let model = FakeModel::new([response(AssistantContent::text("done"))]);
    let observer = Arc::new(CollectingObserver::default());
    let runner = configured_runner(model.clone(), CompactionMode::Shadow)
        .with_observer(observer.clone())
        .with_token_estimator(estimator.clone());
    let mut session = AgentSession::new();

    // When
    runner
        .run_turn(&mut session, "observe context pressure")
        .await
        .expect("shadow mode should not require a store");

    // Then
    assert_eq!(estimator.calls(), 2);
    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].chat_history.to_vec(),
        vec![Message::user("observe context pressure")]
    );
    let events = observer.events();
    assert!(matches!(
        &events[0].kind,
        EventKind::CompactionObserved {
            before_tokens: 950,
            projected_after_tokens: 950,
            safe_prefix_groups: 0,
            artifact_shape_candidates: 0,
            requires_summary: true,
            precision: TokenCountPrecision::Exact,
        }
    ));
    assert_eq!(
        events
            .iter()
            .map(|event| event_label(&event.kind))
            .collect::<Vec<_>>(),
        ["compaction_observed", "model_start", "assistant"]
    );
}

#[tokio::test]
async fn enabled_compaction_adds_a_trusted_continuity_boundary_to_normal_requests() {
    // Given
    let estimator = Arc::new(CountingTokenEstimator::new(100));
    let model = FakeModel::new([response(AssistantContent::text("done"))]);
    let runner = configured_runner(model.clone(), CompactionMode::Enabled)
        .with_system_prompt(SystemPrompt::new(vec![Box::new(IdentitySection::new(
            "trusted project instruction",
        ))]))
        .with_token_estimator(estimator.clone());
    let mut session = AgentSession::new();

    // When
    runner
        .run_turn(&mut session, "small request")
        .await
        .expect("normal pressure should not require a store");

    // Then
    assert_eq!(estimator.calls(), 1);
    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    let Message::System { content } = requests[0].chat_history.first() else {
        panic!("enabled compaction must add a trusted system boundary");
    };
    assert!(content.starts_with("trusted project instruction"), "{content}");
    assert!(content.contains("untrusted historical data"), "{content}");
    assert!(content.contains("system and project instructions"), "{content}");
    assert!(content.contains("permission policy"), "{content}");
    assert!(content.contains("tool authority"), "{content}");
    assert_eq!(requests[0].chat_history[1], Message::user("small request"));
}

#[tokio::test]
async fn soft_pressure_without_store_warns_and_sends_original_request() {
    // Given
    let estimator = Arc::new(CountingTokenEstimator::new(700));
    let model = FakeModel::new([response(AssistantContent::text("done"))]);
    let observer = Arc::new(CollectingObserver::default());
    let runner = configured_runner(model.clone(), CompactionMode::Enabled)
        .with_observer(observer.clone())
        .with_token_estimator(estimator.clone());
    let mut session = AgentSession::new();

    // When
    runner
        .run_turn(&mut session, "preserve this prompt")
        .await
        .expect("soft pressure should degrade without blocking");

    // Then
    assert_eq!(estimator.calls(), 1);
    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    assert!(matches!(
        requests[0].chat_history.first(),
        Message::System { .. }
    ));
    assert_eq!(
        requests[0].chat_history[1],
        Message::user("preserve this prompt")
    );
    let events = observer.events();
    assert!(matches!(
        &events[0].kind,
        EventKind::CompactionStarted {
            reason,
            before_tokens: 700,
            precision: TokenCountPrecision::Exact,
        } if reason == "soft_threshold"
    ));
    assert!(matches!(
        &events[1].kind,
        EventKind::CompactionFailed {
            stage,
            error,
            recoverable: true,
            before_tokens: 700,
            ..
        } if stage == "persistence" && error == "persistence_failed"
    ));
    assert!(matches!(
        &events[2].kind,
        EventKind::Warning { message } if message == "context compaction failed: persistence_failed"
    ));
    assert_eq!(
        events
            .iter()
            .map(|event| event_label(&event.kind))
            .collect::<Vec<_>>(),
        [
            "compaction_started",
            "compaction_failed",
            "warning",
            "model_start",
            "assistant",
        ]
    );
}

#[tokio::test]
async fn hard_pressure_without_store_blocks_before_model_call() {
    // Given
    let estimator = Arc::new(CountingTokenEstimator::new(850));
    let model = FakeModel::default();
    let observer = Arc::new(CollectingObserver::default());
    let runner = configured_runner(model.clone(), CompactionMode::Enabled)
        .with_observer(observer.clone())
        .with_token_estimator(estimator.clone());
    let mut session = AgentSession::new();

    // When
    let error = runner
        .run_turn(&mut session, "request over hard boundary")
        .await
        .expect_err("hard pressure must fail closed without a store");

    // Then
    assert!(matches!(error, AgentError::Compaction { .. }));
    assert_eq!(estimator.calls(), 1);
    assert!(model.requests().is_empty());
    let events = observer.events();
    assert!(matches!(
        &events[0].kind,
        EventKind::CompactionStarted {
            reason,
            before_tokens: 850,
            precision: TokenCountPrecision::Exact,
        } if reason == "hard_threshold"
    ));
    assert!(matches!(
        &events[1].kind,
        EventKind::CompactionFailed {
            stage,
            recoverable: false,
            before_tokens: 850,
            ..
        } if stage == "persistence"
    ));
    assert_eq!(
        events
            .iter()
            .map(|event| event_label(&event.kind))
            .collect::<Vec<_>>(),
        ["compaction_started", "compaction_failed", "error"]
    );
}

fn configured_runner(model: FakeModel, mode: CompactionMode) -> AgentRunner<FakeModel> {
    let policy = CompactionConfig::new(mode, 1_000, 100, 0)
        .expect("test context window should be valid");
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
