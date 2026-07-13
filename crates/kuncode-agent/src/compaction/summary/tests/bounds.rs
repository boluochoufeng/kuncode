use super::{ALLOWED_ARTIFACT, fixture_summary};
use crate::{
    compaction::summary::{SummaryError, SummaryRequest},
    session_store::Seq,
};
use kuncode_core::completion::Message;

use super::super::validation::SummaryValidationContext;

#[test]
fn strict_parser_applies_wire_and_context_validation() {
    let request = request();
    let raw = serde_json::to_string(&fixture_summary()).expect("summary should encode");

    let parsed = request
        .parse_and_validate(&raw)
        .expect("summary should validate");

    assert_eq!(parsed, fixture_summary());
    assert!(matches!(
        request.parse_and_validate("```json\n{}\n```"),
        Err(SummaryError::Decode(_))
    ));
}

#[test]
fn strict_parser_rejects_resource_exhaustion_shapes() {
    let oversized = " ".repeat(256 * 1_024 + 1);
    assert_eq!(
        request().parse_and_validate(&oversized),
        Err(SummaryError::SummaryTooLarge {
            max: 256 * 1_024,
            actual: 256 * 1_024 + 1,
        })
    );

    let mut long_goal = fixture_summary();
    long_goal.current_goal = "x".repeat(8 * 1_024 + 1);
    assert_eq!(
        long_goal.validate(&context()),
        Err(SummaryError::FieldTooLarge {
            field: "current_goal".to_string(),
            max: 8 * 1_024,
            actual: 8 * 1_024 + 1,
        })
    );

    let mut many_constraints = fixture_summary();
    many_constraints.constraints = (0..65).map(|index| format!("constraint {index}")).collect();
    assert_eq!(
        many_constraints.validate(&context()),
        Err(SummaryError::TooManyItems {
            field: "constraints".to_string(),
            max: 64,
            actual: 65,
        })
    );
}

#[test]
fn source_context_and_output_reject_malformed_artifact_ids() {
    let invalid = "tool-result-sha256-NOT-A-HASH";
    assert_eq!(
        SummaryValidationContext::new(Seq::new(2), Seq::new(8), Seq::new(8), [invalid]),
        Err(SummaryError::InvalidArtifactRef(invalid.to_string()))
    );

    let mut summary = fixture_summary();
    summary.artifact_refs = vec![invalid.to_string()];
    assert_eq!(
        summary.validate(&context()),
        Err(SummaryError::InvalidArtifactRef(invalid.to_string()))
    );
}

#[test]
fn source_context_rejects_zero_and_negative_sequences() {
    for start in [Seq::ZERO, Seq::new(-1)] {
        assert_eq!(
            SummaryValidationContext::new(
                start,
                Seq::new(8),
                Seq::new(8),
                std::iter::empty::<&str>(),
            ),
            Err(SummaryError::InvalidSourceRange {
                start: start.get(),
                end: 8,
            })
        );
    }
}

fn context() -> SummaryValidationContext {
    SummaryValidationContext::new(Seq::new(2), Seq::new(8), Seq::new(8), [ALLOWED_ARTIFACT])
        .expect("validation source should be valid")
}

fn request() -> SummaryRequest {
    SummaryRequest::new(None, vec![Message::user("source")], context())
        .expect("summary request should be valid")
}
