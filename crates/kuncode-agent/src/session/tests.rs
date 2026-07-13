use super::*;
use crate::session_store::CommittedCompaction;

mod lineage;

#[test]
fn push_user_routes_through_push() {
    let mut session = AgentSession::new();

    session.push_user("hello");

    assert_eq!(session.messages().len(), 1);
}

#[test]
fn clone_does_not_carry_session_id() {
    let mut session = AgentSession::new();
    session.attach_session_id(SessionId::new("session-a"));
    session.push_user("one");

    let mut cloned = session.clone();
    cloned.push_user("two");
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
    session.attach_session_id(SessionId::new("session-a"));

    assert_eq!(session.durable_seq(), Some(Seq::ZERO));

    session.advance_durable_seq(Seq::new(2));
    session.advance_durable_seq(Seq::new(1));

    assert_eq!(session.durable_seq(), Some(Seq::new(2)));
}

#[test]
fn compacted_context_requires_a_current_commit_receipt() {
    let mut session = AgentSession::new();
    session.attach_session_id(SessionId::new("session-a"));
    session.advance_durable_seq(Seq::new(3));
    session.push_user("original");

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
    session.attach_session_id(SessionId::new("session-a"));
    session.advance_durable_seq(Seq::new(1));
    session.push_user("original");
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
    session.attach_session_id(SessionId::new("session-a"));
    session.advance_durable_seq(Seq::new(1));
    session.push_user("original");
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
    session.attach_session_id(SessionId::new("session-a"));
    session.push_user("original");
    let messages = vec![Message::user("summary")];
    let receipt = committed_compaction("session-b", 1, 2, &messages);

    let result = session.install_compacted_context(prepared(messages), receipt);

    assert_eq!(result, Err(SessionMutationError::SessionMismatch));
    assert_eq!(session.messages(), &[Message::user("original")]);
}

#[test]
fn unattached_session_writes_nothing() {
    let mut session = AgentSession::new();
    session.push_user("hello");
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
