//! The typed envelope that wraps every persisted event.
//!
//! Each line in `events.jsonl` is a serialized `EventEnvelope`. The `kind`
//! field selects the closed `EventKind` enum; `payload` carries the
//! kind-specific JSON.

use kuncode_core::{AgentId, ArtifactId, EventId, RiskFlag, RunId, ToolEffect, ToolErrorKind, ToolRequestId, TurnId};
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
    #[serde(rename = "tool.started")]
    ToolStarted,
    #[serde(rename = "tool.completed")]
    ToolCompleted,
    #[serde(rename = "tool.failed")]
    ToolFailed,
    #[serde(rename = "tool.cancelled")]
    ToolCancelled,
}

impl EventKind {
    /// Returns the wire string for this kind (e.g. `"run.started"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RunStarted => "run.started",
            Self::RunCompleted => "run.completed",
            Self::RunFailed => "run.failed",
            Self::ArtifactCreated => "artifact.created",
            Self::ToolStarted => "tool.started",
            Self::ToolCompleted => "tool.completed",
            Self::ToolFailed => "tool.failed",
            Self::ToolCancelled => "tool.cancelled",
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
            "tool.started" => Some(Self::ToolStarted),
            "tool.completed" => Some(Self::ToolCompleted),
            "tool.failed" => Some(Self::ToolFailed),
            "tool.cancelled" => Some(Self::ToolCancelled),
            _ => None,
        }
    }
}

/// Payload of `tool.started`. See Phase 2 plan §6.1.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolStarted {
    pub tool_request_id: ToolRequestId,
    pub tool_name: String,
    pub effects: Vec<ToolEffect>,
    pub risk_flags: Vec<RiskFlag>,
}

/// Payload of `tool.completed`. See Phase 2 plan §6.1.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolCompleted {
    pub tool_request_id: ToolRequestId,
    pub tool_name: String,
    pub summary: String,
    pub content_ref: Option<ArtifactId>,
}

/// Payload of `tool.failed`. See Phase 2 plan §6.1.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolFailed {
    pub tool_request_id: ToolRequestId,
    pub tool_name: String,
    pub error_kind: ToolErrorKind,
    pub summary: String,
}

