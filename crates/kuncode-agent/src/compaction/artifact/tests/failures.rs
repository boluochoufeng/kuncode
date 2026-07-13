use super::support::{FixedCounter, RejectingStore, tool_exchange, tool_exchange_with_text};
use crate::{
    compaction::{
        artifact::{
            ArtifactSpillError, ArtifactSpillFailure, ArtifactSpillInput, ArtifactSpillOutcome,
            spill_artifacts,
        },
        protocol::{
            ProtectedRecentTail, ProtocolGroup, group_messages, select_protected_recent_tail,
        },
    },
    session::AgentSession,
};
use kuncode_core::completion::Message;

#[test]
fn rejects_non_durable_session_and_empty_protected_suffix_before_replay() {
    // Given: canonical groups and a store that records every replay or write.
    let messages = [
        tool_exchange("old", "bash", "payload"),
        tool_exchange("recent", "read_file", "recent"),
    ]
    .concat();
    let groups = group_messages(&messages).expect("history should be valid");
    let non_durable = AgentSession::from_messages(messages.clone());
    let (store, durable) = RejectingStore::with_messages(&messages);
    let protected = ProtectedRecentTail {
        group_range: groups.len()..groups.len(),
        estimated_tokens: 0,
        budget_tokens: 0,
    };

    // When: callers provide no session authority or omit the mandatory recent group.
    let missing_authority = ArtifactSpillInput::new(&groups, &protected, &non_durable);
    let empty_protection = ArtifactSpillInput::new(&groups, &protected, &durable);

    // Then: validation fails before the authoritative store seam is reachable.
    assert!(matches!(
        missing_authority,
        Err(ArtifactSpillError::NonDurableSession)
    ));
    assert!(matches!(
        empty_protection,
        Err(ArtifactSpillError::InvalidProtectedTail)
    ));
    assert_eq!(store.replay_calls(), 0);
    assert_eq!(store.calls(), 0);
}

#[test]
fn rejects_manually_constructed_open_exchange() {
    // Given: a public ProtocolGroup value whose assistant call has no result.
    let assistant = tool_exchange("open", "bash", "unused").remove(0);
    let messages = vec![assistant.clone()];
    let (_store, session) = RejectingStore::with_messages(&messages);
    let groups = vec![ProtocolGroup::ToolExchange {
        assistant,
        results: Vec::new(),
    }];
    let protected = ProtectedRecentTail {
        group_range: 0..1,
        estimated_tokens: 1,
        budget_tokens: 1,
    };

    // When: the boundary reconstructs and validates the protocol.
    let result = ArtifactSpillInput::new(&groups, &protected, &session);

    // Then: the open exchange is rejected rather than treated as durable.
    assert!(matches!(
        result,
        Err(ArtifactSpillError::InvalidProtocol(_))
    ));
}

#[test]
fn rejects_groups_that_are_not_the_active_session_context() {
    // Given: canonical groups from a different context than the durable session owns.
    let active = [
        tool_exchange("old", "bash", "active"),
        tool_exchange("recent", "read_file", "recent"),
    ]
    .concat();
    let supplied = [
        tool_exchange("old", "bash", "different"),
        tool_exchange("recent", "read_file", "recent"),
    ]
    .concat();
    let (store, session) = RejectingStore::with_messages(&active);
    let groups = group_messages(&supplied).expect("supplied context should be canonical");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1).expect("tail should exist");

    // When: a caller tries to borrow durable authority for unrelated groups.
    let result = ArtifactSpillInput::new(&groups, &protected, &session);

    // Then: validation fails before replay or artifact persistence.
    assert!(matches!(
        result,
        Err(ArtifactSpillError::ActiveSessionMismatch)
    ));
    assert_eq!(store.replay_calls(), 0);
    assert_eq!(store.calls(), 0);
}

