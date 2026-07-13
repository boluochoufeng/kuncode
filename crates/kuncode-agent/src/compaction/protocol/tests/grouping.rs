use kuncode_core::completion::Message;

use super::{assistant_with_calls, flatten, result_message};
use crate::compaction::protocol::{ProtocolError, ProtocolGroup, group_messages};

#[test]
fn groups_complete_multi_tool_exchange_without_rewriting_messages() {
    let messages = vec![
        Message::user("do it"),
        assistant_with_calls(&[("one", None), ("two", Some("provider-two"))]),
        result_message(&[("one", None)], None),
        result_message(&[("two", Some("provider-two"))], Some("feedback")),
        Message::assistant("done"),
    ];

    let groups = group_messages(&messages).expect("complete exchange is valid");

    assert_eq!(groups.len(), 3);
    assert!(matches!(
        &groups[1],
        ProtocolGroup::ToolExchange { results, .. } if results.len() == 2
    ));
    assert_eq!(flatten(&groups), messages);
}

#[test]
fn rejects_missing_tool_result() {
    let messages = vec![
        assistant_with_calls(&[("one", None), ("two", None)]),
        result_message(&[("one", None)], None),
    ];

    let error = group_messages(&messages).expect_err("one result is missing");

    assert_eq!(
        error,
        ProtocolError::MissingResults {
            assistant_index: 0,
            call_ids: vec!["two".to_string()],
        }
    );
}

#[test]
fn rejects_unknown_and_orphan_tool_results() {
    let unknown = vec![
        assistant_with_calls(&[("one", None)]),
        result_message(&[("other", None)], None),
    ];
    let orphan = vec![result_message(&[("one", None)], None)];

    assert_eq!(
        group_messages(&unknown),
        Err(ProtocolError::UnknownResult {
            message_index: 1,
            result_id: "other".to_string(),
        })
    );
    assert_eq!(
        group_messages(&orphan),
        Err(ProtocolError::OrphanResult { message_index: 0 })
    );
}

#[test]
fn rejects_duplicate_call_and_result_ids() {
    let duplicate_calls = vec![assistant_with_calls(&[("same", None), ("same", None)])];
    let duplicate_results = vec![
        assistant_with_calls(&[("one", None)]),
        result_message(&[("one", None), ("one", None)], None),
    ];

    assert_eq!(
        group_messages(&duplicate_calls),
        Err(ProtocolError::DuplicateCallId {
            assistant_index: 0,
            call_id: "same".to_string(),
        })
    );
    assert_eq!(
        group_messages(&duplicate_results),
        Err(ProtocolError::DuplicateResult {
            message_index: 1,
            result_id: "one".to_string(),
        })
    );
}

#[test]
fn checks_provider_call_id_when_both_sides_supply_it() {
    let messages = vec![
        assistant_with_calls(&[("one", Some("provider-one"))]),
        result_message(&[("one", Some("wrong"))], None),
    ];

    assert_eq!(
        group_messages(&messages),
        Err(ProtocolError::CallIdMismatch {
            message_index: 1,
            result_id: "one".to_string(),
            expected: "provider-one".to_string(),
            actual: "wrong".to_string(),
        })
    );
}

#[test]
fn accepts_missing_result_call_id_when_primary_id_matches() {
    let messages = vec![
        assistant_with_calls(&[("one", Some("provider-one"))]),
        result_message(&[("one", None)], None),
    ];

    let groups = group_messages(&messages).expect("secondary result call_id is optional");

    assert_eq!(flatten(&groups), messages);
}

#[test]
fn accepts_result_call_id_when_assistant_omits_it_and_primary_id_matches() {
    let messages = vec![
        assistant_with_calls(&[("one", None)]),
        result_message(&[("one", Some("provider-one"))], None),
    ];

    let groups = group_messages(&messages).expect("secondary assistant call_id is optional");

    assert_eq!(flatten(&groups), messages);
}

#[test]
fn accepts_synthetic_result_as_an_ordinary_complete_result() {
    let messages = vec![
        assistant_with_calls(&[("cancelled", None)]),
        result_message(&[("cancelled", None)], None),
    ];

    let groups = group_messages(&messages).expect("synthetic results close the protocol");

    assert!(matches!(
        groups.as_slice(),
        [ProtocolGroup::ToolExchange { .. }]
    ));
}
