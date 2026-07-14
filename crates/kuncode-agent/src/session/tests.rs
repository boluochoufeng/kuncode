use super::*;
use crate::session_store::CommittedCompaction;

#[test]
fn push_user_routes_through_push() {
    let mut session = AgentSession::new();

    session
        .push_user("hello")
        .expect("unattached session should accept an in-memory message");

    assert_eq!(session.messages().len(), 1);
}

#[test]
fn public_push_rejects_messages_without_a_durable_receipt() {
    let mut session = AgentSession::new();
    session
        .attach_session_id(SessionId::new("session-a"))
        .expect("fresh session should attach");

    let error = session
        .push_user("not journaled")
        .expect_err("attached sessions require the runner's durable-first path");

    assert_eq!(error, SessionAppendError::DurableReceiptRequired);
    assert!(session.messages().is_empty());
    assert!(session.is_durable());
    assert_eq!(session.durable_seq(), Some(Seq::ZERO));
}

#[tokio::test]
async fn public_start_creates_and_attaches_a_new_empty_journal() {
    let root = crate::test_support::TestDir::new();
    let store = crate::session_store::sqlite::SqliteSessionStore::open(
        root.path().join("sessions.sqlite3"),
    )
    .await
    .expect("store should open");
    let mut session = AgentSession::new();

    session
        .start_durable_session(
            &store,
            crate::session_store::NewSession::new(root.path().to_path_buf()),
        )
        .await
        .expect("store should create and bind a new journal");

    assert!(session.session_id().is_some());
    assert_eq!(session.durable_seq(), Some(Seq::ZERO));
    assert!(session.messages().is_empty());
    assert!(session.is_durable());
}

#[test]
fn clone_does_not_carry_session_id() {
    let mut session = AgentSession::new();
    session
        .attach_session_id(SessionId::new("session-a"))
        .expect("fresh session should attach");
    session.push_with_journal_seq(Message::user("one"), Some(Seq::new(1)));

    let mut cloned = session.clone();
    cloned
        .push_user("two")
        .expect("cloned session has no durable identity");
    assert!(cloned.session_id().is_none());
    assert_eq!(
        session.session_id().map(SessionId::as_str),
        Some("session-a")
    );
    assert_eq!(cloned.durable_seq(), None);
}

#[test]
fn durable_frontier_advances_monotonically() {
    let mut session = AgentSession::new();
    session
        .attach_session_id(SessionId::new("session-a"))
        .expect("fresh session should attach");

    assert_eq!(session.durable_seq(), Some(Seq::ZERO));

    session.advance_durable_seq(Seq::new(2));
    session.advance_durable_seq(Seq::new(1));

    assert_eq!(session.durable_seq(), Some(Seq::new(2)));
}

#[test]
fn compacted_context_requires_a_current_commit_receipt() {
    let mut session = AgentSession::new();
    session
        .attach_session_id(SessionId::new("session-a"))
        .expect("fresh session should attach");
    session.advance_durable_seq(Seq::new(3));
    session.push_with_journal_seq(Message::user("original"), Some(Seq::new(3)));

    let messages = vec![Message::user("summary")];
    let receipt = committed_compaction("session-a", 1, 2, &messages);
    let result = session.install_compacted_context(prepared(messages), receipt);

    assert!(matches!(
        result,
        Err(SessionMutationError::StaleCommit { .. })
    ));
    assert_eq!(session.messages(), &[Message::user("original")]);
}

#[test]
fn persistence_failure_prevents_compacted_context_installation() {
    let mut session = AgentSession::new();
    session
        .attach_session_id(SessionId::new("session-a"))
        .expect("fresh session should attach");
    session.advance_durable_seq(Seq::new(1));
    session.push_with_journal_seq(Message::user("original"), Some(Seq::new(1)));
    session.mark_persistence_failed("write failed");
    let messages = vec![Message::user("summary")];
    let receipt = committed_compaction("session-a", 2, 3, &messages);

    let result = session.install_compacted_context(prepared(messages), receipt);

    assert_eq!(result, Err(SessionMutationError::NonDurable));
    assert_eq!(session.messages(), &[Message::user("original")]);
    assert_eq!(session.durable_seq(), Some(Seq::new(1)));
}

#[test]
fn committed_compaction_installs_context_and_advances_frontier() {
    let mut session = AgentSession::new();
    session
        .attach_session_id(SessionId::new("session-a"))
        .expect("fresh session should attach");
    session.advance_durable_seq(Seq::new(1));
    session.push_with_journal_seq(Message::user("original"), Some(Seq::new(1)));
    let messages = vec![Message::user("summary")];
    let receipt = committed_compaction("session-a", 2, 3, &messages);

    let result = session.install_compacted_context(prepared(messages), receipt);

    assert_eq!(result, Ok(()));
    assert_eq!(session.messages(), &[Message::user("summary")]);
    assert_eq!(session.durable_seq(), Some(Seq::new(3)));
}

