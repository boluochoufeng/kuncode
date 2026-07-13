use std::time::Instant;

use kuncode_core::completion::Usage;

mod commit;
mod store;
mod validation;

use super::{
    candidate::{CandidateState, deterministic_candidate, semantic_candidate},
    types::{CompactionDependencies, CompactionError, CompactionOutcome, CompactionPass},
};
use crate::{
    compaction::{
        artifact::{ArtifactSpillInput, ArtifactSpillOutcome, spill_artifacts},
        budget::{BudgetLevel, CompactionMode, ContextBudget},
        protocol::{
            HumanMessageIndex, ProtocolGroup, group_messages,
            select_protected_recent_tail_from_estimates,
        },
        selection::{CandidateLoad, SelectionLimits, SelectionOutcome, select_prefix_tail},
        slimming::{SlimmingOutcome, production_slimming_candidates, slim_tool_results},
    },
    session_store::active_messages_sha256,
};
use commit::{CommitPlan, commit_and_install};
use store::SessionArtifactStore;
use validation::validate_candidate;

pub(crate) async fn compact_context(
    input: CompactionDependencies<'_>,
) -> Result<CompactionOutcome, CompactionError> {
    if input.config.mode() == CompactionMode::Disabled {
        return Ok(CompactionOutcome::Bypassed);
    }
    let before = input.measured_before;
    if input.config.mode() == CompactionMode::Shadow {
        return Ok(CompactionOutcome::Observed(before));
    }
    if before.level(input.config) == BudgetLevel::Normal {
        return Ok(CompactionOutcome::NotNeeded(before));
    }
    if !input.session.is_durable() {
        return Err(CompactionError::NonDurableSession);
    }
    let session_id = input
        .session
        .session_id()
        .cloned()
        .ok_or(CompactionError::NonDurableSession)?;
    let input_hash = active_messages_sha256(input.session.messages())?;
    let groups = group_messages(input.session.messages())?;
    let protected = protected_tail(&input, &groups, before).await?;
    let required_tail = groups[protected.group_range.clone()].to_vec();
    let store = SessionArtifactStore(input.store);
    let spill_input = ArtifactSpillInput::new(&groups, &protected, input.session)?;
    let artifacts = match spill_artifacts(spill_input, &store, input.artifact_counter).await {
        Ok(artifacts) => artifacts,
        Err(error) => {
            if matches!(
                error,
                crate::compaction::artifact::ArtifactSpillError::PersistenceOutcomeUnknown { .. }
                    | crate::compaction::artifact::ArtifactSpillError::ReceiptMismatch
                    | crate::compaction::artifact::ArtifactSpillError::JournalHeadConflict { .. }
            ) {
                input
                    .session
                    .mark_persistence_failed("artifact persistence frontier is no longer trusted");
            }
            return Err(error.into());
        }
    };
    input.session.advance_durable_seq(artifacts.frontier());
    let artifact_count = artifacts
        .outcomes()
        .iter()
        .filter(|outcome| matches!(outcome, ArtifactSpillOutcome::Spilled { .. }))
        .count();
    let mut passes = Vec::new();
    if artifact_count > 0 {
        passes.push(CompactionPass::ArtifactSpill);
    }
    let artifact_budget = candidate_budget(&input, artifacts.groups()).await?;
    let (candidate, after, summary_usage, summary_latency_ms) =
        if artifact_budget.reached_target(input.config) {
            (
                deterministic_candidate(input.session, artifacts.groups(), &artifacts)?,
                artifact_budget,
                None,
                None,
            )
        } else {
            compact_after_artifacts(&input, &artifacts, &protected, &mut passes).await?
        };
    validate_candidate(
        &input,
        &candidate,
        &required_tail,
        artifacts.frontier(),
        before,
        after,
    )?;
    commit_and_install(
        input,
        CommitPlan {
            session_id,
            input_hash,
            expected_head: artifacts.frontier(),
            artifact_count,
            passes,
            before,
            after,
            summary_usage,
            summary_latency_ms,
            candidate,
        },
    )
    .await
}

