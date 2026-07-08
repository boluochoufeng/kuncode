//! Durable session history storage.
//!
//! The store owns the complete journal and active-context checkpoints. An
//! [`AgentSession`](crate::session::AgentSession) remains the in-memory active
//! context.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use kuncode_core::completion::Message;
use serde_json::Value;

mod dto;
pub mod sqlite;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Seq(i64);

impl Seq {
    pub const ZERO: Self = Self(0);

    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewSession {
    pub project_root: PathBuf,
    pub project_slug: String,
}

impl NewSession {
    pub fn new(project_root: PathBuf) -> Self {
        let project_slug = project_slug(&project_root);
        Self {
            project_root,
            project_slug,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalKind {
    Message,
    Compaction,
    CheckpointRef,
    SessionNote,
    ToolArtifact,
}

impl JournalKind {
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

#[derive(Clone, Debug)]
pub struct NewJournalEntry {
    pub kind: JournalKind,
    pub payload_json: Value,
}

impl NewJournalEntry {
    pub fn message(message: &Message) -> Result<Self, SessionStoreError> {
        Ok(Self {
            kind: JournalKind::Message,
            payload_json: dto::message_to_value(message)?,
        })
    }

    pub fn raw(kind: JournalKind, payload_json: Value) -> Self {
        Self { kind, payload_json }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct JournalEntry {
    pub seq: Seq,
    pub kind: String,
    pub payload_json: Value,
}

impl JournalEntry {
    pub fn into_message(self) -> Result<Message, SessionStoreError> {
        dto::message_from_value(self.payload_json)
    }
}

#[derive(Clone, Debug)]
pub struct NewCheckpoint {
    pub session_id: SessionId,
    pub covers_through_seq: Seq,
    pub source_seq_start: Option<Seq>,
    pub source_seq_end: Option<Seq>,
    pub active_messages: Vec<Message>,
    pub summary_json: Option<Value>,
    pub model: Option<String>,
    pub token_usage_json: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Checkpoint {
    pub checkpoint_seq: Seq,
    pub covers_through_seq: Seq,
    pub source_seq_start: Option<Seq>,
    pub source_seq_end: Option<Seq>,
    pub active_messages: Vec<Message>,
    pub summary_json: Option<Value>,
    pub model: Option<String>,
    pub token_usage_json: Option<Value>,
}

/// Tool-result payload captured by the harness before active-context trimming.
///
/// The artifact is not exposed as a workspace file. Future compaction code uses
/// the returned [`ToolArtifactRef`] as a short marker in the active context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewToolArtifact {
    artifact_id: String,
    content_hash: String,
    bytes: i64,
    preview: String,
    payload_text: Option<String>,
    storage_ref: Option<String>,
}

impl NewToolArtifact {
    /// Builds an inline artifact from a tool-result payload already visible to
    /// the model.
    ///
    /// # Errors
    /// Returns [`SessionStoreError::InvalidToolArtifact`] when `content_hash` is
    /// empty, or [`SessionStoreError::ToolArtifactTooLarge`] if the payload
    /// length cannot fit SQLite's signed integer range.
    pub fn inline(
        content_hash: impl Into<String>,
        preview: impl Into<String>,
        payload: impl Into<String>,
    ) -> Result<Self, SessionStoreError> {
        let content_hash = non_empty_artifact_field("content_hash", content_hash.into())?;
        let payload = payload.into();
        let bytes =
            i64::try_from(payload.len()).map_err(|_| SessionStoreError::ToolArtifactTooLarge {
                bytes: payload.len(),
            })?;
        Ok(Self {
            artifact_id: format!("tool-result-{content_hash}"),
            content_hash,
            bytes,
            preview: preview.into(),
            payload_text: Some(payload),
            storage_ref: None,
        })
    }

    pub fn artifact_id(&self) -> &str {
        &self.artifact_id
    }

    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    pub fn bytes(&self) -> i64 {
        self.bytes
    }

    pub fn preview(&self) -> &str {
        &self.preview
    }

    pub fn payload_text(&self) -> Option<&str> {
        self.payload_text.as_deref()
    }

    pub fn storage_ref(&self) -> Option<&str> {
        self.storage_ref.as_deref()
    }
}

/// Stable reference returned after a tool artifact is durably recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolArtifactRef {
    pub(crate) artifact_id: String,
    pub(crate) content_hash: String,
    pub(crate) bytes: i64,
    pub(crate) preview: String,
}

impl ToolArtifactRef {
    pub fn artifact_id(&self) -> &str {
        &self.artifact_id
    }

    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    pub fn bytes(&self) -> i64 {
        self.bytes
    }

    pub fn preview(&self) -> &str {
        &self.preview
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SessionStoreError {
    #[error("session store I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("session store database failed: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("session store JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid stored message payload: {0}")]
    InvalidMessagePayload(String),
    #[error("unsupported {payload} payload schema version: {version}")]
    UnsupportedPayloadVersion { payload: &'static str, version: u64 },
    #[error("invalid checkpoint: {0}")]
    InvalidCheckpoint(String),
    #[error("invalid tool artifact: {0}")]
    InvalidToolArtifact(String),
    #[error("tool artifact is too large to store: {bytes} bytes")]
    ToolArtifactTooLarge { bytes: usize },
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create_session(&self, session: NewSession) -> Result<SessionId, SessionStoreError>;

    async fn append(
        &self,
        session: &SessionId,
        entry: NewJournalEntry,
    ) -> Result<Seq, SessionStoreError>;

    async fn put_tool_artifact(
        &self,
        session: &SessionId,
        artifact: NewToolArtifact,
    ) -> Result<ToolArtifactRef, SessionStoreError>;

    async fn latest_checkpoint(
        &self,
        session: &SessionId,
    ) -> Result<Option<Checkpoint>, SessionStoreError>;

    async fn write_checkpoint(&self, checkpoint: NewCheckpoint) -> Result<Seq, SessionStoreError>;

    async fn replay_after(
        &self,
        session: &SessionId,
        seq: Seq,
    ) -> Result<Vec<JournalEntry>, SessionStoreError>;
}

pub fn session_store_path(home: &Path) -> PathBuf {
    home.join(".kuncode")
        .join("sessions")
        .join("session-store.sqlite3")
}

pub fn project_slug(root: &Path) -> String {
    root.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn non_empty_artifact_field(
    field: &'static str,
    value: String,
) -> Result<String, SessionStoreError> {
    if value.trim().is_empty() {
        Err(SessionStoreError::InvalidToolArtifact(format!(
            "`{field}` must not be empty"
        )))
    } else {
        Ok(value)
    }
}
