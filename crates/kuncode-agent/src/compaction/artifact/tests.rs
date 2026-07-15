use std::sync::Arc;

use crate::{
    compaction::{
        artifact::{
            ArtifactResultLocation, ArtifactSpillFailure, ArtifactSpillInput, ArtifactSpillOutcome,
        },
        protocol::{group_messages, select_protected_recent_tail},
        slimming::slim_tool_results,
    },
    session_store::{NewSession, Seq, SessionStore, turso::TursoSessionStore},
    test_support::TestDir,
    tool::ToolOutput,
};

mod checkpoint;
mod concurrency;
mod durability;
mod failures;
mod metadata;
mod multi;
mod receipt;
mod summary_binding;
mod support;

use support::{AdaptiveMarkerCounter, FixedCounter, persisted_session, tool_exchange};

#[tokio::test]
async fn spills_only_when_result_exceeds_threshold() {
    // Given: an old durable exchange and a protected recent exchange.
    let root = TestDir::new();
    let store = TursoSessionStore::open(root.path().join("sessions.db"))
        .await
        .expect("store should open");
    let session_id = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let messages = [
        tool_exchange("old", "bash", "old output"),
        tool_exchange("recent", "read_file", "recent output"),
    ]
    .concat();
    let session = persisted_session(&store, session_id, &messages).await;
    let groups = group_messages(session.messages()).expect("exchanges should be closed");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1).expect("tail should exist");
    let frontier = session.durable_seq().expect("session should be durable");

    // When: the old result is one token above the strict threshold.
    let input = ArtifactSpillInput::new(&groups, &protected, &session)
        .expect("spill boundaries should be valid");
    let result = super::spill_artifacts(input, &store, &FixedCounter::new(8_193, 200))
        .await
        .expect("journal audit should pass");

    // Then: only the old exchange changes and its receipt advances the frontier.
    assert_ne!(result.groups()[0], groups[0]);
    assert_eq!(result.groups()[1], groups[1]);
    assert!(result.frontier() > frontier);
    assert!(matches!(
        result.outcomes(),
        [ArtifactSpillOutcome::Spilled { .. }]
    ));
}

#[tokio::test]
async fn keeps_result_at_exact_threshold_inline_and_authorizes_slimming() {
    // Given: one durable old exchange before a protected tool exchange.
    let root = TestDir::new();
    let store = TursoSessionStore::open(root.path().join("sessions.db"))
        .await
        .expect("store should open");
    let session_id = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let old_payload =
        ToolOutput::success(serde_json::json!({"stdout": "payload"})).to_model_content();
    let messages = [
        tool_exchange("old", "bash", &old_payload),
        tool_exchange("recent", "read_file", "recent payload"),
    ]
    .concat();
    let session = persisted_session(&store, session_id, &messages).await;
    let groups = group_messages(session.messages()).expect("history should be valid");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1).expect("tail should exist");
    let frontier = session.durable_seq().expect("session should be durable");

    // When: the provider-visible count equals the threshold.
    let input = ArtifactSpillInput::new(&groups, &protected, &session)
        .expect("spill boundaries should be valid");
    let result = super::spill_artifacts(input, &store, &FixedCounter::new(8_192, 100))
        .await
        .expect("journal audit should pass");

    // Then: strict eligibility leaves the payload unchanged.
    assert_eq!(result.groups(), groups);
    assert_eq!(result.frontier(), frontier);
    assert!(matches!(
        result.outcomes(),
        [ArtifactSpillOutcome::BelowThreshold(authorization)]
            if authorization.tokens() == 8_192
                && authorization.source_journal_seq() == Some(Seq::new(2))
    ));
    let location = ArtifactResultLocation {
        group_index: 0,
        result_message_index: 0,
        content_index: 0,
    };
    let slimmed = slim_tool_results(
        &result,
        &protected,
        &[location],
        &FixedCounter::new(100, 100),
    )
    .await
    .expect("same-pass authorization should slim the journal-backed result");
    assert_ne!(slimmed.groups()[0], groups[0]);
    assert_eq!(slimmed.groups()[1], groups[1]);
}

#[tokio::test]
async fn preserves_payload_when_counting_fails() {
    // Given: an eligible durable exchange and a counter failure.
    let root = TestDir::new();
    let store = TursoSessionStore::open(root.path().join("sessions.db"))
        .await
        .expect("store should open");
    let session_id = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let messages = [
        tool_exchange("old", "bash", "payload"),
        tool_exchange("recent", "read_file", "recent payload"),
    ]
    .concat();
    let session = persisted_session(&store, session_id, &messages).await;
    let groups = group_messages(session.messages()).expect("history should be valid");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1).expect("tail should exist");
    let counter = FixedCounter::failing("provider unavailable");

    // When: spill evaluates the failing item.
    let input = ArtifactSpillInput::new(&groups, &protected, &session)
        .expect("spill boundaries should be valid");
    let result = super::spill_artifacts(input, &store, &counter)
        .await
        .expect("journal audit should pass");

    // Then: the original payload survives and the failure is structured.
    assert_eq!(result.groups(), groups);
    assert!(matches!(
        result.outcomes(),
        [ArtifactSpillOutcome::Failed {
            failure: ArtifactSpillFailure::Count(_),
            ..
        }]
    ));
}

#[tokio::test]
async fn repeated_spill_reuses_the_same_artifact_receipt() {
    // Given: the same durable source is evaluated twice.
    let root = TestDir::new();
    let store = Arc::new(
        TursoSessionStore::open(root.path().join("sessions.db"))
            .await
            .expect("store should open"),
    );
    let session_id = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let messages = [
        tool_exchange("old-a", "bash", &"payload".repeat(4_000)),
        tool_exchange("old-b", "read_file", &"payload".repeat(4_000)),
        tool_exchange("recent", "read_file", "recent payload"),
    ]
    .concat();
    let mut session = persisted_session(store.as_ref(), session_id, &messages).await;
    let groups = group_messages(session.messages()).expect("history should be valid");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1).expect("tail should exist");
    // When: counters choose different marker previews for the same complete payload.
    let first_input = ArtifactSpillInput::new(&groups, &protected, &session)
        .expect("first input should be valid");
    let first = super::spill_artifacts(first_input, store.as_ref(), &AdaptiveMarkerCounter::new(1))
        .await
        .expect("first journal audit should pass");
    session.advance_durable_seq(first.frontier());
    let second_input = ArtifactSpillInput::new(&groups, &protected, &session)
        .expect("second input should be valid");
    let second =
        super::spill_artifacts(second_input, store.as_ref(), &AdaptiveMarkerCounter::new(8))
            .await
            .expect("second journal audit should pass");

    // Then: artifact identity and durable receipt remain identical.
    assert_eq!(first.frontier(), second.frontier());
    assert_ne!(first.groups(), second.groups());
    let [
        ArtifactSpillOutcome::Spilled {
            artifact_id: first_a,
            ..
        },
        ArtifactSpillOutcome::Spilled {
            artifact_id: first_b,
            ..
        },
    ] = first.outcomes()
    else {
        panic!("first pass should spill both metadata variants");
    };
    let [
        ArtifactSpillOutcome::Spilled {
            artifact_id: second_a,
            ..
        },
        ArtifactSpillOutcome::Spilled {
            artifact_id: second_b,
            ..
        },
    ] = second.outcomes()
    else {
        panic!("second pass should reuse both metadata variants");
    };
    assert_eq!(first_a, first_b);
    assert_eq!(first_a, second_a);
    assert_eq!(second_a, second_b);
}
