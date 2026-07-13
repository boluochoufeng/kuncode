use super::{
    super::{CompactionOutcome, compact_context},
    support::{
        CountingArtifactCounter, CountingProjector, DurableFixture, FixedGroupEstimator,
        ScriptedEstimator, SummaryBehavior, TestDependencies, TestSummarizer, config, dependencies,
        ordinary_history,
    },
};
use crate::{compaction::budget::CompactionMode, session_store::SessionStore};

#[tokio::test]
async fn disabled_mode_skips_projection_measurement_and_writes() {
    let mut fixture = DurableFixture::new(ordinary_history()).await;
    let before_messages = fixture.session.messages().to_vec();
    let before_head = fixture.session.durable_seq();
    let before_journal = fixture.journal_len().await;
    let config = config(CompactionMode::Disabled);
    let projector = CountingProjector::default();
    let estimator = ScriptedEstimator::new([]);
    let group_estimator = FixedGroupEstimator::new(20);
    let counter = CountingArtifactCounter::new(9_000, 100);
    let summarizer = TestSummarizer::new(SummaryBehavior::ProviderFailure);

    let outcome = compact_context(dependencies(TestDependencies {
        config: &config,
        session: &mut fixture.session,
        store: fixture.store.as_ref(),
        projector: &projector,
        estimator: &estimator,
        group_estimator: &group_estimator,
        artifact_counter: &counter,
        summarizer: &summarizer,
    }))
    .await
    .expect("disabled compaction should bypass");

    assert_eq!(outcome, CompactionOutcome::Bypassed);
    assert_eq!(projector.calls(), 0);
    assert_eq!(estimator.calls(), 0);
    assert_eq!(group_estimator.calls(), 0);
    assert_eq!(counter.calls(), 0);
    assert_eq!(summarizer.calls(), 0);
    assert_eq!(fixture.session.messages(), before_messages);
    assert_eq!(fixture.session.durable_seq(), before_head);
    assert_eq!(fixture.journal_len().await, before_journal);
    assert_eq!(
        fixture
            .store
            .latest_checkpoint(&fixture.session_id)
            .await
            .expect("checkpoint read should succeed"),
        None
    );
}

#[tokio::test]
async fn shadow_mode_reports_the_authoritative_baseline_without_remeasuring() {
    let mut fixture = DurableFixture::new(ordinary_history()).await;
    let before_messages = fixture.session.messages().to_vec();
    let before_head = fixture.session.durable_seq();
    let before_journal = fixture.journal_len().await;
    let config = config(CompactionMode::Shadow);
    let projector = CountingProjector::default();
    let estimator = ScriptedEstimator::new([]);
    let group_estimator = FixedGroupEstimator::new(20);
    let counter = CountingArtifactCounter::new(9_000, 100);
    let summarizer = TestSummarizer::new(SummaryBehavior::ProviderFailure);

    let outcome = compact_context(dependencies(TestDependencies {
        config: &config,
        session: &mut fixture.session,
        store: fixture.store.as_ref(),
        projector: &projector,
        estimator: &estimator,
        group_estimator: &group_estimator,
        artifact_counter: &counter,
        summarizer: &summarizer,
    }))
    .await
    .expect("shadow compaction should only observe");

    let CompactionOutcome::Observed(budget) = outcome else {
        panic!("shadow mode should return an observation");
    };
    assert_eq!(budget.current_input(), 80);
    assert_eq!(projector.calls(), 0);
    assert_eq!(estimator.calls(), 0);
    assert_eq!(group_estimator.calls(), 0);
    assert_eq!(counter.calls(), 0);
    assert_eq!(summarizer.calls(), 0);
    assert_eq!(fixture.session.messages(), before_messages);
    assert_eq!(fixture.session.durable_seq(), before_head);
    assert_eq!(fixture.journal_len().await, before_journal);
    assert!(
        fixture
            .store
            .latest_checkpoint(&fixture.session_id)
            .await
            .expect("checkpoint read should succeed")
            .is_none()
    );
}
