#[derive(Clone, Copy)]
enum AmbiguousWrite {
    Artifact,
    ArtifactConflict,
    Compaction,
}

struct AmbiguousStore {
    inner: Arc<SqliteSessionStore>,
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
            AmbiguousWrite::ArtifactConflict => {
                Err(crate::session_store::SessionStoreError::JournalHeadConflict {
                    expected: expected_journal_head.get(),
                    actual: expected_journal_head.get() + 1,
                })
            }
            AmbiguousWrite::Compaction => {
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
            AmbiguousWrite::Artifact | AmbiguousWrite::ArtifactConflict => {
                self.inner.commit_compaction(commit).await
            }
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
}

fn ambiguous<T>(operation: &'static str) -> Result<T, crate::session_store::SessionStoreError> {
    Err(crate::session_store::SessionStoreError::CommitOutcomeUnknown {
        operation,
        message: "provider-secret-ambiguous-receipt".to_string(),
    })
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
        SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
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
    let mut runner = configured_runner(model.clone(), CompactionMode::Enabled)
        .with_session_store(store);
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

async fn assert_unknown_outcome_blocks(write: AmbiguousWrite) {
    // Given
    let root = TestDir::new();
    let inner = Arc::new(
        SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
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
    let store = Arc::new(AmbiguousStore {
        inner,
        write,
    });
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
    assert!(!events.iter().any(|event| matches!(event.kind, EventKind::ModelStart)));
    let serialized = serde_json::to_string(&events).expect("events should serialize");
    assert!(!serialized.contains("provider-secret-ambiguous-receipt"));
}

async fn durable_tool_history(
    store: &SqliteSessionStore,
    session_id: &SessionId,
) -> AgentSession {
    let large = ToolOutput::success("L".repeat(LARGE_RESULT_BYTES)).to_model_content();
    let small = ToolOutput::success("small").to_model_content();
    let messages = [tool_exchange_message("old", &large), tool_exchange_message("recent", &small)]
        .concat();
    let mut session = AgentSession::new();
    session.attach_session_id(session_id.clone());
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