#[test]
fn compaction_receipt_cannot_cross_session_boundaries() {
    let mut session = AgentSession::new();
    session
        .attach_session_id(SessionId::new("session-a"))
        .expect("fresh session should attach");
    session.push_with_journal_seq(Message::user("original"), Some(Seq::new(1)));
    let messages = vec![Message::user("summary")];
    let receipt = committed_compaction("session-b", 1, 2, &messages);

    let result = session.install_compacted_context(prepared(messages), receipt);

    assert_eq!(result, Err(SessionMutationError::SessionMismatch));
    assert_eq!(session.messages(), &[Message::user("original")]);
}

#[test]
fn unattached_session_writes_nothing() {
    let mut session = AgentSession::new();
    session
        .push_user("hello")
        .expect("unattached session should accept an in-memory message");
    assert_eq!(session.messages().len(), 1);
    assert!(session.take_persistence_error().is_none());
}

#[test]
fn persistence_failure_is_reported_once() {
    let mut session = AgentSession::new();

    session.mark_persistence_failed("disk on fire");

    assert_eq!(
        session.take_persistence_error(),
        Some("session persistence failed: disk on fire".to_string())
    );
    assert!(session.take_persistence_error().is_none());
    assert!(!session.is_durable());
}

#[test]
fn durable_identity_rejects_non_pristine_session() {
    let mut session = AgentSession::from_messages(vec![Message::user("imported")]);

    let error = session
        .attach_session_id(SessionId::new("session-a"))
        .expect_err("imported state has no durable lineage");

    assert_eq!(error, SessionAttachError::NotPristine);
    assert!(session.session_id().is_none());
    assert_eq!(session.durable_seq(), None);
}

#[test]
fn durable_identity_cannot_reset_an_existing_frontier() {
    let mut session = AgentSession::new();
    session
        .attach_session_id(SessionId::new("session-a"))
        .expect("fresh session should attach");
    session.advance_durable_seq(Seq::new(7));

    let error = session
        .attach_session_id(SessionId::new("session-b"))
        .expect_err("an attached session must not be rebound");

    assert_eq!(error, SessionAttachError::AlreadyAttached);
    assert_eq!(
        session.session_id().map(SessionId::as_str),
        Some("session-a")
    );
    assert_eq!(session.durable_seq(), Some(Seq::new(7)));
}

#[test]
fn durable_identity_cannot_clear_fail_closed_state() {
    let mut session = AgentSession::new();
    session.mark_persistence_failed("write failed");

    let error = session
        .attach_session_id(SessionId::new("session-a"))
        .expect_err("lost authority must remain non-durable");

    assert_eq!(error, SessionAttachError::PersistenceFailed);
    assert!(!session.is_durable());
    assert!(session.session_id().is_none());
}

fn committed_compaction(
    session_id: &str,
    compaction_seq: i64,
    journal_head: i64,
    messages: &[Message],
) -> CommittedCompaction {
    let output_hash = crate::session_store::active_messages_sha256(messages)
        .expect("test messages should encode");
    CommittedCompaction::new(
        SessionId::new(session_id),
        Seq::new(compaction_seq),
        Seq::new(journal_head),
        output_hash,
    )
}

fn prepared(messages: Vec<Message>) -> PreparedActiveContext {
    let lineage = vec![MessageLineage::appended(None, false); messages.len()];
    PreparedActiveContext::new(messages, lineage, None).expect("fixture should be aligned")
}

#[test]
fn durable_append_records_exact_non_human_lineage() {
    let mut session = AgentSession::new();
    session
        .attach_session_id(SessionId::new("session-a"))
        .expect("fresh session should attach");

    session.push_with_journal_seq(Message::assistant("done"), Some(Seq::new(7)));

    let lineage = &session.message_lineage()[0];
    let coverage = lineage.coverage().expect("durable append has coverage");
    assert_eq!(
        (coverage.start(), coverage.end()),
        (Seq::new(7), Seq::new(7))
    );
    assert!(!lineage.human_authored());
    assert!(lineage.artifact_refs().is_empty());
    assert!(session.trusted_human_message_indices().next().is_none());
}

#[test]
fn imported_and_cloned_messages_have_no_trusted_lineage() {
    let mut source = AgentSession::from_messages(vec![Message::user("imported")]);
    source.push_human_with_journal_seq(Message::user("live"), Some(Seq::new(3)));

    let cloned = source.clone();

    assert!(source.message_lineage()[0].coverage().is_none());
    assert!(!source.message_lineage()[0].human_authored());
    assert_eq!(
        source.trusted_human_message_indices().collect::<Vec<_>>(),
        [1]
    );
    assert!(cloned.message_lineage().iter().all(|lineage| {
        lineage.coverage().is_none()
            && !lineage.human_authored()
            && lineage.artifact_refs().is_empty()
    }));
}
