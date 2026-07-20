use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use kuncode_core::{
    completion::{AssistantContent, Message, ToolResult, ToolResultContent, UserContent},
    non_empty_vec::NonEmptyVec,
};

use crate::{
    compaction::artifact::{ArtifactStore, ArtifactTokenCounter, ArtifactTokenCounterError},
    session::AgentSession,
    session_store::{
        CommittedArtifact, JournalEntry, JournalSnapshot, NewJournalEntry, NewToolArtifact, Seq,
        SessionId, SessionStore, SessionStoreError, turso::TursoSessionStore,
    },
    tool::ToolOutput,
};

pub(super) struct FixedCounter {
    original: Result<u64, String>,
    marker: u64,
}

pub(super) struct SerializedByteCounter;

pub(super) struct AdaptiveMarkerCounter {
    divisor: u64,
}

#[derive(Default)]
enum PutFailure {
    #[default]
    Rejected,
    OutcomeUnknown,
}

impl AdaptiveMarkerCounter {
    pub(super) const fn new(divisor: u64) -> Self {
        Self { divisor }
    }
}

#[async_trait]
impl ArtifactTokenCounter for AdaptiveMarkerCounter {
    async fn count(&self, result: &ToolResult) -> Result<u64, ArtifactTokenCounterError> {
        let ToolResultContent::Text(text) = result.content.first();
        if !text.text_ref().contains("\"artifact_id\"") {
            return Ok(9_000);
        }
        let bytes = serde_json::to_vec(result)
            .map_err(|error| ArtifactTokenCounterError::provider(error.to_string()))?;
        let count = u64::try_from(bytes.len())
            .map_err(|_| ArtifactTokenCounterError::provider("serialized result is too large"))?;
        Ok(count / self.divisor)
    }
}

#[async_trait]
impl ArtifactTokenCounter for SerializedByteCounter {
    async fn count(&self, result: &ToolResult) -> Result<u64, ArtifactTokenCounterError> {
        let bytes = serde_json::to_vec(result)
            .map_err(|error| ArtifactTokenCounterError::provider(error.to_string()))?;
        u64::try_from(bytes.len())
            .map_err(|_| ArtifactTokenCounterError::provider("serialized result is too large"))
    }
}

#[derive(Default)]
pub(super) struct RejectingStore {
    calls: AtomicUsize,
    replay_calls: AtomicUsize,
    entries: Vec<JournalEntry>,
    put_failure: PutFailure,
}

impl RejectingStore {
    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub(super) fn replay_calls(&self) -> usize {
        self.replay_calls.load(Ordering::SeqCst)
    }

    pub(super) fn with_messages(messages: &[Message]) -> (Self, AgentSession) {
        Self::with_failure(messages, PutFailure::Rejected)
    }

    pub(super) fn with_unknown_commit(messages: &[Message]) -> (Self, AgentSession) {
        Self::with_failure(messages, PutFailure::OutcomeUnknown)
    }

    fn with_failure(messages: &[Message], put_failure: PutFailure) -> (Self, AgentSession) {
        let entries = messages
            .iter()
            .enumerate()
            .map(|(index, message)| {
                let entry = NewJournalEntry::message(message).expect("message should encode");
                JournalEntry {
                    seq: Seq::new(i64::try_from(index + 1).expect("fixture should fit i64")),
                    kind: entry.kind.as_str().to_string(),
                    payload_json: entry.payload_json,
                }
            })
            .collect::<Vec<_>>();
        let mut session = AgentSession::new();
        session
            .attach_session_id(SessionId::new("test-session"))
            .expect("fresh session should attach");
        for (message, entry) in messages.iter().zip(&entries) {
            session.push_with_journal_seq(message.clone(), Some(entry.seq));
        }
        (
            Self {
                calls: AtomicUsize::new(0),
                replay_calls: AtomicUsize::new(0),
                entries,
                put_failure,
            },
            session,
        )
    }
}

#[async_trait]
impl ArtifactStore for RejectingStore {
    async fn replay(
        &self,
        _session: &SessionId,
        seq: Seq,
    ) -> Result<Vec<JournalEntry>, SessionStoreError> {
        Ok(self
            .entries
            .iter()
            .filter(|entry| entry.seq > seq)
            .cloned()
            .collect())
    }

    async fn journal_snapshot(
        &self,
        _session: &SessionId,
        seqs: &[Seq],
    ) -> Result<JournalSnapshot, SessionStoreError> {
        self.replay_calls.fetch_add(1, Ordering::SeqCst);
        let requested = seqs
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let head = self.entries.last().map_or(Seq::ZERO, |entry| entry.seq);
        let entries = self
            .entries
            .iter()
            .filter(|entry| requested.contains(&entry.seq))
            .cloned()
            .collect();
        Ok(JournalSnapshot::new(head, entries))
    }

    async fn put(
        &self,
        _session: &SessionId,
        _expected_journal_head: Seq,
        _artifact: NewToolArtifact,
    ) -> Result<CommittedArtifact, SessionStoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.put_failure {
            PutFailure::Rejected => Err(SessionStoreError::InvalidToolArtifact(
                "injected store failure".to_string(),
            )),
            PutFailure::OutcomeUnknown => Err(SessionStoreError::CommitOutcomeUnknown {
                operation: "put test artifact",
                message: "injected uncertain commit".to_string(),
            }),
        }
    }
}

pub(super) async fn persisted_session(
    store: &TursoSessionStore,
    session_id: SessionId,
    messages: &[Message],
) -> AgentSession {
    let mut session = AgentSession::new();
    session
        .attach_session_id(session_id.clone())
        .expect("fresh session should attach");
    for message in messages {
        let seq = store
            .append(
                &session_id,
                NewJournalEntry::message(message).expect("message should encode"),
            )
            .await
            .expect("message should commit");
        session.push_with_journal_seq(message.clone(), Some(seq));
    }
    session
}

impl FixedCounter {
    pub(super) fn new(original: u64, marker: u64) -> Self {
        Self {
            original: Ok(original),
            marker,
        }
    }

    pub(super) fn failing(message: &str) -> Self {
        Self {
            original: Err(message.to_string()),
            marker: 0,
        }
    }
}

#[async_trait]
impl ArtifactTokenCounter for FixedCounter {
    async fn count(&self, result: &ToolResult) -> Result<u64, ArtifactTokenCounterError> {
        let ToolResultContent::Text(text) = result.content.first();
        if text.text_ref().contains("\"artifact_id\"") {
            Ok(self.marker)
        } else {
            self.original
                .clone()
                .map_err(ArtifactTokenCounterError::provider)
        }
    }
}

pub(super) fn tool_exchange(id: &str, name: &str, body: &str) -> Vec<Message> {
    let output = ToolOutput::success(serde_json::json!({ "body": body })).to_model_content();
    tool_exchange_with_text(id, name, &output)
}

pub(super) fn tool_exchange_with_text(id: &str, name: &str, text: &str) -> Vec<Message> {
    vec![
        Message::Assistant {
            id: None,
            content: NonEmptyVec::new(AssistantContent::tool_call(id, name, serde_json::json!({}))),
        },
        Message::User {
            content: NonEmptyVec::new(UserContent::ToolResult(ToolResult {
                id: id.to_string(),
                call_id: None,
                content: NonEmptyVec::new(ToolResultContent::text(text)),
            })),
        },
    ]
}
