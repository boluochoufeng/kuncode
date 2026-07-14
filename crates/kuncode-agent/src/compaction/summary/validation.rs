//! Fails closed when decoding and validating model-controlled summary output.
//!
//! Validation binds the closed wire schema to one observed durable range and an
//! explicit artifact allowlist. Raw and per-field bounds cap memory retained from
//! model output, while recursive requests must continue to cover prior provenance.

use std::collections::BTreeSet;

use thiserror::Error;

use super::{
    CONTINUITY_SUMMARY_VERSION, ContinuitySummary,
    types::{CommandSummary, SummaryTodo, WorkspaceSummary},
};
use crate::session_store::Seq;

mod bounds;

use bounds::{MAX_SUMMARY_JSON_BYTES, validate_allowed_artifact_refs, validate_summary_bounds};

/// Durable source facts used to validate untrusted summary output.
///
/// The context is minted from one selection boundary; output cannot choose a
/// different range or introduce artifact references absent from that source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SummaryValidationContext {
    source_seq_start: Seq,
    source_seq_end: Seq,
    allowed_artifact_refs: BTreeSet<String>,
}

impl SummaryValidationContext {
    /// Binds validation to one exact source range and artifact set.
    ///
    /// # Errors
    /// Returns [`SummaryError`] for an invalid range, a source beyond the
    /// durable head, or malformed and excessive artifact references.
    pub(super) fn new<I, S>(
        source_seq_start: Seq,
        source_seq_end: Seq,
        durable_head: Seq,
        allowed_artifact_refs: I,
    ) -> Result<Self, SummaryError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        validate_source_range(source_seq_start, source_seq_end)?;
        if source_seq_end > durable_head {
            return Err(SummaryError::SourceBeyondDurableHead {
                end: source_seq_end.get(),
                durable_head: durable_head.get(),
            });
        }
        let allowed_artifact_refs = allowed_artifact_refs
            .into_iter()
            .map(Into::into)
            .collect::<BTreeSet<_>>();
        validate_allowed_artifact_refs(&allowed_artifact_refs)?;
        Ok(Self {
            source_seq_start,
            source_seq_end,
            allowed_artifact_refs,
        })
    }

    pub(super) const fn source_range(&self) -> (Seq, Seq) {
        (self.source_seq_start, self.source_seq_end)
    }

    pub(super) const fn allowed_artifact_refs(&self) -> &BTreeSet<String> {
        &self.allowed_artifact_refs
    }

    pub(super) fn parse_and_validate(&self, raw: &str) -> Result<ContinuitySummary, SummaryError> {
        parse_and_validate_summary(raw, self)
    }
}

