use super::support::{
    AgentError, AgentSession, Arc, AssistantContent, CollectingObserver, CompactionMode, EventKind,
    FakeModel, FixedRunnerGroupEstimator, LARGE_RESULT_BYTES, Message, NewSession, NonEmptyVec,
    ScriptedRequestEstimator, Seq, SessionId, SessionStore, TestDir, ToolOutput, TursoSessionStore,
    async_trait, configured_runner, response,
};

// Adversarial persistence tests for authority-invalidating compaction failures.
//
// Even under soft pressure, ambiguous writes or corrupted durable facts must
// poison the session and prevent a provider request from using stale context.

#[derive(Clone, Copy)]
enum AmbiguousWrite {
    Artifact,
    ArtifactConflict,
    ArtifactHeadIntegrity,
    Compaction,
}

struct AmbiguousStore {
    inner: Arc<TursoSessionStore>,
    write: AmbiguousWrite,
}

#[async_trait]
impl SessionStore for AmbiguousStore {
    async fn create_session(
        &self,
        session: NewSession,
    ) -> Result<SessionId, crate::session_store::SessionStoreError> {
        self.inner.create_session(session).await
    }

    async fn append(
        &self,
        session: &SessionId,
        entry: crate::session_store::NewJournalEntry,
    ) -> Result<Seq, crate::session_store::SessionStoreError> {
        self.inner.append(session, entry).await
    }

    async fn put_tool_artifact(
        &self,
        session: &SessionId,
        expected_journal_head: Seq,
        artifact: crate::session_store::NewToolArtifact,
    ) -> Result<crate::session_store::CommittedArtifact, crate::session_store::SessionStoreError>
    {
        match self.write {
            AmbiguousWrite::Artifact => ambiguous("tool artifact"),
            AmbiguousWrite::ArtifactConflict => Err(
                crate::session_store::SessionStoreError::JournalHeadConflict {
                    expected: expected_journal_head.get(),
                    actual: expected_journal_head.get() + 1,
                },
            ),
            AmbiguousWrite::ArtifactHeadIntegrity | AmbiguousWrite::Compaction => {
                self.inner
                    .put_tool_artifact(session, expected_journal_head, artifact)
                    .await
            }
        }
    }

    async fn latest_checkpoint(
        &self,
        session: &SessionId,
    ) -> Result<Option<crate::session_store::Checkpoint>, crate::session_store::SessionStoreError>
    {
        self.inner.latest_checkpoint(session).await
    }

    async fn write_checkpoint(
        &self,
        checkpoint: crate::session_store::NewCheckpoint,
    ) -> Result<Seq, crate::session_store::SessionStoreError> {
        self.inner.write_checkpoint(checkpoint).await
    }

    async fn commit_compaction(
        &self,
        commit: crate::session_store::NewCompactionCommit,
    ) -> Result<crate::session_store::CommittedCompaction, crate::session_store::SessionStoreError>
    {
        match self.write {
            AmbiguousWrite::Artifact
            | AmbiguousWrite::ArtifactConflict
            | AmbiguousWrite::ArtifactHeadIntegrity => self.inner.commit_compaction(commit).await,
            AmbiguousWrite::Compaction => ambiguous("compaction"),
        }
    }

    async fn replay_after(
        &self,
        session: &SessionId,
        seq: Seq,
    ) -> Result<Vec<crate::session_store::JournalEntry>, crate::session_store::SessionStoreError>
    {
        self.inner.replay_after(session, seq).await
    }

    async fn journal_snapshot(
        &self,
        session: &SessionId,
        seqs: &[Seq],
    ) -> Result<crate::session_store::JournalSnapshot, crate::session_store::SessionStoreError>
    {
        let snapshot = self.inner.journal_snapshot(session, seqs).await?;
        if matches!(self.write, AmbiguousWrite::ArtifactHeadIntegrity) {
            let connection = self.inner.connection_for_test().await;
            connection
                .execute(
                    "UPDATE journal_entries SET seq = 'not-an-integer' \
                     WHERE session_id = ?1 AND seq = \
                       (SELECT MAX(seq) FROM journal_entries WHERE session_id = ?2)",
                    (session.as_str(), session.as_str()),
                )
                .await
                .expect("fixture should corrupt the journal head after the snapshot");
        }
        Ok(snapshot)
    }
}

