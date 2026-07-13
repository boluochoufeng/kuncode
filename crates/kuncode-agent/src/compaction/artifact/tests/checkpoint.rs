use super::super::audit::audit_journal;
use super::support::{FixedCounter, tool_exchange};
use crate::{
    compaction::{
        artifact::{ArtifactSpillInput, ArtifactSpillOutcome, spill_artifacts},
        protocol::{group_messages, select_protected_recent_tail},
    },
    session::AgentSession,
    session_store::{
        NewCheckpoint, NewJournalEntry, NewSession, Seq, SessionStore, sqlite::SqliteSessionStore,
    },
    test_support::TestDir,
};

#[tokio::test]
async fn rebuilds_active_context_from_checkpoint_and_message_tail() {
    // Given: a checkpoint baseline followed by a complete recent exchange in the journal.
    let root = TestDir::new();
    let store = SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
        .await
        .expect("store should open");
    let session_id = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let raw_messages = [
        kuncode_core::completion::Message::user("raw one"),
        kuncode_core::completion::Message::user("raw two"),
    ];
    let mut head = Seq::ZERO;
    for message in &raw_messages {
        head = store
            .append(
                &session_id,
                NewJournalEntry::message(message).expect("message should encode"),
            )
            .await
            .expect("message should commit");
    }
    let checkpoint_messages = tool_exchange("old", "bash", "old payload");
    let covered = head;
    let gap = tool_exchange("gap", "glob", "gap payload");
    for message in &gap {
        head = store
            .append(
                &session_id,
                NewJournalEntry::message(message).expect("message should encode"),
            )
            .await
            .expect("gap message should commit");
    }
    let gap_result_seq = head;
    let checkpoint_seq = store
        .write_checkpoint(NewCheckpoint {
            session_id: session_id.clone(),
            covers_through_seq: covered,
            source_seq_start: Some(Seq::new(1)),
            source_seq_end: Some(covered),
            active_messages: checkpoint_messages.clone(),
            summary_json: Some(serde_json::json!({ "schema_version": 1 })),
            model: Some("test-summary-model".to_string()),
            token_usage_json: None,
        })
        .await
        .expect("checkpoint should commit");
    assert!(checkpoint_seq > head);
    let recent = tool_exchange("recent", "read_file", "recent payload");
    for message in &recent {
        head = store
            .append(
                &session_id,
                NewJournalEntry::message(message).expect("message should encode"),
            )
            .await
            .expect("message should commit");
    }
    let messages = [checkpoint_messages, gap, recent].concat();
    let mut session = AgentSession::from_messages(messages);
    session.attach_session_id(session_id);
    session.advance_durable_seq(head);
    let groups = group_messages(session.messages()).expect("active context should be closed");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1).expect("tail should exist");

    // When: artifact audit rebuilds from the checkpoint plus its journal tail.
    let input = ArtifactSpillInput::new(&groups, &protected, &session)
        .expect("active context should bind to the session");
    let audit = audit_journal(&input, &store)
        .await
        .expect("checkpoint-aware audit should pass");
    assert_eq!(audit.message_seq(1), None);
    assert_eq!(audit.message_seq(3), Some(gap_result_seq));
    let result = spill_artifacts(input, &store, &FixedCounter::new(9_000, 100))
        .await
        .expect("checkpoint-aware audit should pass");

    // Then: the old exchange spills and the protected recent exchange remains intact.
    assert!(matches!(
        result.outcomes(),
        [
            ArtifactSpillOutcome::Spilled { .. },
            ArtifactSpillOutcome::Spilled { .. }
        ]
    ));
    assert_ne!(result.groups()[0], groups[0]);
    assert_ne!(result.groups()[1], groups[1]);
    assert_eq!(result.groups()[2], groups[2]);
}
