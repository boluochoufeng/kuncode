use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use kuncode_core::completion::{CompletionError, Message, Usage, UserContent};
use serde::Deserialize;

use crate::{
    compaction::summary::{
        ContextSummarizer, ContinuitySummary, GeneratedSummary, SummarizerError, SummaryRequest,
        WorkspaceSummary, build_summary_prompt,
    },
    session_store::{
        JournalKind, NewJournalEntry, Seq, SessionId, SessionStore, sqlite::SqliteSessionStore,
    },
};

pub(crate) enum SummaryBehavior {
    Valid,
    Malformed,
    ProviderFailure,
    AppendDuringCall {
        store: Arc<SqliteSessionStore>,
        session_id: SessionId,
    },
}

pub(crate) struct TestSummarizer {
    calls: AtomicUsize,
    behavior: SummaryBehavior,
    observations: Mutex<Vec<SummaryObservation>>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct SummaryObservation {
    pub(crate) existing_summary: Option<ContinuitySummary>,
    pub(crate) source_seq_start: i64,
    pub(crate) source_seq_end: i64,
    pub(crate) allowed_artifact_refs: BTreeSet<String>,
    pub(crate) source_messages: Vec<Message>,
}

impl TestSummarizer {
    pub(crate) const fn new(behavior: SummaryBehavior) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            behavior,
            observations: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub(crate) fn observations(&self) -> Vec<SummaryObservation> {
        self.observations
            .lock()
            .expect("summary observations should not be poisoned")
            .clone()
    }

    async fn valid(&self, request: SummaryRequest) -> Result<GeneratedSummary, SummarizerError> {
        let prompt = build_summary_prompt(&request).map_err(SummarizerError::InvalidRequest)?;
        let Message::User { content } = &prompt[1] else {
            panic!("summary prompt should end with user data");
        };
        let UserContent::Text(text) = content.first() else {
            panic!("summary payload should be text");
        };
        let observation: SummaryObservation =
            serde_json::from_str(text.text_ref()).expect("summary payload should be JSON");
        self.observations
            .lock()
            .expect("summary observations should not be poisoned")
            .push(observation.clone());
        let summary = ContinuitySummary {
            version: 1,
            source_seq_start: Seq::new(observation.source_seq_start),
            source_seq_end: Seq::new(observation.source_seq_end),
            current_goal: "continue the coding task".to_string(),
            constraints: vec![],
            decisions: vec![],
            completed_work: vec![],
            workspace: WorkspaceSummary {
                working_directory: "/workspace".to_string(),
                files: vec![],
                symbols: vec![],
            },
            commands_and_tests: vec![],
            unresolved_errors: vec![],
            todos: vec![],
            next_actions: vec!["continue".to_string()],
            artifact_refs: observation.allowed_artifact_refs.into_iter().collect(),
        };
        let raw = serde_json::to_string(&summary).expect("summary should encode");
        let summary = request
            .parse_and_validate(&raw)
            .expect("summary should match its bound request");
        Ok(GeneratedSummary {
            summary,
            usage: Usage {
                input_tokens: 30,
                output_tokens: 10,
                total_tokens: 40,
                ..Usage::default()
            },
        })
    }
}

#[async_trait]
impl ContextSummarizer for TestSummarizer {
    async fn summarize(
        &self,
        request: SummaryRequest,
    ) -> Result<GeneratedSummary, SummarizerError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.behavior {
            SummaryBehavior::Valid => self.valid(request).await,
            SummaryBehavior::Malformed => {
                let source = request
                    .parse_and_validate("not-json")
                    .expect_err("malformed summary should be rejected");
                Err(SummarizerError::InvalidSummary {
                    source,
                    usage: Usage::default(),
                })
            }
            SummaryBehavior::ProviderFailure => Err(SummarizerError::Completion(
                CompletionError::ResponseError("provider unavailable".to_string()),
            )),
            SummaryBehavior::AppendDuringCall { store, session_id } => {
                store
                    .append(
                        session_id,
                        NewJournalEntry::raw(
                            JournalKind::SessionNote,
                            serde_json::json!({"note": "concurrent append"}),
                        ),
                    )
                    .await
                    .expect("concurrent journal append should commit");
                self.valid(request).await
            }
        }
    }
}