fn ambiguous<T>(operation: &'static str) -> Result<T, crate::session_store::SessionStoreError> {
    Err(
        crate::session_store::SessionStoreError::CommitOutcomeUnknown {
            operation,
            message: "provider-secret-ambiguous-receipt".to_string(),
        },
    )
}

#[tokio::test]
async fn soft_artifact_unknown_outcome_blocks_without_model_request() {
    assert_unknown_outcome_blocks(AmbiguousWrite::Artifact).await;
}

#[tokio::test]
async fn soft_compaction_unknown_outcome_blocks_without_model_request() {
    assert_unknown_outcome_blocks(AmbiguousWrite::Compaction).await;
}

#[tokio::test]
async fn soft_artifact_cas_conflict_fails_closed_without_model_request() {
    // Given
    let root = TestDir::new();
    let inner = Arc::new(
        TursoSessionStore::open(root.path().join("sessions.db"))
            .await
            .expect("store should open"),
    );
    let session_id = inner
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let mut session = durable_tool_history(inner.as_ref(), &session_id).await;
    let model = FakeModel::new([response(AssistantContent::text("done"))]);
    let store = Arc::new(AmbiguousStore {
        inner,
        write: AmbiguousWrite::ArtifactConflict,
    });
    let mut runner =
        configured_runner(model.clone(), CompactionMode::Enabled).with_session_store(store);
    runner.token_estimator = Arc::new(ScriptedRequestEstimator);
    runner.group_estimator = Arc::new(FixedRunnerGroupEstimator(100));

    // When
    let error = runner
        .continue_session(&mut session)
        .await
        .expect_err("a stale journal frontier must fail closed");

    // Then
    assert!(matches!(error, AgentError::Compaction { .. }));
    assert!(model.requests().is_empty());
    assert!(!session.is_durable());
    assert!(session.take_persistence_error().is_some());
}

#[tokio::test]
async fn soft_mistyped_head_after_snapshot_fails_closed_without_model_request() {
    // Given
    let root = TestDir::new();
    let database = root.path().join("sessions.db");
    let inner = Arc::new(
        TursoSessionStore::open(&database)
            .await
            .expect("store should open"),
    );
    let session_id = inner
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let mut session = durable_tool_history(inner.as_ref(), &session_id).await;
    let store = Arc::new(AmbiguousStore {
        inner,
        write: AmbiguousWrite::ArtifactHeadIntegrity,
    });
    let model = FakeModel::new([response(AssistantContent::text("must not be requested"))]);
    let mut runner =
        configured_runner(model.clone(), CompactionMode::Enabled).with_session_store(store);
    runner.token_estimator = Arc::new(ScriptedRequestEstimator);
    runner.group_estimator = Arc::new(FixedRunnerGroupEstimator(100));

    // When
    let error = runner
        .continue_session(&mut session)
        .await
        .expect_err("a mistyped CAS head must terminate the loop");

    // Then
    assert!(matches!(error, AgentError::Compaction { .. }));
    assert!(model.requests().is_empty());
    assert!(!session.is_durable());
    assert!(session.take_persistence_error().is_some());
}

#[tokio::test]
async fn soft_stale_journal_audit_fails_closed_without_model_request() {
    // Given
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
    let mut session = durable_tool_history(store.as_ref(), &session_id).await;
    store
        .append(
            &session_id,
            crate::session_store::NewJournalEntry::raw(
                crate::session_store::JournalKind::SessionNote,
                serde_json::json!({"note": "concurrent durable fact"}),
            ),
        )
        .await
        .expect("concurrent fact should persist");
    let model = FakeModel::new([response(AssistantContent::text("must not be requested"))]);
    let mut runner =
        configured_runner(model.clone(), CompactionMode::Enabled).with_session_store(store);
    runner.token_estimator = Arc::new(ScriptedRequestEstimator);
    runner.group_estimator = Arc::new(FixedRunnerGroupEstimator(100));

    // When
    let error = runner
        .continue_session(&mut session)
        .await
        .expect_err("a stale audit frontier must terminate the loop");

    // Then
    assert!(matches!(error, AgentError::Compaction { .. }));
    assert!(model.requests().is_empty());
    assert!(!session.is_durable());
    assert!(session.take_persistence_error().is_some());
}

