use super::*;

#[test]
fn durable_append_records_exact_non_human_lineage() {
    let mut session = AgentSession::new();
    session.attach_session_id(SessionId::new("session-a"));

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
