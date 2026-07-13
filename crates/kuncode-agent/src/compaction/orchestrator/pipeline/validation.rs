//! Final safety gates applied before durable candidate commit.

use super::super::{
    candidate::CandidateState,
    types::{CompactionDependencies, CompactionError},
};
use crate::{
    compaction::{
        budget::{BudgetLevel, ContextBudget},
        protocol::{ProtocolGroup, group_messages},
    },
    session_store::Seq,
};

pub(super) fn validate_candidate(
    input: &CompactionDependencies<'_>,
    candidate: &CandidateState,
    required_tail: &[ProtocolGroup],
    expected_head: Seq,
    before: ContextBudget,
    after: ContextBudget,
) -> Result<(), CompactionError> {
    let candidate_groups = group_messages(&candidate.messages)?;
    if !candidate_groups.ends_with(required_tail) {
        return Err(CompactionError::ProtectedTailChanged);
    }
    if before.current_input() <= after.current_input() {
        return Err(CompactionError::InsufficientReduction);
    }
    if after.level(input.config) != BudgetLevel::Normal {
        return Err(CompactionError::AboveSoftThreshold);
    }
    if candidate.messages.len() != candidate.lineage.len() {
        return Err(CompactionError::InvalidLineage);
    }
    if candidate.source_start > candidate.source_end || candidate.source_end > expected_head {
        return Err(CompactionError::InvalidLineage);
    }
    for lineage in &candidate.lineage {
        let coverage = lineage.coverage().ok_or(CompactionError::InvalidLineage)?;
        if coverage.start() > coverage.end() || coverage.end() > expected_head {
            return Err(CompactionError::InvalidLineage);
        }
    }
    Ok(())
}