/// Payload of `tool.cancelled`. See Phase 2 plan §6.1.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolCancelled {
    pub tool_request_id: ToolRequestId,
    pub tool_name: String,
    pub summary: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuncode_core::RiskFlag;
    use serde_json::json;

    #[test]
    fn event_kind_as_str_covers_every_variant() {
        let cases = [
            (EventKind::RunStarted, "run.started"),
            (EventKind::RunCompleted, "run.completed"),
            (EventKind::RunFailed, "run.failed"),
            (EventKind::ArtifactCreated, "artifact.created"),
            (EventKind::ToolStarted, "tool.started"),
            (EventKind::ToolCompleted, "tool.completed"),
            (EventKind::ToolFailed, "tool.failed"),
            (EventKind::ToolCancelled, "tool.cancelled"),
        ];
        for (kind, wire) in cases {
            assert_eq!(kind.as_str(), wire);
        }
    }

    #[test]
    fn event_kind_from_wire_roundtrips_every_variant() {
        let cases = [
            EventKind::RunStarted,
            EventKind::RunCompleted,
            EventKind::RunFailed,
            EventKind::ArtifactCreated,
            EventKind::ToolStarted,
            EventKind::ToolCompleted,
            EventKind::ToolFailed,
            EventKind::ToolCancelled,
        ];
        for kind in cases {
            assert_eq!(EventKind::from_wire(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn event_kind_from_wire_rejects_unknown() {
        assert_eq!(EventKind::from_wire("tool.queued"), None);
        assert_eq!(EventKind::from_wire(""), None);
        // PascalCase is not the wire format.
        assert_eq!(EventKind::from_wire("ToolStarted"), None);
    }

    #[test]
    fn event_kind_serde_uses_wire_string() {
        let value = serde_json::to_value(EventKind::ToolStarted).expect("serialize");
        assert_eq!(value, json!("tool.started"));
        let parsed: EventKind = serde_json::from_value(json!("tool.cancelled")).expect("parse");
        assert_eq!(parsed, EventKind::ToolCancelled);
    }

    #[test]
    fn tool_started_payload_matches_wire_format() {
        let request_id = ToolRequestId::new();
        let payload = ToolStarted {
            tool_request_id: request_id,
            tool_name: "read_file".to_owned(),
            effects: vec![ToolEffect::ReadWorkspace],
            risk_flags: vec![RiskFlag::MutatesWorkspace],
        };

        let value = serde_json::to_value(&payload).expect("serialize");
        assert_eq!(value["tool_name"], "read_file");
        assert_eq!(value["effects"], json!(["read_workspace"]));
        assert_eq!(value["risk_flags"], json!(["mutates_workspace"]));
        let parsed: ToolStarted = serde_json::from_value(value).expect("parse");
        assert_eq!(parsed, payload);
    }

    #[test]
    fn tool_completed_payload_serializes_null_content_ref() {
        let payload = ToolCompleted {
            tool_request_id: ToolRequestId::new(),
            tool_name: "read_file".to_owned(),
            summary: "read src/lib.rs (1024 bytes)".to_owned(),
            content_ref: None,
        };
        let value = serde_json::to_value(&payload).expect("serialize");
        assert!(value["content_ref"].is_null());
        let parsed: ToolCompleted = serde_json::from_value(value).expect("parse");
        assert_eq!(parsed, payload);
    }

    #[test]
    fn tool_completed_payload_carries_artifact_id() {
        let artifact = ArtifactId::new();
        let payload = ToolCompleted {
            tool_request_id: ToolRequestId::new(),
            tool_name: "git_diff".to_owned(),
            summary: "diff src/lib.rs (640 bytes, artifact)".to_owned(),
            content_ref: Some(artifact),
        };
        let value = serde_json::to_value(&payload).expect("serialize");
        assert_eq!(value["content_ref"], json!(artifact.to_string()));
        let parsed: ToolCompleted = serde_json::from_value(value).expect("parse");
        assert_eq!(parsed.content_ref, Some(artifact));
    }

    #[test]
    fn tool_failed_payload_round_trips() {
        let payload = ToolFailed {
            tool_request_id: ToolRequestId::new(),
            tool_name: "read_file".to_owned(),
            error_kind: ToolErrorKind::Workspace,
            summary: "path escapes workspace root".to_owned(),
        };
        let value = serde_json::to_value(&payload).expect("serialize");
        assert_eq!(value["error_kind"], "workspace");
        let parsed: ToolFailed = serde_json::from_value(value).expect("parse");
        assert_eq!(parsed, payload);
    }

    #[test]
    fn tool_failed_payload_serializes_compound_error_kind() {
        let payload = ToolFailed {
            tool_request_id: ToolRequestId::new(),
            tool_name: "exec_argv".to_owned(),
            error_kind: ToolErrorKind::ResultTooLarge,
            summary: "stdout exceeded inline cap".to_owned(),
        };
        let value = serde_json::to_value(&payload).expect("serialize");
        assert_eq!(value["error_kind"], "result_too_large");
        let parsed: ToolFailed = serde_json::from_value(value).expect("parse");
        assert_eq!(parsed, payload);
    }

    #[test]
    fn tool_cancelled_payload_round_trips() {
        let payload = ToolCancelled {
            tool_request_id: ToolRequestId::new(),
            tool_name: "exec_argv".to_owned(),
            summary: "cancelled".to_owned(),
        };
        let value = serde_json::to_value(&payload).expect("serialize");
        assert_eq!(value["summary"], "cancelled");
        let parsed: ToolCancelled = serde_json::from_value(value).expect("parse");
        assert_eq!(parsed, payload);
    }
}
