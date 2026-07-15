use kuncode_core::completion::{Message, UserContent};

use super::super::test_support::{ALLOWED_ARTIFACT, fixture_summary};
use crate::{
    compaction::summary::{
        SummaryError, SummaryRequest, build_summary_prompt, validation::SummaryValidationContext,
    },
    session_store::Seq,
};

#[test]
fn rejects_incompatible_recursive_sources() {
    let previous = fixture_summary();
    // Advancing the start would silently discard provenance represented by the
    // previous summary even though the new range still overlaps it.
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

#[test]
fn recursive_prompt_and_fixture_replace_a_superseded_conclusion() {
    let old = "Use JSONL because no database is available";
    let current = "Use Turso; JSONL is superseded because atomic CAS is required";
    let mut previous = fixture_summary();
    previous.decisions = vec![old.to_string()];
    let context =
        SummaryValidationContext::new(Seq::new(2), Seq::new(10), Seq::new(10), [ALLOWED_ARTIFACT])
            .expect("validation source should be valid");
    let request = SummaryRequest::new(
        Some(previous.clone()),
        vec![Message::user(
            "Correction: use Turso, not JSONL, because compaction needs atomic CAS.",
        )],
        context,
    )
    .expect("recursive request should be valid");

    let prompt = build_summary_prompt(&request).expect("prompt should encode");
    let Message::System { content: system } = prompt.first() else {
        panic!("summary prompt should start with system authority");
    };
    assert!(system.contains("explicitly labeled superseded"));
    let Message::User { content } = &prompt[1] else {
        panic!("summary source should remain user-role data");
    };
    let UserContent::Text(text) = content.first() else {
        panic!("summary source should be JSON text");
    };
    assert!(text.text_ref().contains(old));
    assert!(text.text_ref().contains("Correction: use Turso"));

    // The previous conclusion remains source evidence, but the replacement output
    // carries only the currently supported decision rather than accumulating both.
    let mut corrected = previous;
    corrected.source_seq_end = Seq::new(10);
    corrected.decisions = vec![current.to_string()];
    let parsed = request
        .parse_and_validate(
            &serde_json::to_string(&corrected).expect("fixture summary should encode"),
        )
        .expect("corrected summary should satisfy deterministic gates");
    assert_eq!(parsed.decisions, [current]);
    assert!(!parsed.decisions.iter().any(|decision| decision == old));
}