#[test]
fn rejects_tail_that_omits_the_latest_tool_exchange() {
    // Given: a tool exchange followed by an ordinary message and a forged suffix.
    let messages = [
        tool_exchange("old", "bash", "payload"),
        vec![Message::user("after")],
    ]
    .concat();
    let (store, session) = RejectingStore::with_messages(&messages);
    let groups = group_messages(&messages).expect("active context should be canonical");
    let protected = ProtectedRecentTail {
        group_range: 1..2,
        estimated_tokens: 1,
        budget_tokens: 1,
    };

    // When: the suffix excludes the mandatory latest tool exchange.
    let result = ArtifactSpillInput::new(&groups, &protected, &session);

    // Then: validation rejects it without reaching durable storage.
    assert!(matches!(
        result,
        Err(ArtifactSpillError::InvalidProtectedTail)
    ));
    assert_eq!(store.replay_calls(), 0);
    assert_eq!(store.calls(), 0);
}

#[tokio::test]
async fn preserves_inline_payload_when_parsing_or_storage_fails() {
    // Given: two independently invalid old results before a protected exchange.
    let messages = [
        tool_exchange_with_text("invalid", "bash", "not JSON"),
        tool_exchange("store", "bash", "valid payload"),
        tool_exchange("recent", "read_file", "recent"),
    ]
    .concat();
    let (store, session) = RejectingStore::with_messages(&messages);
    let groups = group_messages(session.messages()).expect("history should be valid");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1).expect("tail should exist");
    let input =
        ArtifactSpillInput::new(&groups, &protected, &session).expect("input should be valid");

    // When: parsing fails for one item and durable storage rejects the other.
    let result = spill_artifacts(input, &store, &FixedCounter::new(9_000, 100))
        .await
        .expect("journal audit should pass");

    // Then: both original groups remain inline and failures stay isolated.
    assert_eq!(result.groups(), groups);
    assert!(matches!(
        result.outcomes(),
        [
            ArtifactSpillOutcome::Failed {
                failure: ArtifactSpillFailure::Parse(_),
                ..
            },
            ArtifactSpillOutcome::Failed {
                failure: ArtifactSpillFailure::Store(_),
                ..
            }
        ]
    ));
    assert_eq!(store.calls(), 1);
}

#[tokio::test]
async fn aborts_spill_when_artifact_commit_outcome_is_unknown() {
    // Given: two spillable results before the protected exchange and an uncertain store commit.
    let messages = [
        tool_exchange("old-a", "bash", "first payload"),
        tool_exchange("old-b", "bash", "second payload"),
        tool_exchange("recent", "read_file", "recent"),
    ]
    .concat();
    let (store, session) = RejectingStore::with_unknown_commit(&messages);
    let groups = group_messages(session.messages()).expect("history should be valid");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1).expect("tail should exist");
    let input =
        ArtifactSpillInput::new(&groups, &protected, &session).expect("input should be valid");

    // When: the first artifact write may have committed without returning its receipt.
    let result = spill_artifacts(input, &store, &FixedCounter::new(9_000, 100)).await;

    // Then: the whole pass stops immediately instead of recording an isolated item failure.
    assert!(matches!(
        result,
        Err(ArtifactSpillError::PersistenceOutcomeUnknown {
            operation: "put test artifact",
            message,
        }) if message == "injected uncertain commit"
    ));
    assert_eq!(store.calls(), 1);
}

#[tokio::test]
async fn keeps_inline_when_marker_metadata_cannot_fit() {
    // Given: an exact call id that alone exceeds the marker budget.
    let long_id = "x".repeat(10_000);
    let messages = [
        tool_exchange(&long_id, "bash", "payload"),
        tool_exchange("recent", "read_file", "recent"),
    ]
    .concat();
    let (store, session) = RejectingStore::with_messages(&messages);
    let groups = group_messages(session.messages()).expect("history should be valid");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1).expect("tail should exist");
    let input =
        ArtifactSpillInput::new(&groups, &protected, &session).expect("input should be valid");

    // When: the provider counter rejects every marker size.
    let result = spill_artifacts(input, &store, &FixedCounter::new(9_000, 3_000))
        .await
        .expect("journal audit should pass");

    // Then: no dangling artifact is written and the complete result remains inline.
    assert_eq!(result.groups(), groups);
    assert!(matches!(
        result.outcomes(),
        [ArtifactSpillOutcome::Failed {
            failure: ArtifactSpillFailure::MarkerTooLarge,
            ..
        }]
    ));
    assert_eq!(store.calls(), 0);
}
