use kuncode_core::completion::Message;

use crate::{
    compaction::{
        artifact::fixture_spill_result_for_session,
        protocol::{HumanMessageIndex, ProtectedRecentTail, group_messages},
        selection::{SelectionLimits, SelectionOutcome, select_prefix_tail},
        summary::build_summary_prompt,
    },
    session::{AgentSession, SummarySourceError},
    session_store::{Seq, SessionId},
};

#[test]
fn production_issuer_binds_the_selected_durable_prefix() {
    let session_id = SessionId::new("summary-source");
    let messages = vec![Message::user("old"), Message::assistant("recent")];
    let mut session = AgentSession::new();
    session.attach_session_id(session_id.clone());
    session.push_with_journal_seq(messages[0].clone(), Some(Seq::new(1)));
    session.push_with_journal_seq(messages[1].clone(), Some(Seq::new(2)));
    let groups = group_messages(&messages).expect("history should be closed");
    let selection = selection(&groups, &messages);
    let artifacts = fixture_spill_result_for_session(session_id, groups, Seq::new(2), vec![]);

    let request = session
        .issue_summary_request(&artifacts, &selection)
        .expect("durable selection should mint a request");
    let prompt = build_summary_prompt(&request).expect("prompt should encode");
    let encoded = serde_json::to_value(&prompt[1]).expect("user payload should encode");
    let text = encoded["content"][0]["text"]
        .as_str()
        .expect("user payload should contain text");
    let payload: serde_json::Value = serde_json::from_str(text).expect("payload should be JSON");

    assert_eq!(payload["source_seq_start"], 1);
    assert_eq!(payload["source_seq_end"], 1);
    assert_eq!(payload["source_messages"][0]["content"][0]["text"], "old");
}

#[test]
fn production_issuer_rejects_missing_or_cross_session_provenance() {
    let messages = vec![Message::user("old"), Message::assistant("recent")];
    let groups = group_messages(&messages).expect("history should be closed");
    let selection = selection(&groups, &messages);
    let mut session = AgentSession::from_messages(messages);
    session.attach_session_id(SessionId::new("active"));

    let missing = fixture_spill_result_for_session(
        SessionId::new("active"),
        groups.clone(),
        Seq::ZERO,
        vec![],
    );
    assert_eq!(
        session.issue_summary_request(&missing, &selection),
        Err(SummarySourceError::MissingMessageProvenance { message_index: 0 })
    );

    let foreign =
        fixture_spill_result_for_session(SessionId::new("foreign"), groups, Seq::ZERO, vec![]);
    assert_eq!(
        session.issue_summary_request(&foreign, &selection),
        Err(SummarySourceError::SnapshotMismatch)
    );
}

#[test]
fn production_issuer_rejects_selection_with_a_rebound_prefix() {
    let session_id = SessionId::new("summary-source");
    let messages = vec![Message::user("old"), Message::assistant("recent")];
    let groups = group_messages(&messages).expect("history should be closed");
    let artifacts =
        fixture_spill_result_for_session(session_id.clone(), groups.clone(), Seq::new(2), vec![]);
    let mut session = AgentSession::new();
    session.attach_session_id(session_id);
    session.push_with_journal_seq(messages[0].clone(), Some(Seq::new(1)));
    session.push_with_journal_seq(messages[1].clone(), Some(Seq::new(2)));

    let forged_messages = vec![Message::user("forged"), messages[1].clone()];
    let forged_groups = group_messages(&forged_messages).expect("forged history should be closed");
    let rebound = selection(&forged_groups, &forged_messages);

    assert_eq!(
        session.issue_summary_request(&artifacts, &rebound),
        Err(SummarySourceError::SnapshotMismatch)
    );
}

fn selection(
    groups: &[crate::compaction::protocol::ProtocolGroup],
    messages: &[Message],
) -> crate::compaction::selection::CompactionSelection {
    let protected = ProtectedRecentTail {
        group_range: 1..2,
        estimated_tokens: 1,
        budget_tokens: 1,
    };
    let outcome = select_prefix_tail(
        groups,
        messages,
        &protected,
        &[HumanMessageIndex(0)],
        SelectionLimits::new(10, 100).expect("limits should be valid"),
        50,
    )
    .expect("selection should succeed");
    let SelectionOutcome::Summarize(selection) = outcome else {
        panic!("old prefix should require summary");
    };
    selection
}
