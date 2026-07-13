use super::{
    super::{CompactionError, compact_context},
    support::{
        CountingArtifactCounter, CountingProjector, DurableFixture, FixedGroupEstimator,
        RejectedReceiptStore, ScriptedEstimator, SummaryBehavior, TestDependencies, TestSummarizer,
        UnknownCommitStore, config, dependencies, ordinary_history,
    },
};
use crate::{
    compaction::{budget::CompactionMode, summary::SummarizerError},
    session_store::{SessionStore, SessionStoreError},
};

#[tokio::test]
async fn malformed_summary_leaves_active_and_durable_context_unchanged() {
    assert_summary_failure(SummaryBehavior::Malformed, |error| {
        matches!(error, SummarizerError::InvalidSummary { .. })
    })
    .await;
}

#[tokio::test]
async fn provider_summary_failure_leaves_active_and_durable_context_unchanged() {
    assert_summary_failure(SummaryBehavior::ProviderFailure, |error| {
        matches!(error, SummarizerError::Completion(_))
    })
    .await;
}

#[tokio::test]
async fn unknown_commit_outcome_keeps_candidate_uninstalled_and_marks_session_non_durable() {
    let mut fixture = DurableFixture::new(ordinary_history()).await;
    let before_messages = fixture.session.messages().to_vec();
    let before_journal = fixture.journal_len().await;
    let store = UnknownCommitStore::new(fixture.store.clone());
    let config = config(CompactionMode::Enabled);
    let projector = CountingProjector::default();
    let estimator = ScriptedEstimator::new([80, 80, 40]);
    let group_estimator = FixedGroupEstimator::new(20);
    let counter = CountingArtifactCounter::new(100, 20);
    let summarizer = TestSummarizer::new(SummaryBehavior::Valid);

    let error = compact_context(dependencies(TestDependencies {
        config: &config,
        session: &mut fixture.session,
        store: &store,
        projector: &projector,
        estimator: &estimator,
        group_estimator: &group_estimator,
        artifact_counter: &counter,
        summarizer: &summarizer,
    }))
    .await
    .expect_err("ambiguous commit should abort installation");

    assert!(matches!(
        error,
        CompactionError::Store(SessionStoreError::CommitOutcomeUnknown { .. })
    ));
    assert_eq!(fixture.session.messages(), before_messages);
    assert!(!fixture.session.is_durable());
    assert!(fixture.session.take_persistence_error().is_some());
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

#[tokio::test]
async fn committed_checkpoint_with_rejected_receipt_marks_session_non_durable() {
    // Given
    let mut fixture = DurableFixture::new(ordinary_history()).await;
    let before_messages = fixture.session.messages().to_vec();
    let before_journal = fixture.journal_len().await;
    let store = RejectedReceiptStore::new(fixture.store.clone());
    let config = config(CompactionMode::Enabled);
    let projector = CountingProjector::default();
    let estimator = ScriptedEstimator::new([80, 80, 40]);
    let group_estimator = FixedGroupEstimator::new(20);
    let counter = CountingArtifactCounter::new(100, 20);
    let summarizer = TestSummarizer::new(SummaryBehavior::Valid);

    // When
    let error = compact_context(dependencies(TestDependencies {
        config: &config,
        session: &mut fixture.session,
        store: &store,
        projector: &projector,
        estimator: &estimator,
        group_estimator: &group_estimator,
        artifact_counter: &counter,
        summarizer: &summarizer,
    }))
    .await
    .expect_err("a mismatched receipt must abort installation");

    // Then
    assert!(matches!(error, CompactionError::Apply(_)));
    assert_eq!(fixture.session.messages(), before_messages);
    assert!(!fixture.session.is_durable());
    assert!(fixture.session.take_persistence_error().is_some());
    assert_eq!(fixture.journal_len().await, before_journal + 2);
    assert!(
        fixture
            .store
            .latest_checkpoint(&fixture.session_id)
            .await
            .expect("checkpoint read should succeed")
            .is_some()
    );
}

async fn assert_summary_failure(
    behavior: SummaryBehavior,
    matches_error: impl FnOnce(&SummarizerError) -> bool,
) {
    let mut fixture = DurableFixture::new(ordinary_history()).await;
    let before_messages = fixture.session.messages().to_vec();
    let before_head = fixture.session.durable_seq();
    let before_journal = fixture.journal_len().await;
    let config = config(CompactionMode::Enabled);
    let projector = CountingProjector::default();
    let estimator = ScriptedEstimator::new([80, 80]);
    let group_estimator = FixedGroupEstimator::new(20);
    let counter = CountingArtifactCounter::new(100, 20);
    let summarizer = TestSummarizer::new(behavior);

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
    .expect_err("summary failure should abort compaction");

    let CompactionError::Summary(summary_error) = &error else {
        panic!("failure should preserve the summary error: {error}");
    };
    assert!(matches_error(summary_error));
    assert_eq!(summarizer.calls(), 1);
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
