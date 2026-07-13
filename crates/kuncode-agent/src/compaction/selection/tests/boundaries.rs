use kuncode_core::{
    completion::{AssistantContent, Message, ToolResult, ToolResultContent, UserContent},
    non_empty_vec::NonEmptyVec,
};

use super::history;
use crate::compaction::{
    protocol::{
        HumanMessageIndex, ProtectedRecentTail, ProtocolGroup, group_messages,
        select_protected_recent_tail,
    },
    selection::{
        CandidateLoad, SelectionError, SelectionLimits, SelectionOutcome, select_prefix_tail,
    },
};

#[test]
fn exact_target_with_safe_prefix_skips_summary() {
    // Given: closed history with a non-empty old prefix.
    let messages = history();
    let groups = group_messages(&messages).expect("ordinary history should group");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1)
        .expect("history should have a protected tail");

    // When: deterministic projection reaches the exact optimization target.
    let outcome = select_prefix_tail(
        &groups,
        &messages,
        &protected,
        &[HumanMessageIndex(0)],
        SelectionLimits::new(50, 75).expect("limits should be ordered"),
        50,
    )
    .expect("closed history should be selectable");

    // Then: later lossy passes stop before semantic summary.
    assert_eq!(
        outcome,
        SelectionOutcome::DeterministicCandidate {
            load: CandidateLoad::TargetReached,
        }
    );
}

#[test]
fn human_anchor_comes_from_authoritative_messages_not_projected_prefix() {
    // Given: a deterministic projection changed old prefix content only.
    let messages = history();
    let mut groups = group_messages(&messages).expect("ordinary history should group");
    groups[0] = ProtocolGroup::Message(Message::user("projected surrogate"));
    let protected = select_protected_recent_tail(&groups, 0, |_| 1)
        .expect("history should have a protected tail");

    // When: selection builds the summary input and current-request anchor.
    let outcome = select_prefix_tail(
        &groups,
        &messages,
        &protected,
        &[HumanMessageIndex(0)],
        SelectionLimits::new(50, 75).expect("limits should be ordered"),
        60,
    )
    .expect("prefix-only projection should be selectable");

    // Then: candidate prefix is summarized but the anchor remains exact source text.
    let SelectionOutcome::Summarize(selection) = outcome else {
        panic!("safe old prefix should require summary");
    };
    assert_eq!(selection.summarize(), &groups[..2]);
    assert_eq!(
        selection.current_request_anchor(),
        Some(&Message::user("fix it"))
    );
}

#[test]
fn changed_protected_tail_is_rejected_before_summary() {
    // Given: candidate groups changed a message inside the protected suffix.
    let messages = history();
    let mut groups = group_messages(&messages).expect("ordinary history should group");
    groups[2] = ProtocolGroup::Message(Message::assistant("rewritten recent"));
    let protected = select_protected_recent_tail(&groups, 0, |_| 1)
        .expect("history should have a protected tail");

    // When: selection validates candidate retention against authoritative history.
    let result = select_prefix_tail(
        &groups,
        &messages,
        &protected,
        &[HumanMessageIndex(0)],
        SelectionLimits::new(50, 75).expect("limits should be ordered"),
        75,
    );

    // Then: protected-tail loss fails closed without producing a selection.
    assert_eq!(result, Err(SelectionError::ProtectedTailChanged));
}

#[test]
fn forged_tail_cannot_omit_the_latest_tool_exchange() {
    let messages = vec![
        Message::Assistant {
            id: None,
            content: NonEmptyVec::new(AssistantContent::tool_call(
                "call",
                "read_file",
                serde_json::json!({"path": "src/lib.rs"}),
            )),
        },
        Message::User {
            content: NonEmptyVec::new(UserContent::ToolResult(ToolResult {
                id: "call".to_string(),
                call_id: None,
                content: NonEmptyVec::new(ToolResultContent::text("result")),
            })),
        },
        Message::assistant("after tools"),
    ];
    let groups = group_messages(&messages).expect("fixture should be canonical");
    let protected = ProtectedRecentTail {
        group_range: 1..2,
        estimated_tokens: 1,
        budget_tokens: 1,
    };

    let result = select_prefix_tail(
        &groups,
        &messages,
        &protected,
        &[],
        SelectionLimits::new(50, 75).expect("limits should be ordered"),
        60,
    );

    assert_eq!(result, Err(SelectionError::InvalidProtectedTail));
}
