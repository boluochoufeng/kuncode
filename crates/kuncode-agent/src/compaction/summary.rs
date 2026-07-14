//! Versioned semantic continuity summaries and their untrusted-input prompt.

mod projection;
mod prompt;
mod summarizer;
mod types;
mod validation;

pub(crate) use projection::{
    COMPACTED_CONTEXT_SYSTEM_INSTRUCTION, is_compacted_context_message, project_summary_message,
};
pub use prompt::{SummaryRequest, build_summary_prompt};
pub use summarizer::{ContextSummarizer, GeneratedSummary, LlmContextSummarizer, SummarizerError};
pub use types::{
    CONTINUITY_SUMMARY_VERSION, CommandSummary, ContinuitySummary, SummaryTodo, SummaryTodoStatus,
    WorkspaceSummary,
};
pub use validation::SummaryError;

/// Returns the structured-output schema for [`ContinuitySummary`].
///
/// # Errors
/// Returns [`SummaryError::SchemaEncoding`] if the generated schema cannot be
/// represented as JSON.
pub fn continuity_summary_schema() -> Result<serde_json::Value, SummaryError> {
    serde_json::to_value(schemars::schema_for!(ContinuitySummary))
        .map_err(|error| SummaryError::SchemaEncoding(error.to_string()))
}

#[cfg(test)]
mod test_support {
    use super::{
        CONTINUITY_SUMMARY_VERSION, CommandSummary, ContinuitySummary, SummaryTodo,
        SummaryTodoStatus, WorkspaceSummary,
    };
    use crate::session_store::Seq;

    pub(super) const ALLOWED_ARTIFACT: &str =
        "tool-result-sha256-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    pub(super) fn fixture_summary() -> ContinuitySummary {
        ContinuitySummary {
            version: CONTINUITY_SUMMARY_VERSION,
            source_seq_start: Seq::new(2),
            source_seq_end: Seq::new(8),
            current_goal: "Implement context compaction".to_string(),
            constraints: vec!["Keep the journal immutable".to_string()],
            decisions: vec!["Resume is deferred because v1 has no runtime support".to_string()],
            completed_work: vec!["Implemented deterministic artifact spilling".to_string()],
            workspace: WorkspaceSummary {
                working_directory: "/workspace".to_string(),
                files: vec!["src/compaction.rs".to_string()],
                symbols: vec!["ContinuitySummary".to_string()],
            },
            commands_and_tests: vec![CommandSummary {
                command: "cargo test --workspace".to_string(),
                outcome: "passed".to_string(),
                exit_code: Some(0),
            }],
            unresolved_errors: vec![],
            todos: vec![SummaryTodo {
                content: "Implement the summarizer".to_string(),
                status: SummaryTodoStatus::Pending,
            }],
            next_actions: vec!["Validate model JSON".to_string()],
            artifact_refs: vec![ALLOWED_ARTIFACT.to_string()],
        }
    }
}

#[cfg(test)]
mod schema_tests {
    use super::continuity_summary_schema;

    #[test]
    fn generated_schema_is_strict_and_uses_durable_wire_names() {
        let schema = continuity_summary_schema().expect("schema should encode");
        assert!(schema["properties"].get("schema_version").is_some());
        assert_eq!(schema["additionalProperties"], false);
        assert!(
            schema["required"]
                .as_array()
                .is_some_and(|fields| fields.iter().any(|field| field == "current_goal"))
        );
    }
}

#[cfg(test)]
mod tests;
