use kuncode_core::completion::Message;

use super::result_message;
use crate::compaction::protocol::{HumanMessageIndex, ProtocolError, current_human_request_anchor};

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
        .err()
        .expect("tool result is not human text");

    assert_eq!(
        error,
        ProtocolError::InvalidHumanMessageIndex { message_index: 0 }
    );
}
