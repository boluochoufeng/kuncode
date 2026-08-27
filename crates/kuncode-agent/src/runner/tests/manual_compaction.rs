use super::support::{
    AgentSession, Arc, AssistantContent, CancellationToken, CollectingObserver, CompactionMode,
    CompletionRequest, EventKind, FakeModel, FixedRunnerGroupEstimator, Message, NewSession, Seq,
    SessionStore, TestDir, TokenCountPrecision, TokenEstimate, TokenEstimationError,
    TokenEstimator, TursoSessionStore, async_trait, configured_runner, event_label, response,
};

// `/compact`: a user request runs the same pipeline as budget pressure, minus
// the pressure precondition. The configured target still gates it, so a context
// that is already small enough reports that instead of spending a summary call.
//
// The test config is a 1000-token window with 100 reserved, so usable capacity
// is 900: the target (0.50) sits at 450 and the soft threshold (0.75) at 675.

use crate::runner::ManualCompaction;
use crate::session_store::{JournalKind, NewJournalEntry};

/// Uncompacted context at 600 tokens — above the target, below the threshold,
/// which is exactly the band where only a manual request does anything.
struct BelowThresholdEstimator;

#[async_trait]
impl TokenEstimator for BelowThresholdEstimator {
    async fn estimate(
        &self,
        request: &CompletionRequest,
    ) -> Result<TokenEstimate, TokenEstimationError> {
        let tokens = if request
            .chat_history
            .iter()
            .any(crate::compaction::summary::is_compacted_context_message)
        {
            300
        } else {
            600
        };
        Ok(TokenEstimate::new(tokens, TokenCountPrecision::Exact))
    }
}

/// Every projection sits under the target, so there is nothing to reclaim.
struct UnderTargetEstimator;

#[async_trait]
impl TokenEstimator for UnderTargetEstimator {
    async fn estimate(
        &self,
        _request: &CompletionRequest,
    ) -> Result<TokenEstimate, TokenEstimationError> {
        Ok(TokenEstimate::new(400, TokenCountPrecision::Exact))
    }
}

async fn durable_session(store: &TursoSessionStore, root: &TestDir) -> AgentSession {
    let session_id = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let mut session = AgentSession::new();
    session
        .attach_session_id(session_id)
        .expect("fresh session should attach");
    let id = session.session_id().cloned().expect("attached id");
    // The trailing human message is the protected recent tail, so the
    // summarizable prefix is exactly seq 1..3 — what `summary_json` declares.
    for (message, human) in [
        (Message::user("fix the old failure"), true),
        (Message::assistant("inspected old implementation"), false),
        (Message::assistant("recent old response"), false),
        (Message::user("implement the next change"), true),
    ] {
        let seq = store
            .append(
                &id,
                NewJournalEntry::message(&message).expect("history should encode"),
            )
            .await
            .expect("history should persist");
        if human {
            session.push_human_with_journal_seq(message, Some(seq));
        } else {
            session.push_with_journal_seq(message, Some(seq));
        }
    }
    session
}

#[tokio::test]
async fn compact_now_compacts_a_context_the_automatic_path_would_skip() {
    // Given
    let root = TestDir::new();
    let store = Arc::new(
        TursoSessionStore::open(root.path().join("sessions.db"))
            .await
            .expect("store should open"),
    );
    let mut session = durable_session(&store, &root).await;
    // Two identical responses: the summarizer may make a second attempt, and a
    // starved queue would panic rather than fail the assertion under test.
    let model = FakeModel::new([
        response(AssistantContent::text(summary_json())),
        response(AssistantContent::text(summary_json())),
    ]);
    let observer = Arc::new(CollectingObserver::default());
    let mut runner = configured_runner(model.clone(), CompactionMode::Enabled)
        .with_session_store(store.clone())
        .with_observer(observer.clone());
    runner.token_estimator = Arc::new(BelowThresholdEstimator);
    runner.group_estimator = Arc::new(FixedRunnerGroupEstimator(100));

    // When
    let outcome = runner
        .compact_now(&mut session, &CancellationToken::new())
        .await
        .expect("a manual request under the threshold should still compact");

    // Then
    let events = observer.events();
    assert_eq!(
        events
            .iter()
            .map(|event| event_label(&event.kind))
            .collect::<Vec<_>>(),
        ["compaction_started", "compaction_completed"],
        "{:#?}",
        events.iter().map(|event| &event.kind).collect::<Vec<_>>(),
    );
    assert_eq!(outcome, ManualCompaction::Compacted);
    assert!(
        matches!(
            &events[0].kind,
            EventKind::CompactionStarted { reason, before_tokens: 600, .. }
                if reason == "manual_request"
        ),
        "{:?}",
        events[0].kind,
    );
    assert!(
        matches!(
            &events[1].kind,
            EventKind::CompactionCompleted {
                before_tokens: 600,
                after_tokens: 300,
                ..
            }
        ),
        "{:?}",
        events[1].kind,
    );
    let session_id = session.session_id().cloned().expect("durable session");
    assert!(
        store
            .latest_checkpoint(&session_id)
            .await
            .expect("checkpoint read should succeed")
            .is_some(),
        "a manual compaction commits like an automatic one",
    );
    // The audit record must not claim a threshold that was never crossed.
    let journal = store
        .replay_after(&session_id, Seq::ZERO)
        .await
        .expect("journal replay should succeed");
    let commit = journal
        .iter()
        .find(|entry| entry.kind == JournalKind::Compaction.as_str())
        .expect("a commit record should be journaled");
    assert_eq!(commit.payload_json["reason"], "manual");
}

