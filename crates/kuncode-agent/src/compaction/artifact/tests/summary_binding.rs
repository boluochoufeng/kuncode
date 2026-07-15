use crate::{
    compaction::{
        artifact::{ArtifactSpillInput, ArtifactSpillOutcome, spill_artifacts},
        protocol::{group_messages, select_protected_recent_tail},
        selection::{SelectionLimits, SelectionOutcome, select_prefix_tail},
    },
    session_store::{NewSession, SessionStore, turso::TursoSessionStore},
    test_support::TestDir,
};

use super::support::{FixedCounter, persisted_session, tool_exchange};

#[tokio::test]
async fn spilled_artifacts_authorize_a_summary_at_the_advanced_frontier() {
    // The summary must bind to the artifact receipt frontier; the earlier audit
    // frontier does not cover the durable payload named by the marker.
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
    let groups = group_messages(session.messages()).expect("history should be closed");
    let protected = select_protected_recent_tail(&groups, 0, |_| 1).expect("tail should exist");
    let source_frontier = session.durable_seq().expect("session should be durable");
    let input = ArtifactSpillInput::new(&groups, &protected, &session)
        .expect("spill input should be valid");

    let artifacts = spill_artifacts(input, &store, &FixedCounter::new(8_193, 200))
        .await
        .expect("spill should commit");
    assert!(artifacts.frontier() > source_frontier);
    let [ArtifactSpillOutcome::Spilled { artifact_id, .. }] = artifacts.outcomes() else {
        panic!("old result should spill");
    };
    let outcome = select_prefix_tail(
        artifacts.groups(),
        session.messages(),
        &protected,
        &[],
        SelectionLimits::new(10, 100).expect("limits should be valid"),
        50,
    )
    .expect("selection should be valid");
    let SelectionOutcome::Summarize(selection) = outcome else {
        panic!("old exchange should require summary");
    };

    let request = session
        .issue_summary_request(&artifacts, &selection)
        .expect("artifact receipts may advance beyond the source frontier");
    let output = serde_json::json!({
        "schema_version": 1,
        "source_seq_start": 1,
        "source_seq_end": 2,
        "current_goal": "continue",
        "constraints": [],
        "decisions": [],
        "completed_work": [],
        "workspace": {"working_directory": "unknown", "files": [], "symbols": []},
        "commands_and_tests": [],
        "unresolved_errors": [],
        "todos": [],
        "next_actions": [],
        "artifact_refs": [artifact_id],
    });
    let parsed = request
        .parse_and_validate(&output.to_string())
        .expect("same-batch artifact reference should be authorized");
    assert_eq!(parsed.artifact_refs, vec![artifact_id.clone()]);
}