/// Deterministic rejection of untrusted continuity-summary data.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SummaryError {
    /// The output targets a schema version the harness does not understand.
    #[error("unsupported continuity summary version {actual}; expected {expected}")]
    UnsupportedVersion {
        /// Version implemented by this harness.
        expected: u32,
        /// Version supplied by the untrusted output.
        actual: u32,
    },
    /// A source range cannot identify durable journal facts.
    #[error("invalid summary source range {start}..={end}")]
    InvalidSourceRange {
        /// Inclusive start supplied by the caller or output.
        start: i64,
        /// Inclusive end supplied by the caller or output.
        end: i64,
    },
    /// The selected source claims facts beyond the observed journal frontier.
    #[error("summary source end {end} is beyond durable journal head {durable_head}")]
    SourceBeyondDurableHead {
        /// Inclusive source end supplied by the orchestrator.
        end: i64,
        /// Durable head observed before summary generation.
        durable_head: i64,
    },
    /// Output provenance differs from the selection-bound range.
    #[error(
        "summary source range {actual_start}..={actual_end} differs from expected {expected_start}..={expected_end}"
    )]
    SourceRangeMismatch {
        /// Selection-bound inclusive start.
        expected_start: i64,
        /// Selection-bound inclusive end.
        expected_end: i64,
        /// Output inclusive start.
        actual_start: i64,
        /// Output inclusive end.
        actual_end: i64,
    },
    /// A recursive summary may only extend the range already represented.
    #[error(
        "new summary source range {new_start}..={new_end} does not cover previous range {previous_start}..={previous_end}"
    )]
    PreviousSourceRangeNotCovered {
        /// Inclusive start stored by the previous validated summary.
        previous_start: i64,
        /// Inclusive end stored by the previous validated summary.
        previous_end: i64,
        /// Inclusive start selected for the new summary.
        new_start: i64,
        /// Inclusive end selected for the new summary.
        new_end: i64,
    },
    /// A required semantic field contains only whitespace.
    #[error("continuity summary field `{0}` must not be blank")]
    BlankField(String),
    /// An artifact reference was not present in the summarized source.
    #[error("continuity summary references unknown artifact `{0}`")]
    UnknownArtifactRef(String),
    /// Repeating an artifact reference adds ambiguity without information.
    #[error("continuity summary repeats artifact `{0}`")]
    DuplicateArtifactRef(String),
    /// A summary request must contain the non-empty selected prefix.
    #[error("summary request source messages must not be empty")]
    EmptySourceMessages,
    /// Prompt construction could not encode the untrusted payload.
    #[error("summary prompt JSON encoding failed: {0}")]
    PromptEncoding(String),
    /// JSON Schema generation failed before a structured request could be built.
    #[error("continuity summary schema encoding failed: {0}")]
    SchemaEncoding(String),
    /// Raw model output exceeded the fixed decoding budget.
    #[error("continuity summary JSON is {actual} bytes; maximum is {max}")]
    SummaryTooLarge {
        /// Maximum accepted UTF-8 byte length.
        max: usize,
        /// Actual UTF-8 byte length.
        actual: usize,
    },
    /// Strict JSON decoding rejected the model output.
    #[error("continuity summary JSON decoding failed: {0}")]
    Decode(String),
    /// One semantic field exceeded its fixed UTF-8 byte limit.
    #[error("continuity summary field `{field}` is {actual} bytes; maximum is {max}")]
    FieldTooLarge {
        /// Stable field path used for diagnostics.
        field: String,
        /// Maximum accepted UTF-8 byte length.
        max: usize,
        /// Actual UTF-8 byte length.
        actual: usize,
    },
    /// One collection exceeded its fixed item limit.
    #[error("continuity summary field `{field}` has {actual} items; maximum is {max}")]
    TooManyItems {
        /// Stable field path used for diagnostics.
        field: String,
        /// Maximum accepted item count.
        max: usize,
        /// Actual item count.
        actual: usize,
    },
    /// Artifact identifiers must use the durable content-addressed format.
    #[error("invalid continuity summary artifact reference `{0}`")]
    InvalidArtifactRef(String),
}

impl ContinuitySummary {
    /// Validates version, exact provenance, required text, and artifact origin.
    ///
    /// Decoding into the closed schema is necessary but insufficient: these gates
    /// bind otherwise well-formed model output back to the exact durable source.
    ///
    /// # Errors
    /// Returns [`SummaryError`] when any deterministic hard gate fails.
    pub(super) fn validate(&self, context: &SummaryValidationContext) -> Result<(), SummaryError> {
        if self.version != CONTINUITY_SUMMARY_VERSION {
            return Err(SummaryError::UnsupportedVersion {
                expected: CONTINUITY_SUMMARY_VERSION,
                actual: self.version,
            });
        }
        validate_summary_bounds(self)?;
        validate_source_range(self.source_seq_start, self.source_seq_end)?;
        if self.source_seq_start != context.source_seq_start
            || self.source_seq_end != context.source_seq_end
        {
            return Err(SummaryError::SourceRangeMismatch {
                expected_start: context.source_seq_start.get(),
                expected_end: context.source_seq_end.get(),
                actual_start: self.source_seq_start.get(),
                actual_end: self.source_seq_end.get(),
            });
        }
        validate_semantic_fields(self)?;
        validate_artifact_refs(&self.artifact_refs, &context.allowed_artifact_refs)
    }
}

/// Strictly decodes and validates one untrusted model output.
///
/// # Errors
/// Returns [`SummaryError`] for size, wire-shape, version, provenance, field,
/// or artifact-reference violations.
fn parse_and_validate_summary(
    raw: &str,
    context: &SummaryValidationContext,
) -> Result<ContinuitySummary, SummaryError> {
    // Bound the complete input before serde can allocate collections controlled by
    // the model; field and item limits below further constrain retained structure.
    if raw.len() > MAX_SUMMARY_JSON_BYTES {
        return Err(SummaryError::SummaryTooLarge {
            max: MAX_SUMMARY_JSON_BYTES,
            actual: raw.len(),
        });
    }
    let summary = serde_json::from_str::<ContinuitySummary>(raw)
        .map_err(|error| SummaryError::Decode(error.to_string()))?;
    summary.validate(context)?;
    Ok(summary)
}

