use kuncode_core::completion::Message;

use super::{ALLOWED_ARTIFACT, fixture_summary};
use crate::{
    compaction::summary::{SummaryError, SummaryRequest, validation::SummaryValidationContext},
    session_store::Seq,
};

#[test]
fn rejects_incompatible_recursive_sources() {
    let previous = fixture_summary();
    let context =
        SummaryValidationContext::new(Seq::new(4), Seq::new(10), Seq::new(10), [ALLOWED_ARTIFACT])
            .expect("validation source should be valid");

    assert!(matches!(
        SummaryRequest::new(Some(previous), vec![Message::user("new facts")], context),
        Err(SummaryError::PreviousSourceRangeNotCovered { .. })
    ));
}

#[test]
fn accepts_valid_recursive_extension() {
    let previous = fixture_summary();
    let context =
        SummaryValidationContext::new(Seq::new(1), Seq::new(10), Seq::new(10), [ALLOWED_ARTIFACT])
            .expect("validation source should be valid");

    SummaryRequest::new(Some(previous), vec![Message::user("new facts")], context)
        .expect("recursive source should remain covered");
}

#[test]
fn revalidates_recursive_version_and_artifacts() {
    let context =
        SummaryValidationContext::new(Seq::new(1), Seq::new(10), Seq::new(10), [ALLOWED_ARTIFACT])
            .expect("validation source should be valid");
    let mut wrong_version = fixture_summary();
    wrong_version.version += 1;
    assert!(matches!(
        SummaryRequest::new(
            Some(wrong_version),
            vec![Message::user("new facts")],
            context.clone(),
        ),
        Err(SummaryError::UnsupportedVersion { .. })
    ));

    let mut forged_artifact = fixture_summary();
    forged_artifact.artifact_refs = vec![
        "tool-result-sha256-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            .to_string(),
    ];
    assert!(matches!(
        SummaryRequest::new(
            Some(forged_artifact),
            vec![Message::user("new facts")],
            context,
        ),
        Err(SummaryError::UnknownArtifactRef(_))
    ));
}
