//! Closed, versioned wire schema for lossy semantic continuity.
//!
//! These types are decoded from model-controlled JSON. Every object rejects
//! unknown fields so schema drift and injected side channels fail closed instead
//! of being silently ignored.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::session_store::Seq;

/// Wire version accepted by the first continuity-summary validator.
pub const CONTINUITY_SUMMARY_VERSION: u32 = 1;

/// Lossy semantic continuity extracted from one durable journal range.
///
/// The range is provenance, not a claim that the summary preserves every source
/// detail. Durable journal facts remain authoritative when the projection differs.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContinuitySummary {
    /// Version controlling decoding and validation semantics.
    ///
    /// The wire name follows other durable payloads in this crate.
    #[serde(rename = "schema_version")]
    #[schemars(range(min = 1, max = 1))]
    pub version: u32,
    /// First durable journal fact represented by this summary.
    ///
    /// Encoded as the journal's signed integer coordinate on the wire so summary
    /// provenance can be compared directly with durable [`Seq`] values.
    #[serde(with = "seq_serde")]
    #[schemars(with = "i64")]
    #[schemars(range(min = 1))]
    pub source_seq_start: Seq,
    /// Last durable journal fact represented by this summary.
    ///
    /// Uses the same durable wire coordinate as [`Self::source_seq_start`].
    #[serde(with = "seq_serde")]
    #[schemars(with = "i64")]
    #[schemars(range(min = 1))]
    pub source_seq_end: Seq,
    /// Best current semantic goal, without authority over runtime state.
    pub current_goal: String,
    /// User constraints inferred from history and therefore potentially lossy.
    pub constraints: Vec<String>,
    /// Confirmed, rejected, deferred, or superseded decisions with reasons.
    pub decisions: Vec<String>,
    /// Work believed complete; durable state remains the authority.
    pub completed_work: Vec<String>,
    /// Workspace paths and symbols needed to continue efficiently.
    pub workspace: WorkspaceSummary,
    /// Commands and tests with their observed outcomes.
    pub commands_and_tests: Vec<CommandSummary>,
    /// Errors that still need investigation or resolution.
    pub unresolved_errors: Vec<String>,
    /// Lossy task projection that cannot replace the harness todo state.
    pub todos: Vec<SummaryTodo>,
    /// Concrete actions most likely to advance the current goal.
    pub next_actions: Vec<String>,
    /// Artifact identifiers selected only from the supplied source set.
    pub artifact_refs: Vec<String>,
}

/// Workspace facts useful for re-establishing coding context.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSummary {
    /// Working directory observed in the summarized context, or `unknown`.
    pub working_directory: String,
    /// Relevant file and directory paths.
    pub files: Vec<String>,
    /// Relevant types, functions, modules, and other symbols.
    pub symbols: Vec<String>,
}

/// One command or test and its observed result.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandSummary {
    /// Exact command when known, otherwise a concise test description.
    pub command: String,
    /// Observed outcome without inventing execution evidence.
    pub outcome: String,
    /// Process exit code when the source context contains one.
    pub exit_code: Option<i32>,
}

/// One non-authoritative task retained for semantic continuity.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SummaryTodo {
    /// Concise action or acceptance condition.
    pub content: String,
    /// Status inferred from summarized history.
    pub status: SummaryTodoStatus,
}

/// Status vocabulary isolated from the authoritative runtime todo list.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SummaryTodoStatus {
    /// Work has not started.
    Pending,
    /// Work is currently active.
    InProgress,
    /// Work is believed complete from the summarized evidence.
    Completed,
}

mod seq_serde {
    //! Preserves the journal sequence wire representation during serialization.

    use serde::{Deserialize, Deserializer, Serializer};

    use crate::session_store::Seq;

    pub(super) fn serialize<S>(value: &Seq, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i64(value.get())
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Seq, D::Error>
    where
        D: Deserializer<'de>,
    {
        i64::deserialize(deserializer).map(Seq::new)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::fixture_summary;
    use super::{CONTINUITY_SUMMARY_VERSION, ContinuitySummary};

    #[test]
    fn summary_roundtrips_through_the_strict_wire_schema() {
        let summary = fixture_summary();

        let json = serde_json::to_string(&summary).expect("summary should encode");
        let decoded: ContinuitySummary =
            serde_json::from_str(&json).expect("summary should decode");

        assert_eq!(decoded, summary);
        assert_eq!(decoded.version, CONTINUITY_SUMMARY_VERSION);
        assert!(json.contains("\"schema_version\":1"));
        assert!(!json.contains("\"version\":"));
    }

    #[test]
    fn wire_schema_rejects_missing_and_unknown_fields() {
        for field in [
            "schema_version",
            "source_seq_start",
            "source_seq_end",
            "current_goal",
            "constraints",
            "decisions",
            "completed_work",
            "workspace",
            "commands_and_tests",
            "unresolved_errors",
            "todos",
            "next_actions",
            "artifact_refs",
        ] {
            let mut missing =
                serde_json::to_value(fixture_summary()).expect("summary should encode");
            missing
                .as_object_mut()
                .expect("summary should be an object")
                .remove(field);
            assert!(
                serde_json::from_value::<ContinuitySummary>(missing).is_err(),
                "missing field should be rejected: {field}"
            );
        }

        let mut unknown = serde_json::to_value(fixture_summary()).expect("summary should encode");
        unknown
            .as_object_mut()
            .expect("summary should be an object")
            .insert("permission_override".to_string(), serde_json::json!(true));
        assert!(serde_json::from_value::<ContinuitySummary>(unknown).is_err());

        let mut nested = serde_json::to_value(fixture_summary()).expect("summary should encode");
        nested["workspace"]
            .as_object_mut()
            .expect("workspace should be an object")
            .insert("permission".to_string(), serde_json::json!("allow"));
        assert!(serde_json::from_value::<ContinuitySummary>(nested).is_err());
    }
}
