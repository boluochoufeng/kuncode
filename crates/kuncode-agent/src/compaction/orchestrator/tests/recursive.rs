use kuncode_core::{
    completion::{AssistantContent, Message},
    non_empty_vec::NonEmptyVec,
};

use super::{
    super::{CompactionOutcome, compact_context, types::CompactionReport},
    support::{
        CountingArtifactCounter, CountingProjector, DurableFixture, FixedGroupEstimator,
        ScriptedEstimator, SummaryBehavior, TestDependencies, TestSummarizer, artifact_history,
        config, dependencies,
    },
};
use crate::{
    compaction::{budget::CompactionMode, summary::project_summary_message},
    session_store::{JournalKind, NewJournalEntry, SessionStore, active_messages_sha256},
    tool::ToolOutput,
};

#[tokio::test]
async fn recursive_semantic_compaction_extends_trusted_provenance() {
    // Given: one live durable session whose first old tool result can be spilled.
    let mut fixture = DurableFixture::new(artifact_history()).await;
    let config = config(CompactionMode::Enabled);
    let projector = CountingProjector::default();
    let estimator = ScriptedEstimator::new([80, 80, 40, 80, 80, 40]);
    let group_estimator = FixedGroupEstimator::new(20);
    let counter = CountingArtifactCounter::new(9_000, 20);
    let summarizer = TestSummarizer::new(SummaryBehavior::Valid);
    let first_input = fixture.session.messages().to_vec();

    // When: semantic compaction runs, then new durable facts trigger it again.
    let first = compact_context(dependencies(TestDependencies {
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
    .expect("first recursive baseline should compact");
    let CompactionOutcome::Compacted(first_report) = first else {
        panic!("first pressured context should compact");
    };
    let first_checkpoint = fixture
        .store
        .latest_checkpoint(&fixture.session_id)
        .await
        .expect("first checkpoint read should succeed")
        .expect("first checkpoint should exist");
    let first_summary = fixture
        .session
        .active_summary()
        .cloned()
        .expect("first summary should be active");
    let first_projection =
        project_summary_message(&first_summary).expect("first summary projection should encode");

    let latest_human = Message::user("keep this request byte-for-byte: café 中文");
    append_message(&mut fixture, latest_human.clone(), true).await;
    for message in tool_exchange("new", "read_file", "protected payload") {
        append_message(&mut fixture, message, false).await;
    }
    let second_input = fixture.session.messages().to_vec();
    let second = compact_context(dependencies(TestDependencies {
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
    .expect("second recursive compaction should commit");
    let CompactionOutcome::Compacted(second_report) = second else {
        panic!("second pressured context should compact");
    };

    // Then: recursive provenance expands, without treating the old projection as source history.
    let observations = summarizer.observations();
    assert_eq!(observations.len(), 2);
    assert_eq!(
        observations[1].existing_summary.as_ref(),
        Some(&first_summary)
    );
    assert_eq!(
        observations[1].source_seq_start,
        first_report.source_start.get()
    );
    assert!(observations[1].source_seq_end > first_report.source_end.get());
    assert_eq!(
        observations[1].source_seq_end,
        second_report.source_end.get()
    );
    assert!(!observations[1].source_messages.contains(&first_projection));
    assert!(
        observations[1]
            .source_messages
            .iter()
            .all(|message| !matches!(message, Message::System { .. }))
    );
    assert!(observations[1].source_messages.contains(&latest_human));
    assert_eq!(first_summary.artifact_refs.len(), 1);
    assert_eq!(observations[1].allowed_artifact_refs.len(), 2);
    assert!(
        first_summary
            .artifact_refs
            .iter()
            .all(|reference| observations[1].allowed_artifact_refs.contains(reference))
    );
    let second_summary = fixture
        .session
        .active_summary()
        .expect("second summary should be active");
    assert_eq!(
        second_summary
            .artifact_refs
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        observations[1].allowed_artifact_refs
    );
    assert_eq!(fixture.session.messages()[1], latest_human);

    let second_checkpoint = fixture
        .store
        .latest_checkpoint(&fixture.session_id)
        .await
        .expect("second checkpoint read should succeed")
        .expect("second checkpoint should exist");
    let journal = fixture
        .store
        .replay_after(&fixture.session_id, crate::session_store::Seq::ZERO)
        .await
        .expect("journal should replay");
    let receipts = journal
        .windows(2)
        .filter(|pair| {
            pair[0].kind == JournalKind::Compaction.as_str()
                && pair[1].kind == JournalKind::CheckpointRef.as_str()
        })
        .collect::<Vec<_>>();
    assert_eq!(receipts.len(), 2);
    assert_receipt(ReceiptExpectation {
        checkpoint: &first_checkpoint,
        report: &first_report,
        journal: receipts[0],
        input_hash: active_messages_sha256(&first_input).expect("first input should hash"),
    });
    assert_receipt(ReceiptExpectation {
        checkpoint: &second_checkpoint,
        report: &second_report,
        journal: receipts[1],
        input_hash: active_messages_sha256(&second_input).expect("second input should hash"),
    });
}

async fn append_message(fixture: &mut DurableFixture, message: Message, human: bool) {
    let seq = fixture
        .store
        .append(
            &fixture.session_id,
            NewJournalEntry::message(&message).expect("message should encode"),
        )
        .await
        .expect("message should commit");
    if human {
        fixture
            .session
            .push_human_with_journal_seq(message, Some(seq));
    } else {
        fixture.session.push_with_journal_seq(message, Some(seq));
    }
}

fn tool_exchange(id: &str, name: &str, body: &str) -> Vec<Message> {
    let output = ToolOutput::success(serde_json::json!({"body": body})).to_model_content();
    vec![
        Message::Assistant {
            id: None,
            content: NonEmptyVec::new(AssistantContent::tool_call(id, name, serde_json::json!({}))),
        },
        Message::tool_result(id, output),
    ]
}

struct ReceiptExpectation<'a> {
    checkpoint: &'a crate::session_store::Checkpoint,
    report: &'a CompactionReport,
    journal: &'a [crate::session_store::JournalEntry],
    input_hash: String,
}

fn assert_receipt(expected: ReceiptExpectation<'_>) {
    let [compaction, checkpoint_ref] = expected.journal else {
        panic!("receipt should contain a compaction and checkpoint reference");
    };
    assert_eq!(checkpoint_ref.seq, expected.checkpoint.checkpoint_seq);
    assert_eq!(
        checkpoint_ref.payload_json["checkpoint_seq"],
        expected.checkpoint.checkpoint_seq.get()
    );
    assert_eq!(
        checkpoint_ref.payload_json["covers_through_seq"],
        expected.checkpoint.covers_through_seq.get()
    );
    assert_eq!(compaction.payload_json["input_hash"], expected.input_hash);
    assert_eq!(
        compaction.payload_json["output_hash"],
        active_messages_sha256(&expected.checkpoint.active_messages)
            .expect("checkpoint should hash")
    );
    assert_eq!(
        expected.checkpoint.source_seq_start,
        Some(expected.report.source_start)
    );
    assert_eq!(
        expected.checkpoint.source_seq_end,
        Some(expected.report.source_end)
    );
    assert_eq!(
        compaction.payload_json["source_seq_start"],
        expected.report.source_start.get()
    );
    assert_eq!(
        compaction.payload_json["source_seq_end"],
        expected.report.source_end.get()
    );
}
