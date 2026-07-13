use kuncode_core::{
    completion::{AssistantContent, Message},
    non_empty_vec::NonEmptyVec,
};

use super::{
    super::{CompactionOutcome, CompactionPass, compact_context},
    support::{
        CountingArtifactCounter, CountingProjector, DurableFixture, FixedGroupEstimator,
        ScriptedEstimator, SummaryBehavior, TestDependencies, TestSummarizer, config, dependencies,
        ordinary_history,
    },
};
use crate::{
    compaction::budget::{CompactionMode, TokenCountPrecision},
    session_store::{JournalKind, NewJournalEntry, SessionStore},
    tool::ToolOutput,
};

#[tokio::test]
async fn deterministic_compaction_preserves_inherited_summary_provenance() {
    // Given
    let mut fixture = DurableFixture::new(ordinary_history()).await;
    let config = config(CompactionMode::Enabled);
    let projector = CountingProjector::default();
    let estimator = ScriptedEstimator::new([80, 80, 40, 40]);
    let group_estimator = FixedGroupEstimator::new(20);
    let counter = CountingArtifactCounter::new(9_000, 20);
    let summarizer = TestSummarizer::new(SummaryBehavior::Valid);
    compact(dependencies(TestDependencies {
        config: &config,
        session: &mut fixture.session,
        store: fixture.store.as_ref(),
        projector: &projector,
        estimator: &estimator,
        group_estimator: &group_estimator,
        artifact_counter: &counter,
        summarizer: &summarizer,
    }))
    .await;
    let first = fixture
        .store
        .latest_checkpoint(&fixture.session_id)
        .await
        .expect("checkpoint lookup should succeed")
        .expect("semantic checkpoint should exist");
    for message in [tool_exchange("old"), tool_exchange("recent")].concat() {
        append(&mut fixture, message).await;
    }

    // When
    let second = compact(dependencies(TestDependencies {
        config: &config,
        session: &mut fixture.session,
        store: fixture.store.as_ref(),
        projector: &projector,
        estimator: &estimator,
        group_estimator: &group_estimator,
        artifact_counter: &counter,
        summarizer: &summarizer,
    }))
    .await;

    // Then
    assert_eq!(
        second.passes,
        vec![CompactionPass::ArtifactSpill, CompactionPass::AtomicCommit]
    );
    assert_eq!(summarizer.calls(), 1);
    let checkpoint = fixture
        .store
        .latest_checkpoint(&fixture.session_id)
        .await
        .expect("checkpoint lookup should succeed")
        .expect("deterministic checkpoint should exist");
    assert_eq!(checkpoint.summary_json, first.summary_json);
    assert_eq!(checkpoint.source_seq_start, first.source_seq_start);
    assert_eq!(checkpoint.source_seq_end, first.source_seq_end);
    assert_eq!(checkpoint.model, first.model);
    assert_eq!(checkpoint.token_usage_json, first.token_usage_json);
    let journal = fixture
        .store
        .replay_after(&fixture.session_id, crate::session_store::Seq::ZERO)
        .await
        .expect("journal should replay");
    let event = journal
        .iter()
        .rev()
        .find(|entry| entry.kind == JournalKind::Compaction.as_str())
        .expect("second compaction event should exist");
    assert_eq!(event.payload_json["schema_version"], 2);
    assert_eq!(event.payload_json["summary"], serde_json::Value::Null);
    assert_eq!(event.payload_json["model"], serde_json::Value::Null);
    assert_eq!(event.payload_json["token_usage"], serde_json::Value::Null);
    assert_eq!(
        event.payload_json["passes"],
        serde_json::json!(["artifact_spill", "atomic_commit"])
    );
    assert_eq!(second.after.precision(), TokenCountPrecision::Exact);
}

async fn compact(
    input: super::super::CompactionDependencies<'_>,
) -> super::super::types::CompactionReport {
    let outcome = compact_context(input)
        .await
        .expect("compaction should succeed");
    let CompactionOutcome::Compacted(report) = outcome else {
        panic!("pressure should produce a checkpoint");
    };
    report
}

async fn append(fixture: &mut DurableFixture, message: Message) {
    let seq = fixture
        .store
        .append(
            &fixture.session_id,
            NewJournalEntry::message(&message).expect("message should encode"),
        )
        .await
        .expect("message should commit");
    fixture.session.push_with_journal_seq(message, Some(seq));
}

fn tool_exchange(id: &str) -> Vec<Message> {
    let output = ToolOutput::success(serde_json::json!({"body": id})).to_model_content();
    vec![
        Message::Assistant {
            id: None,
            content: NonEmptyVec::new(AssistantContent::tool_call(
                id,
                "test_tool",
                serde_json::json!({}),
            )),
        },
        Message::tool_result(id, output),
    ]
}
