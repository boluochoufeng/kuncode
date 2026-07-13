//! Session journal and checkpoint domain types.

use std::path::PathBuf;

use kuncode_core::completion::Message;
use serde_json::Value;

use super::{SessionStoreError, dto, project_slug};

mod compaction;

pub use compaction::{
    CommittedCompaction, CompactionEvent, CompactionMetadata, CompactionPassKind, CompactionReason,
    NewCompactionCommit,
};

/// Stable durable-session identifier that isolates every journal and checkpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionId(String);

impl SessionId {
    /// Wraps a session identifier supplied by the store or caller.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the raw identifier used by the persistence protocol.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Monotonically increasing journal sequence within one session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Seq(i64);

impl Seq {
    /// Frontier used before the journal contains any durable facts.
    pub const ZERO: Self = Self(0);

    /// Constructs a sequence from SQLite's signed integer representation.
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    /// Returns the underlying integer required for SQLite binding.
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// Project identity required to create a durable session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewSession {
    /// Original root path of the session's project.
    pub project_root: PathBuf,
    /// Filename-safe identifier derived deterministically by [`project_slug`].
    pub project_slug: String,
}

impl NewSession {
    /// Derives a stable project identity from its root path.
    pub fn new(project_root: PathBuf) -> Self {
        let project_slug = project_slug(&project_root);
        Self {
            project_root,
            project_slug,
        }
    }
}

/// Wire-protocol category of a journal payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalKind {
    /// Conversation message produced by a user, assistant, or tool.
    Message,
    /// Audit fact for a lossy context compaction.
    Compaction,
    /// Reference to a committed active-context checkpoint.
    CheckpointRef,
    /// Session-level note excluded from message replay.
    SessionNote,
    /// Reference fact for a fully persisted tool result.
    ToolArtifact,
}

impl JournalKind {
    /// Returns the stable protocol value written to SQLite's `kind` column.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Compaction => "compaction",
            Self::CheckpointRef => "checkpoint_ref",
            Self::SessionNote => "session_note",
            Self::ToolArtifact => "tool_artifact",
        }
    }
}

/// Journal append request that has not yet been assigned a sequence.
#[derive(Clone, Debug)]
pub struct NewJournalEntry {
    /// Protocol category that determines how to interpret the payload.
    pub kind: JournalKind,
    /// Versioned JSON payload encoded for its category.
    pub payload_json: Value,
}

impl NewJournalEntry {
    /// Encodes a completion message as a versioned journal payload.
    ///
    /// # Errors
    /// Returns [`SessionStoreError::Json`] when the message cannot be encoded as
    /// durable JSON.
    pub fn message(message: &Message) -> Result<Self, SessionStoreError> {
        Ok(Self {
            kind: JournalKind::Message,
            payload_json: dto::message_to_value(message)?,
        })
    }

    /// Wraps a payload already encoded by the caller for its [`JournalKind`].
    pub fn raw(kind: JournalKind, payload_json: Value) -> Self {
        Self { kind, payload_json }
    }
}

/// Durable fact recovered through journal replay.
#[derive(Clone, Debug, PartialEq)]
pub struct JournalEntry {
    /// Fact's ordered position within its session.
    pub seq: Seq,
    /// Protocol category retained verbatim so unknown stored values are not lost.
    pub kind: String,
    /// Versioned payload not yet decoded according to its category.
    pub payload_json: Value,
}

impl JournalEntry {
    /// Decodes a message-category payload into a completion message.
    ///
    /// # Errors
    /// Returns a storage error when the payload shape, schema version, or message
    /// content is invalid.
    pub fn into_message(self) -> Result<Message, SessionStoreError> {
        dto::message_from_value(self.payload_json)
    }
}

/// Complete candidate for an active-context checkpoint write.
#[derive(Clone, Debug)]
pub struct NewCheckpoint {
    /// Session that owns the checkpoint.
    pub session_id: SessionId,
    /// Highest journal sequence absorbed into the active context.
    pub covers_through_seq: Seq,
    /// First sequence covered by the lossy summary, absent without a summary.
    pub source_seq_start: Option<Seq>,
    /// Last sequence covered by the lossy summary, absent without a summary.
    pub source_seq_end: Option<Seq>,
    /// Derived message view ready for the next model request.
    pub active_messages: Vec<Message>,
    /// Structured summary and schema version, absent without semantic summarization.
    pub summary_json: Option<Value>,
    /// Model that generated the active summary, absent without summary provenance.
    pub model: Option<String>,
    /// Provider usage for the active summary, absent without summary provenance.
    pub token_usage_json: Option<Value>,
}

/// Committed checkpoint used to rebuild the active context.
#[derive(Clone, Debug, PartialEq)]
pub struct Checkpoint {
    /// Sequence of the corresponding `checkpoint_ref` journal fact.
    pub checkpoint_seq: Seq,
    /// Highest absorbed sequence that replay must skip.
    pub covers_through_seq: Seq,
    /// First sequence covered by the lossy summary, absent without a summary.
    pub source_seq_start: Option<Seq>,
    /// Last sequence covered by the lossy summary, absent without a summary.
    pub source_seq_end: Option<Seq>,
    /// Active-context message view at checkpoint commit time.
    pub active_messages: Vec<Message>,
    /// Structured summary and its schema version.
    pub summary_json: Option<Value>,
    /// Model that generated the semantic summary.
    pub model: Option<String>,
    /// Provider token usage for summarization.
    pub token_usage_json: Option<Value>,
}
