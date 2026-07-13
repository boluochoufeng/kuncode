use super::{
    super::{CompactionError, compact_context},
    support::{
        CountingArtifactCounter, CountingProjector, DurableFixture, FixedGroupEstimator,
        ScriptedEstimator, SummaryBehavior, TestDependencies, TestSummarizer, config, dependencies,
        ordinary_history,
    },
};
use crate::{
    compaction::budget::CompactionMode,
    session_store::{SessionStore, SessionStoreError},
};

#[tokio::test]
async fn concurrent_append_during_summary_loses_cas_without_installing_candidate() {
    let mut fixture = DurableFixture::new(ordinary_history()).await;
    let before_messages = fixture.session.messages().to_vec();
    let before_head = fixture.session.durable_seq();
    let before_journal = fixture.journal_len().await;
    let config = config(CompactionMode::Enabled);
    let projector = CountingProjector::default();
    let estimator = ScriptedEstimator::new([80, 80, 40]);
    let group_estimator = FixedGroupEstimator::new(20);
    let counter = CountingArtifactCounter::new(100, 20);
    let summarizer = TestSummarizer::new(SummaryBehavior::AppendDuringCall {
        store: fixture.store.clone(),
        session_id: fixture.session_id.clone(),
    });

    let error = compact_context(dependencies(TestDependencies {
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
    .expect_err("stale compaction commit should lose journal CAS");

    assert!(matches!(
        error,
        CompactionError::Store(SessionStoreError::JournalHeadConflict { .. })
    ));
    assert_eq!(summarizer.calls(), 1);
    assert_eq!(fixture.session.messages(), before_messages);
    assert_eq!(fixture.session.durable_seq(), before_head);
    assert!(!fixture.session.is_durable());
    assert!(fixture.session.take_persistence_error().is_some());
    assert_eq!(fixture.journal_len().await, before_journal + 1);
    assert!(
        fixture
            .store
            .latest_checkpoint(&fixture.session_id)
            .await
            .expect("checkpoint read should succeed")
            .is_none()
    );
}
