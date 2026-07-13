use super::*;

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

    let receipt = CommittedCompaction::new(SessionId::new("session-a"), Seq::new(1), Seq::new(2));
    let result = session.install_compacted_context(vec![Message::user("summary")], receipt);

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
    let receipt = CommittedCompaction::new(SessionId::new("session-a"), Seq::new(2), Seq::new(3));

    let result = session.install_compacted_context(vec![Message::user("summary")], receipt);

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
    let receipt = CommittedCompaction::new(SessionId::new("session-a"), Seq::new(2), Seq::new(3));

    let result = session.install_compacted_context(vec![Message::user("summary")], receipt);

    assert_eq!(result, Ok(()));
    assert_eq!(session.messages(), &[Message::user("summary")]);
    assert_eq!(session.durable_seq(), Some(Seq::new(3)));
}

#[test]
fn compaction_receipt_cannot_cross_session_boundaries() {
    let mut session = AgentSession::new();
    session.attach_session_id(SessionId::new("session-a"));
    session.push_user("original");
    let receipt = CommittedCompaction::new(SessionId::new("session-b"), Seq::new(1), Seq::new(2));

    let result = session.install_compacted_context(vec![Message::user("summary")], receipt);

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