async fn compact_after_artifacts(
    input: &CompactionDependencies<'_>,
    artifacts: &crate::compaction::artifact::ArtifactSpillResult,
    protected: &crate::compaction::protocol::ProtectedRecentTail,
    passes: &mut Vec<CompactionPass>,
) -> Result<(CandidateState, ContextBudget, Option<Usage>, Option<u64>), CompactionError> {
    let authorized = production_slimming_candidates(artifacts, protected);
    let slimmed =
        slim_tool_results(artifacts, protected, &authorized, input.artifact_counter).await?;
    if slimmed
        .outcomes()
        .iter()
        .any(|outcome| matches!(outcome, SlimmingOutcome::Slimmed { .. }))
    {
        passes.push(CompactionPass::ToolResultSlimming);
    }
    let slim_budget = candidate_budget(input, slimmed.groups()).await?;
    if slim_budget.reached_target(input.config) {
        return Ok((
            deterministic_candidate(input.session, slimmed.groups(), artifacts)?,
            slim_budget,
            None,
            None,
        ));
    }
    let limits = selection_limits(input.config, slim_budget)?;
    let humans = input
        .session
        .trusted_human_message_indices()
        .map(HumanMessageIndex)
        .collect::<Vec<_>>();
    match select_prefix_tail(
        slimmed.groups(),
        input.session.messages(),
        protected,
        &humans,
        limits,
        slim_budget.current_input(),
    )? {
        SelectionOutcome::DeterministicCandidate { load }
            if load != CandidateLoad::RequiresCompaction =>
        {
            Ok((
                deterministic_candidate(input.session, slimmed.groups(), artifacts)?,
                slim_budget,
                None,
                None,
            ))
        }
        SelectionOutcome::Summarize(selection) => {
            let request = input
                .session
                .issue_slimmed_summary_request(&slimmed, &selection)?;
            let started = Instant::now();
            let generated = input.summarizer.summarize(request).await?;
            let summary_latency_ms = elapsed_ms(started);
            let usage = generated.usage;
            let candidate = semantic_candidate(
                input.session,
                &selection,
                generated.summary,
                input.summary_model,
                usage,
            )?;
            let budget = candidate_message_budget(input, &candidate).await?;
            passes.push(CompactionPass::SemanticSummary);
            Ok((candidate, budget, Some(usage), Some(summary_latency_ms)))
        }
        SelectionOutcome::DeterministicCandidate { .. }
        | SelectionOutcome::Uncompressible { .. } => Err(CompactionError::NoSafeBoundary),
    }
}

async fn protected_tail(
    input: &CompactionDependencies<'_>,
    groups: &[ProtocolGroup],
    budget: ContextBudget,
) -> Result<crate::compaction::protocol::ProtectedRecentTail, CompactionError> {
    let mut estimates = Vec::with_capacity(groups.len());
    for group in groups {
        estimates.push(input.group_estimator.estimate(group).await?);
    }
    let recent = ratio_tokens(budget.usable_input_limit(), input.config.recent_ratio());
    select_protected_recent_tail_from_estimates(groups, recent, &estimates)
        .ok_or(CompactionError::NoSafeBoundary)
}

async fn candidate_budget(
    input: &CompactionDependencies<'_>,
    groups: &[ProtocolGroup],
) -> Result<ContextBudget, CompactionError> {
    let messages = crate::compaction::protocol::flatten_groups(groups);
    let request = input.projector.project(&messages)?;
    Ok(ContextBudget::for_request(input.config, &request, input.estimator).await?)
}

async fn candidate_message_budget(
    input: &CompactionDependencies<'_>,
    candidate: &CandidateState,
) -> Result<ContextBudget, CompactionError> {
    let request = input.projector.project(&candidate.messages)?;
    Ok(ContextBudget::for_request(input.config, &request, input.estimator).await?)
}

fn selection_limits(
    config: &crate::compaction::budget::CompactionConfig,
    budget: ContextBudget,
) -> Result<SelectionLimits, CompactionError> {
    let target = ratio_tokens(budget.usable_input_limit(), config.target_ratio()).max(1);
    let soft = ratio_tokens(budget.usable_input_limit(), config.soft_threshold());
    SelectionLimits::new(target, soft).map_err(|_| CompactionError::InvalidThresholds)
}

fn ratio_tokens(limit: u64, ratio: f64) -> u64 {
    (limit as f64 * ratio).floor() as u64
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
