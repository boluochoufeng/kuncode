use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::session_store::Seq;

/// Wire version accepted by the first continuity-summary validator.
pub const CONTINUITY_SUMMARY_VERSION: u32 = 1;

/// Lossy semantic continuity extracted from one durable journal range.
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
    #[serde(with = "seq_serde")]
    #[schemars(with = "i64")]
    #[schemars(range(min = 1))]
    pub source_seq_start: Seq,
    /// Last durable journal fact represented by this summary.
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
