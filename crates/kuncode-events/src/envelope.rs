//! The typed envelope that wraps every persisted event.
//!
//! Each line in `events.jsonl` is a serialized `EventEnvelope`. The `kind`
//! field selects the closed `EventKind` enum; `payload` carries the
//! kind-specific JSON.

use kuncode_core::{AgentId, EventId, RunId, TurnId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

/// Wire schema version. Bumped only when a breaking change is made to the
/// envelope structure itself.
pub const EVENT_SCHEMA_VERSION: u16 = 1;

/// A single persisted event in the event log.
///
/// Every event carries enough metadata to reconstruct the full run timeline
/// without consulting any other source.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema_version: u16,
    pub event_id: EventId,
    pub run_id: RunId,
    pub agent_id: Option<AgentId>,
    pub turn_id: Option<TurnId>,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub kind: EventKind,
    pub payload: Value,
}

impl EventEnvelope {
    /// Create a new envelope with an auto-generated `EventId` and current UTC
    /// timestamp. `agent_id` and `turn_id` default to `None`.
    pub fn new(run_id: RunId, kind: EventKind, payload: Value) -> Self {
        Self {
            schema_version: EVENT_SCHEMA_VERSION,
            event_id: EventId::new(),
            run_id,
            agent_id: None,
            turn_id: None,
            timestamp: OffsetDateTime::now_utc(),
            kind,
            payload,
        }
    }

    #[must_use]
    pub fn with_agent(mut self, agent_id: AgentId) -> Self {
        self.agent_id = Some(agent_id);
        self
    }

    #[must_use]
    pub fn with_turn(mut self, turn_id: TurnId) -> Self {
        self.turn_id = Some(turn_id);
        self
    }
}

/// Closed set of event kinds for the MVP.
///
/// Stored as a tagged JSON string (`"kind": "run.started"`) so that unknown
/// kinds can be detected by `EventLogReader` without breaking deserialization
/// of the surrounding envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum EventKind {
    #[serde(rename = "run.started")]
    RunStarted,
    #[serde(rename = "run.completed")]
    RunCompleted,
    #[serde(rename = "run.failed")]
    RunFailed,
    #[serde(rename = "artifact.created")]
    ArtifactCreated,
}

impl EventKind {
    /// Returns the wire string for this kind (e.g. `"run.started"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RunStarted => "run.started",
            Self::RunCompleted => "run.completed",
            Self::RunFailed => "run.failed",
            Self::ArtifactCreated => "artifact.created",
        }
    }

    /// Parse a wire string into an `EventKind`. Returns `None` for unknown
    /// values so the reader can emit `EventLogError::UnknownKind`.
    pub fn from_wire(kind: &str) -> Option<Self> {
        match kind {
            "run.started" => Some(Self::RunStarted),
            "run.completed" => Some(Self::RunCompleted),
            "run.failed" => Some(Self::RunFailed),
            "artifact.created" => Some(Self::ArtifactCreated),
            _ => None,
        }
    }
}
