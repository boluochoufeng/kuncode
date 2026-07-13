use super::{
    super::{CompactionOutcome, CompactionPass, compact_context},
    support::{
        CountingArtifactCounter, CountingProjector, DurableFixture, FixedGroupEstimator,
        ScriptedEstimator, SummaryBehavior, TestDependencies, TestSummarizer, artifact_history,
        config, dependencies,
    },
};
use crate::{compaction::budget::CompactionMode, session_store::SessionStore};

#[tokio::test]
async fn artifact_spill_reaching_target_commits_and_installs_without_summary() {
    let mut fixture = DurableFixture::new(artifact_history()).await;
    let original_messages = fixture.session.messages().to_vec();
    let config = config(CompactionMode::Enabled);
    let projector = CountingProjector::default();
    let estimator = ScriptedEstimator::new([40]);
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
    .expect("artifact candidate should commit");

    let CompactionOutcome::Compacted(report) = outcome else {
        panic!("enabled pressure should compact");
    };
    assert_eq!(
        report.passes,
        vec![CompactionPass::ArtifactSpill, CompactionPass::AtomicCommit]
    );
    assert_eq!(report.artifact_count, 1);
    assert!(report.target_reached);
    assert_eq!(summarizer.calls(), 0);
    assert_eq!(estimator.calls(), 1);
    assert_eq!(counter.calls(), 2);
    assert_ne!(fixture.session.messages(), original_messages);
    let checkpoint = fixture
        .store
        .latest_checkpoint(&fixture.session_id)
        .await
        .expect("checkpoint read should succeed")
        .expect("compaction should persist a checkpoint");
    assert_eq!(checkpoint.active_messages, fixture.session.messages());
    assert_eq!(checkpoint.summary_json, None);
    assert_eq!(checkpoint.source_seq_start, None);
    assert_eq!(checkpoint.source_seq_end, None);
    assert_eq!(checkpoint.model, None);
    assert_eq!(checkpoint.token_usage_json, None);
    assert_eq!(
        fixture.session.durable_seq(),
        Some(checkpoint.checkpoint_seq)
    );
    assert_eq!(report.checkpoint_seq, checkpoint.checkpoint_seq);
}