#[tokio::test]
async fn soft_journal_message_mismatch_fails_closed_without_model_request() {
    // Given
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
    let durable = [
        tool_exchange_message("old", &ToolOutput::success("durable").to_model_content()),
        tool_exchange_message("recent", &ToolOutput::success("recent").to_model_content()),
    ]
    .concat();
    let active = [
        tool_exchange_message("old", &ToolOutput::success("tampered").to_model_content()),
        tool_exchange_message("recent", &ToolOutput::success("recent").to_model_content()),
    ]
    .concat();
    let mut session = AgentSession::new();
    session
        .attach_session_id(session_id.clone())
        .expect("fresh session should attach");
    for (durable_message, active_message) in durable.into_iter().zip(active) {
        let seq = store
            .append(
                &session_id,
                crate::session_store::NewJournalEntry::message(&durable_message)
                    .expect("message should encode"),
            )
            .await
            .expect("message should persist");
        session.push_with_journal_seq(active_message, Some(seq));
    }
    let model = FakeModel::new([response(AssistantContent::text("must not be requested"))]);
    let mut runner =
        configured_runner(model.clone(), CompactionMode::Enabled).with_session_store(store);
    runner.token_estimator = Arc::new(ScriptedRequestEstimator);
    runner.group_estimator = Arc::new(FixedRunnerGroupEstimator(100));

    // When
    let error = runner
        .continue_session(&mut session)
        .await
        .expect_err("a journal message mismatch must terminate the loop");

    // Then
    assert!(matches!(error, AgentError::Compaction { .. }));
    assert!(model.requests().is_empty());
    assert!(!session.is_durable());
    assert!(session.take_persistence_error().is_some());
}

#[tokio::test]
async fn soft_corrupt_journal_message_fails_closed_without_model_request() {
    // Given
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
        tool_exchange_message("old", &ToolOutput::success("durable").to_model_content()),
        tool_exchange_message("recent", &ToolOutput::success("recent").to_model_content()),
    ]
    .concat();
    let mut session = AgentSession::new();
    session
        .attach_session_id(session_id.clone())
        .expect("fresh session should attach");
    for (index, message) in messages.into_iter().enumerate() {
        let entry = if index == 0 {
            crate::session_store::NewJournalEntry::raw(
                crate::session_store::JournalKind::Message,
                serde_json::json!({"schema_version": 1, "message": "corrupt"}),
            )
        } else {
            crate::session_store::NewJournalEntry::message(&message).expect("message should encode")
        };
        let seq = store
            .append(&session_id, entry)
            .await
            .expect("journal fact should persist");
        session.push_with_journal_seq(message, Some(seq));
    }
    let model = FakeModel::new([response(AssistantContent::text("must not be requested"))]);
    let mut runner =
        configured_runner(model.clone(), CompactionMode::Enabled).with_session_store(store);
    runner.token_estimator = Arc::new(ScriptedRequestEstimator);
    runner.group_estimator = Arc::new(FixedRunnerGroupEstimator(100));

    // When
    let error = runner
        .continue_session(&mut session)
        .await
        .expect_err("corrupt durable payload must terminate the loop");

    // Then
    assert!(matches!(error, AgentError::Compaction { .. }));
    assert!(model.requests().is_empty());
    assert!(!session.is_durable());
    assert!(session.take_persistence_error().is_some());
}

#[tokio::test]
async fn soft_undecodable_journal_row_fails_closed_without_model_request() {
    // Given
    let root = TestDir::new();
    let database = root.path().join("sessions.db");
    let store = Arc::new(
        TursoSessionStore::open(&database)
            .await
            .expect("store should open"),
    );
    let session_id = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let mut session = durable_tool_history(store.as_ref(), &session_id).await;
    {
        let connection = store.connection_for_test().await;
        connection
            .execute(
                "UPDATE journal_entries SET payload_json = X'FF' \
                 WHERE session_id = ?1 AND kind = 'message' AND seq = 1",
                [session_id.as_str()],
            )
            .await
            .expect("fixture should corrupt the durable journal row");
    }
    let model = FakeModel::new([response(AssistantContent::text("must not be requested"))]);
    let mut runner =
        configured_runner(model.clone(), CompactionMode::Enabled).with_session_store(store);
    runner.token_estimator = Arc::new(ScriptedRequestEstimator);
    runner.group_estimator = Arc::new(FixedRunnerGroupEstimator(100));

    // When
    let error = runner
        .continue_session(&mut session)
        .await
        .expect_err("undecodable durable row must terminate the loop");

    // Then
    assert!(matches!(error, AgentError::Compaction { .. }));
    assert!(model.requests().is_empty());
    assert!(!session.is_durable());
    assert!(session.take_persistence_error().is_some());
}

