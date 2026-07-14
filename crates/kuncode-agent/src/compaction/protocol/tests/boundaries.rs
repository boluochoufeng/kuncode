use kuncode_core::completion::Message;

use super::{assistant_with_calls, result_message};
use crate::compaction::protocol::{
    HumanMessageIndex, ProtocolError, ProtocolGroup, current_human_request_anchor, group_messages,
    select_protected_recent_tail,
};

#[test]
fn anchor_copies_latest_authoritative_human_text_only_when_summarized() {
    let messages = vec![
        Message::user("first"),
        Message::assistant("work"),
        Message::user("latest"),
        Message::assistant("answer"),
    ];
    let human = [HumanMessageIndex(0), HumanMessageIndex(2)];

    let anchor = current_human_request_anchor(&messages, &human, 3)
        .expect("indices are valid")
        .expect("latest human message is summarized");

    assert_eq!(anchor.source_message_index, 2);
    assert_eq!(anchor.message, Message::user("latest"));
    assert!(matches!(
        current_human_request_anchor(&messages, &human, 2),
        Ok(None)
    ));
}

#[test]
fn anchor_rejects_index_that_is_not_human_text() {
    let messages = vec![result_message(&[("one", None)], None)];

    let error = current_human_request_anchor(&messages, &[HumanMessageIndex(0)], 1)
        .expect_err("tool result is not human text");

    assert_eq!(
        error,
        ProtocolError::InvalidHumanMessageIndex { message_index: 0 }
    );
}

#[test]
fn protected_tail_keeps_latest_exchange_and_contiguous_suffix() {
    let groups = group_messages(&[
        Message::user("old"),
        assistant_with_calls(&[("one", None)]),
        result_message(&[("one", None)], None),
        Message::assistant("recent"),
    ])
    .expect("fixture is valid");

    let tail = select_protected_recent_tail(&groups, 5, |group| match group {
        ProtocolGroup::Message(_) => 2,
        ProtocolGroup::ToolExchange { .. } => 4,
    })
    .expect("non-empty history has a tail");

    assert_eq!(tail.group_range, 1..3);
    assert_eq!(tail.estimated_tokens, 6);
    assert_eq!(tail.budget_tokens, 5);
}

#[test]
fn protected_tail_uses_last_ordinary_group_without_tools() {
    let groups = group_messages(&[
        Message::user("old"),
        Message::assistant("middle"),
        Message::user("latest"),
    ])
    .expect("ordinary messages are valid");

    let tail =
        select_protected_recent_tail(&groups, 10, |_| 4).expect("non-empty history has a tail");

    assert_eq!(tail.group_range, 1..3);
}

#[test]
fn mandatory_group_may_exceed_recent_tail_budget() {
    let groups = group_messages(&[
        Message::user("old"),
        assistant_with_calls(&[("one", None)]),
        result_message(&[("one", None)], None),
    ])
    .expect("fixture is valid");

    let tail = select_protected_recent_tail(&groups, 1, |group| match group {
        ProtocolGroup::Message(_) => 1,
        ProtocolGroup::ToolExchange { .. } => 9,
    })
    .expect("mandatory exchange is always retained");

    assert_eq!(tail.group_range, 1..2);
    assert_eq!(tail.estimated_tokens, 9);
    assert_eq!(tail.budget_tokens, 1);
}

#[test]
fn protected_tail_respects_non_default_budget_from_caller() {
    let groups = group_messages(&[
        Message::user("old"),
        Message::assistant("middle"),
        Message::user("latest"),
    ])
    .expect("ordinary messages are valid");

    let tail =
        select_protected_recent_tail(&groups, 7, |_| 3).expect("non-empty history has a tail");

    assert_eq!(tail.group_range, 1..3);
    assert_eq!(tail.estimated_tokens, 6);
    assert_eq!(tail.budget_tokens, 7);
}
