use super::{
    super::{CompactionOutcome, CompactionPass, compact_context},
    support::{
        CountingArtifactCounter, CountingProjector, DurableFixture, FixedGroupEstimator,
        ScriptedEstimator, SummaryBehavior, TestDependencies, TestSummarizer, artifact_history,
        config, dependencies, ordinary_history,
    },
};
use crate::{
    compaction::budget::CompactionMode,
    session_store::{Seq, SessionStore},
};

#[tokio::test]
async fn insufficient_deterministic_reduction_summarizes_once_then_commits_and_installs() {
    let mut fixture = DurableFixture::new(ordinary_history()).await;
    let original_messages = fixture.session.messages().to_vec();
    let config = config(CompactionMode::Enabled);
    let projector = CountingProjector::default();
    let estimator = ScriptedEstimator::new([80, 80, 40]);
    let group_estimator = FixedGroupEstimator::new(20);
    let counter = CountingArtifactCounter::new(100, 20);
    let summarizer = TestSummarizer::new(SummaryBehavior::Valid);

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
    .expect("summary candidate should commit");

    let CompactionOutcome::Compacted(report) = outcome else {
        panic!("enabled pressure should compact");
    };
    assert_eq!(
        report.passes,
        vec![
            CompactionPass::SemanticSummary,
            CompactionPass::AtomicCommit
        ]
    );
    assert_eq!(report.source_start, Seq::new(1));
    assert_eq!(report.source_end, Seq::new(2));
    assert_eq!(
        report.summary_usage.map(|usage| usage.total_tokens),
        Some(40)
    );
    assert_eq!(summarizer.calls(), 1);
    assert_eq!(estimator.calls(), 3);
    assert_ne!(fixture.session.messages(), original_messages);
    assert!(fixture.session.active_summary().is_some());
    let checkpoint = fixture
        .store
        .latest_checkpoint(&fixture.session_id)
        .await
        .expect("checkpoint read should succeed")
        .expect("summary should persist a checkpoint");
    assert_eq!(checkpoint.active_messages, fixture.session.messages());
    assert!(checkpoint.summary_json.is_some());
    assert_eq!(checkpoint.model.as_deref(), Some("test-summary-model"));
}

#[tokio::test]
async fn successful_tool_result_without_safety_metadata_is_summarized_without_slimming() {
    // Given: an old successful result whose marker would be cheaper, but whose
    // invocation has no trusted effect or retention metadata.
    let mut fixture = DurableFixture::new(artifact_history()).await;
    let config = config(CompactionMode::Enabled);
    let projector = CountingProjector::default();
    let estimator = ScriptedEstimator::new([80, 80, 40]);
    let group_estimator = FixedGroupEstimator::new(20);
    let counter = CountingArtifactCounter::new(1_000, 20);
    let summarizer = TestSummarizer::new(SummaryBehavior::Valid);

    // When: production compaction evaluates the otherwise slimmable result.
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
    .expect("semantic compaction should remain available");

    // Then: no tool-result rewrite is authorized and the safe summary pass runs.
    let CompactionOutcome::Compacted(report) = outcome else {
        panic!("enabled pressure should compact");
    };
    assert_eq!(
        report.passes,
        vec![
            CompactionPass::SemanticSummary,
            CompactionPass::AtomicCommit
        ]
    );
    assert_eq!(report.artifact_count, 0);
    assert_eq!(summarizer.calls(), 1);
    assert_eq!(counter.calls(), 1);
}

#[tokio::test]
async fn trusted_retention_allows_slimming_to_reach_target_without_summary() {
    // Given: only the old result carries harness-minted slimming authority.
    let mut fixture = DurableFixture::new_with_slimmable_results(artifact_history(), &[1]).await;
    let config = config(CompactionMode::Enabled);
    let projector = CountingProjector::default();
    let estimator = ScriptedEstimator::new([80, 40]);
    let group_estimator = FixedGroupEstimator::new(20);
    let counter = CountingArtifactCounter::new(1_000, 20);
    let summarizer = TestSummarizer::new(SummaryBehavior::Valid);

    // When: the deterministic marker reaches the target boundary.
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
    .expect("trusted deterministic compaction should commit");

    // Then: summary is skipped and only the authorized old result is projected.
    let CompactionOutcome::Compacted(report) = outcome else {
        panic!("enabled pressure should compact");
    };
    assert_eq!(
        report.passes,
        vec![
            CompactionPass::ToolResultSlimming,
            CompactionPass::AtomicCommit
        ]
    );
    assert_eq!(summarizer.calls(), 0);
    assert_eq!(counter.calls(), 2);
    let active = fixture.session.messages();
    assert!(
        active
            .iter()
            .any(|message| format!("{message:?}").contains("slimmed_tool_result"))
    );
}