#[tokio::test]
async fn soft_tampered_durable_artifact_fails_closed_without_model_request() {
    assert_tampered_durable_artifact_fails_closed(TamperedArtifactFact::Payload).await;
}

#[tokio::test]
async fn soft_mistyped_durable_artifact_fails_closed_without_model_request() {
    assert_tampered_durable_artifact_fails_closed(TamperedArtifactFact::Bytes).await;
}

#[tokio::test]
async fn soft_malformed_artifact_journal_fails_closed_without_model_request() {
    assert_tampered_durable_artifact_fails_closed(TamperedArtifactFact::Journal).await;
}

#[tokio::test]
async fn soft_ambiguous_artifact_journal_fails_closed_without_model_request() {
    assert_tampered_durable_artifact_fails_closed(TamperedArtifactFact::DuplicateJournalKey).await;
}

#[tokio::test]
async fn soft_non_positive_artifact_journal_seq_fails_closed_without_model_request() {
    assert_tampered_durable_artifact_fails_closed(TamperedArtifactFact::JournalSeq).await;
}

#[derive(Clone, Copy)]
enum TamperedArtifactFact {
    Payload,
    Bytes,
    Journal,
    DuplicateJournalKey,
    JournalSeq,
}

async fn assert_tampered_durable_artifact_fails_closed(tampered: TamperedArtifactFact) {
    // Given
    let root = TestDir::new();
    let database = root.path().join("sessions.db");
    let store = Arc::new(
        TursoSessionStore::open(&database)
            .await
            .expect("store should open"),
    );
    let session_id = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let mut session = durable_tool_history(store.as_ref(), &session_id).await;
    let payload = ToolOutput::success("L".repeat(LARGE_RESULT_BYTES)).to_model_content();
    let digest = <sha2::Sha256 as sha2::Digest>::digest(payload.as_bytes());
    let hash = format!("sha256-{digest:x}");
    let artifact = crate::session_store::NewToolArtifact::inline(
        &hash,
        crate::compaction::artifact::adaptive_preview(&payload, 4_096),
        &payload,
    )
    .expect("artifact should be valid");
    let artifact_id = artifact.artifact_id().to_string();
    let receipt = store
        .put_tool_artifact(
            &session_id,
            session.durable_seq().expect("session should be durable"),
            artifact,
        )
        .await
        .expect("artifact should persist");
    session.advance_durable_seq(receipt.journal_seq());
    if matches!(tampered, TamperedArtifactFact::JournalSeq) {
        let later = store
            .append(
                &session_id,
                crate::session_store::NewJournalEntry::raw(
                    crate::session_store::JournalKind::SessionNote,
                    serde_json::json!({"note": "later durable fact"}),
                ),
            )
            .await
            .expect("later fact should persist");
        session.advance_durable_seq(later);
    }
    {
        let connection = store.connection_for_test().await;
        match tampered {
            TamperedArtifactFact::Payload => {
                connection
                    .execute(
                        "UPDATE tool_artifacts SET payload_text = ?1 \
                         WHERE session_id = ?2 AND artifact_id = ?3",
                        (
                            "tampered durable payload",
                            session_id.as_str(),
                            artifact_id.as_str(),
                        ),
                    )
                    .await
                    .expect("fixture should tamper the durable row");
            }
            TamperedArtifactFact::Bytes => {
                connection
                    .execute(
                        "UPDATE tool_artifacts SET bytes = 'not-an-integer' \
                         WHERE session_id = ?1 AND artifact_id = ?2",
                        (session_id.as_str(), artifact_id.as_str()),
                    )
                    .await
                    .expect("fixture should corrupt the durable row type");
            }
            TamperedArtifactFact::Journal => {
                connection
                    .execute(
                        "UPDATE journal_entries SET payload_json = X'FF' \
                         WHERE session_id = ?1 AND kind = 'tool_artifact'",
                        [session_id.as_str()],
                    )
                    .await
                    .expect("fixture should corrupt the artifact journal fact");
            }
            TamperedArtifactFact::DuplicateJournalKey => {
                let mut rows = connection
                    .query(
                        "SELECT payload_json FROM journal_entries \
                         WHERE session_id = ?1 AND kind = 'tool_artifact'",
                        [session_id.as_str()],
                    )
                    .await
                    .expect("artifact journal query should succeed");
                let original = rows
                    .next()
                    .await
                    .expect("artifact journal row should decode")
                    .expect("artifact journal fact should exist")
                    .get::<String>(0)
                    .expect("artifact journal payload should be text");
                let closing = original
                    .rfind('}')
                    .expect("fixture journal payload should be an object");
                let ambiguous = format!(
                    "{},\"artifact_id\":\"tool-result-sha256-duplicate\"}}",
                    &original[..closing]
                );
                connection
                    .execute(
                        "UPDATE journal_entries SET payload_json = ?1 \
                         WHERE session_id = ?2 AND kind = 'tool_artifact'",
                        (ambiguous, session_id.as_str()),
                    )
                    .await
                    .expect("fixture should add a duplicate journal field");
            }
            TamperedArtifactFact::JournalSeq => {
                connection
                    .execute(
                        "UPDATE journal_entries SET seq = -1 \
                         WHERE session_id = ?1 AND kind = 'tool_artifact'",
                        [session_id.as_str()],
                    )
                    .await
                    .expect("fixture should corrupt the earlier artifact sequence");
            }
        }
    }
    let model = FakeModel::new([response(AssistantContent::text("must not be requested"))]);
    let mut runner =
        configured_runner(model.clone(), CompactionMode::Enabled).with_session_store(store);
    runner.token_estimator = Arc::new(ScriptedRequestEstimator);
    runner.group_estimator = Arc::new(FixedRunnerGroupEstimator(100));

    // When
    let error = runner
        .continue_session(&mut session)
        .await
        .expect_err("tampered durable artifact must terminate the loop");

    // Then
    assert!(matches!(error, AgentError::Compaction { .. }));
    assert!(model.requests().is_empty());
    assert!(!session.is_durable());
    assert!(session.take_persistence_error().is_some());
}

