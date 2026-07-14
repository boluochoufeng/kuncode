use super::validation::SummaryValidationContext;
use super::{
    CONTINUITY_SUMMARY_VERSION, CommandSummary, ContinuitySummary, SummaryError, SummaryRequest,
    SummaryTodo, SummaryTodoStatus, WorkspaceSummary, continuity_summary_schema,
};
use crate::session_store::Seq;

mod binding;
mod bounds;
mod prompt;
mod recursive;

pub(super) const ALLOWED_ARTIFACT: &str =
    "tool-result-sha256-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn summary_roundtrips_through_the_strict_wire_schema() {
    let summary = fixture_summary();

    let json = serde_json::to_string(&summary).expect("summary should encode");
    let decoded: ContinuitySummary = serde_json::from_str(&json).expect("summary should decode");

    assert_eq!(decoded, summary);
    assert_eq!(decoded.version, CONTINUITY_SUMMARY_VERSION);
    assert!(json.contains("\"schema_version\":1"));
    assert!(!json.contains("\"version\":"));
}

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
        let mut missing = serde_json::to_value(fixture_summary()).expect("summary should encode");
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

#[test]
fn validation_rejects_version_range_and_artifact_mismatches() {
    let context =
        SummaryValidationContext::new(Seq::new(2), Seq::new(8), Seq::new(8), [ALLOWED_ARTIFACT])
            .expect("validation source should be valid");

    let mut wrong_version = fixture_summary();
    wrong_version.version += 1;
    assert_eq!(
        wrong_version.validate(&context),
        Err(SummaryError::UnsupportedVersion {
            expected: CONTINUITY_SUMMARY_VERSION,
            actual: CONTINUITY_SUMMARY_VERSION + 1,
        })
    );

    let mut wrong_range = fixture_summary();
    wrong_range.source_seq_end = Seq::new(7);
    assert!(matches!(
        wrong_range.validate(&context),
        Err(SummaryError::SourceRangeMismatch { .. })
    ));

    let mut unknown_artifact = fixture_summary();
    let forged =
        "tool-result-sha256-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    unknown_artifact.artifact_refs = vec![forged.to_string()];
    assert_eq!(
        unknown_artifact.validate(&context),
        Err(SummaryError::UnknownArtifactRef(forged.to_string()))
    );
}

#[test]
fn validation_rejects_invalid_ranges_blank_fields_and_duplicate_refs() {
    assert_eq!(
        SummaryValidationContext::new(
            Seq::new(8),
            Seq::new(2),
            Seq::new(8),
            std::iter::empty::<&str>(),
        ),
        Err(SummaryError::InvalidSourceRange { start: 8, end: 2 })
    );

    assert_eq!(
        SummaryValidationContext::new(
            Seq::new(2),
            Seq::new(9),
            Seq::new(8),
            std::iter::empty::<&str>(),
        ),
        Err(SummaryError::SourceBeyondDurableHead {
            end: 9,
            durable_head: 8,
        })
    );

    let context =
        SummaryValidationContext::new(Seq::new(2), Seq::new(8), Seq::new(8), [ALLOWED_ARTIFACT])
            .expect("validation source should be valid");
    let mut blank = fixture_summary();
    blank.current_goal = "  ".to_string();
    assert_eq!(
        blank.validate(&context),
        Err(SummaryError::BlankField("current_goal".to_string()))
    );

    let mut duplicate = fixture_summary();
    duplicate
        .artifact_refs
        .push(duplicate.artifact_refs[0].clone());
    assert_eq!(
        duplicate.validate(&context),
        Err(SummaryError::DuplicateArtifactRef(
            ALLOWED_ARTIFACT.to_string()
        ))
    );
}

#[test]
fn summary_request_rejects_empty_history() {
    let context = SummaryValidationContext::new(
        Seq::new(1),
        Seq::new(2),
        Seq::new(2),
        std::iter::empty::<&str>(),
    )
    .expect("validation source should be valid");
    assert!(matches!(
        SummaryRequest::new(None, vec![], context),
        Err(SummaryError::EmptySourceMessages)
    ));
}

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