pub(super) fn validate_source_range(start: Seq, end: Seq) -> Result<(), SummaryError> {
    if start <= Seq::ZERO || end < start {
        Err(SummaryError::InvalidSourceRange {
            start: start.get(),
            end: end.get(),
        })
    } else {
        Ok(())
    }
}

fn validate_semantic_fields(summary: &ContinuitySummary) -> Result<(), SummaryError> {
    non_blank("current_goal", &summary.current_goal)?;
    validate_strings("constraints", &summary.constraints)?;
    validate_strings("decisions", &summary.decisions)?;
    validate_strings("completed_work", &summary.completed_work)?;
    validate_workspace(&summary.workspace)?;
    validate_commands(&summary.commands_and_tests)?;
    validate_strings("unresolved_errors", &summary.unresolved_errors)?;
    validate_todos(&summary.todos)?;
    validate_strings("next_actions", &summary.next_actions)
}

fn validate_workspace(workspace: &WorkspaceSummary) -> Result<(), SummaryError> {
    non_blank("workspace.working_directory", &workspace.working_directory)?;
    validate_strings("workspace.files", &workspace.files)?;
    validate_strings("workspace.symbols", &workspace.symbols)
}

fn validate_commands(commands: &[CommandSummary]) -> Result<(), SummaryError> {
    for (index, command) in commands.iter().enumerate() {
        non_blank(
            &format!("commands_and_tests[{index}].command"),
            &command.command,
        )?;
        non_blank(
            &format!("commands_and_tests[{index}].outcome"),
            &command.outcome,
        )?;
    }
    Ok(())
}

fn validate_todos(todos: &[SummaryTodo]) -> Result<(), SummaryError> {
    for (index, todo) in todos.iter().enumerate() {
        non_blank(&format!("todos[{index}].content"), &todo.content)?;
    }
    Ok(())
}

fn validate_strings(field: &str, values: &[String]) -> Result<(), SummaryError> {
    for (index, value) in values.iter().enumerate() {
        non_blank(&format!("{field}[{index}]"), value)?;
    }
    Ok(())
}

fn non_blank(field: &str, value: &str) -> Result<(), SummaryError> {
    if value.trim().is_empty() {
        Err(SummaryError::BlankField(field.to_string()))
    } else {
        Ok(())
    }
}

fn validate_artifact_refs(
    artifact_refs: &[String],
    allowed: &BTreeSet<String>,
) -> Result<(), SummaryError> {
    let mut seen = BTreeSet::new();
    for artifact_ref in artifact_refs {
        non_blank("artifact_refs", artifact_ref)?;
        if !bounds::is_artifact_id(artifact_ref) {
            return Err(SummaryError::InvalidArtifactRef(artifact_ref.clone()));
        }
        if !seen.insert(artifact_ref) {
            return Err(SummaryError::DuplicateArtifactRef(artifact_ref.clone()));
        }
        if !allowed.contains(artifact_ref) {
            return Err(SummaryError::UnknownArtifactRef(artifact_ref.clone()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{
        CONTINUITY_SUMMARY_VERSION,
        test_support::{ALLOWED_ARTIFACT, fixture_summary},
    };
    use super::{SummaryError, SummaryValidationContext};
    use crate::session_store::Seq;

    #[test]
    fn validation_rejects_version_range_and_artifact_mismatches() {
        let context = SummaryValidationContext::new(
            Seq::new(2),
            Seq::new(8),
            Seq::new(8),
            [ALLOWED_ARTIFACT],
        )
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

        let context = SummaryValidationContext::new(
            Seq::new(2),
            Seq::new(8),
            Seq::new(8),
            [ALLOWED_ARTIFACT],
        )
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
    fn strict_parser_applies_wire_and_context_validation() {
        let context = context();
        let raw = serde_json::to_string(&fixture_summary()).expect("summary should encode");

        let parsed = context
            .parse_and_validate(&raw)
            .expect("summary should validate");

        assert_eq!(parsed, fixture_summary());
        assert!(matches!(
            context.parse_and_validate("```json\n{}\n```"),
            Err(SummaryError::Decode(_))
        ));
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
}