async fn assert_unknown_outcome_blocks(write: AmbiguousWrite) {
    // Given
    let root = TestDir::new();
    let inner = Arc::new(
        TursoSessionStore::open(root.path().join("sessions.db"))
            .await
            .expect("store should open"),
    );
    let session_id = inner
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let mut session = durable_tool_history(inner.as_ref(), &session_id).await;
    let model = FakeModel::default();
    let observer = Arc::new(CollectingObserver::default());
    let store = Arc::new(AmbiguousStore { inner, write });
    let mut runner = configured_runner(model.clone(), CompactionMode::Enabled)
        .with_session_store(store)
        .with_observer(observer.clone());
    runner.token_estimator = Arc::new(ScriptedRequestEstimator);
    runner.group_estimator = Arc::new(FixedRunnerGroupEstimator(100));

    // When
    let error = runner
        .continue_session(&mut session)
        .await
        .expect_err("ambiguous persistence must fail closed");

    // Then
    assert!(matches!(error, AgentError::Compaction { .. }));
    assert!(model.requests().is_empty());
    assert!(!session.is_durable());
    let events = observer.events();
    assert!(events.iter().any(|event| matches!(
        event.kind,
        EventKind::CompactionFailed {
            recoverable: false,
            ..
        }
    )));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.kind, EventKind::Warning { .. }))
            .count(),
        1
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, EventKind::ModelStart))
    );
    let serialized = serde_json::to_string(&events).expect("events should serialize");
    assert!(!serialized.contains("provider-secret-ambiguous-receipt"));
}

async fn durable_tool_history(store: &TursoSessionStore, session_id: &SessionId) -> AgentSession {
    let large = ToolOutput::success("L".repeat(LARGE_RESULT_BYTES)).to_model_content();
    let small = ToolOutput::success("small").to_model_content();
    let messages = [
        tool_exchange_message("old", &large),
        tool_exchange_message("recent", &small),
    ]
    .concat();
    let mut session = AgentSession::new();
    session
        .attach_session_id(session_id.clone())
        .expect("fresh session should attach");
    for message in messages {
        let seq = store
            .append(
                session_id,
                crate::session_store::NewJournalEntry::message(&message)
                    .expect("message should encode"),
            )
            .await
            .expect("message should persist");
        session.push_with_journal_seq(message, Some(seq));
    }
    session
}

fn tool_exchange_message(id: &str, output: &str) -> Vec<Message> {
    vec![
        Message::Assistant {
            id: None,
            content: NonEmptyVec::new(AssistantContent::tool_call(
                id,
                "scripted_result",
                serde_json::json!({}),
            )),
        },
        Message::tool_result(id, output),
    ]
}