#[tokio::test]
async fn compact_now_reports_nothing_to_do_under_the_target() {
    // Given
    let root = TestDir::new();
    let store = Arc::new(
        TursoSessionStore::open(root.path().join("sessions.db"))
            .await
            .expect("store should open"),
    );
    let mut session = durable_session(&store, &root).await;
    let observer = Arc::new(CollectingObserver::default());
    let mut runner = configured_runner(FakeModel::new([]), CompactionMode::Enabled)
        .with_session_store(store.clone())
        .with_observer(observer.clone());
    runner.token_estimator = Arc::new(UnderTargetEstimator);
    runner.group_estimator = Arc::new(FixedRunnerGroupEstimator(100));

    // When
    let outcome = runner
        .compact_now(&mut session, &CancellationToken::new())
        .await
        .expect("an unnecessary request is not a failure");

    // Then
    assert_eq!(outcome, ManualCompaction::NotNeeded);
    let events = observer.events();
    assert!(
        matches!(
            &events[0].kind,
            EventKind::CompactionSkipped { reason, before_tokens: 400, .. }
                if reason == "manual_below_target"
        ),
        "{:?}",
        events[0].kind,
    );
    // No summary was requested, so nothing was spent finding that out.
    assert_eq!(events.len(), 1);
    let session_id = session.session_id().cloned().expect("durable session");
    assert!(
        store
            .latest_checkpoint(&session_id)
            .await
            .expect("checkpoint read should succeed")
            .is_none(),
        "a skipped request must not touch durable state",
    );
}

#[tokio::test]
async fn compact_now_is_unavailable_unless_compaction_is_enabled() {
    for mode in [CompactionMode::Disabled, CompactionMode::Shadow] {
        // Given
        let root = TestDir::new();
        let store = Arc::new(
            TursoSessionStore::open(root.path().join("sessions.db"))
                .await
                .expect("store should open"),
        );
        let mut session = durable_session(&store, &root).await;
        let observer = Arc::new(CollectingObserver::default());
        let runner = configured_runner(FakeModel::new([]), mode)
            .with_session_store(store.clone())
            .with_observer(observer.clone());

        // When
        let outcome = runner
            .compact_now(&mut session, &CancellationToken::new())
            .await
            .expect("an unavailable mode is reported, not an error");

        // Then
        assert!(
            matches!(outcome, ManualCompaction::Unavailable { .. }),
            "{mode:?} should refuse to replace the context: {outcome:?}",
        );
        assert!(
            observer.events().is_empty(),
            "an unavailable mode does nothing worth reporting as compaction",
        );
    }
}

fn summary_json() -> String {
    serde_json::json!({
        "schema_version": 1,
        "source_seq_start": 1,
        "source_seq_end": 3,
        "current_goal": "continue the paused work",
        "constraints": [],
        "decisions": [],
        "completed_work": [],
        "workspace": {
            "working_directory": "/workspace",
            "files": [],
            "symbols": []
        },
        "commands_and_tests": [],
        "unresolved_errors": [],
        "todos": [],
        "next_actions": [],
        "artifact_refs": []
    })
    .to_string()
}
