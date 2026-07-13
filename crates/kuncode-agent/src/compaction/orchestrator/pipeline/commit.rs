//! Receipt-bound atomic commit and in-memory installation.

use kuncode_core::completion::Usage;

use super::super::{
    candidate::CandidateState,
    types::{
        CompactionDependencies, CompactionError, CompactionOutcome, CompactionPass,
        CompactionReport,
    },
};
use crate::{
    compaction::budget::{BudgetLevel, ContextBudget},
    session_store::{
        CompactionEvent, CompactionMetadata, CompactionPassKind, CompactionReason, NewCheckpoint,
        NewCompactionCommit, Seq, SessionId, active_messages_sha256,
    },
};

pub(super) struct CommitPlan {
    pub(super) session_id: SessionId,
    pub(super) input_hash: String,
    pub(super) expected_head: Seq,
    pub(super) artifact_count: usize,
    pub(super) passes: Vec<CompactionPass>,
    pub(super) before: ContextBudget,
    pub(super) after: ContextBudget,
    pub(super) summary_usage: Option<Usage>,
    pub(super) summary_latency_ms: Option<u64>,
    pub(super) candidate: CandidateState,
}

pub(super) async fn commit_and_install(
    input: CompactionDependencies<'_>,
    plan: CommitPlan,
) -> Result<CompactionOutcome, CompactionError> {
    if input.session.durable_seq() != Some(plan.expected_head)
        || active_messages_sha256(input.session.messages())? != plan.input_hash
    {
        return Err(CompactionError::StaleActiveContext);
    }
    let source_start = plan.candidate.source_start;
    let source_end = plan.candidate.source_end;
    let output_hash = active_messages_sha256(&plan.candidate.messages)?;
    let active_messages = plan.candidate.messages.clone();
    let summary_json = plan
        .candidate
        .summary
        .as_ref()
        .map(|active| serde_json::to_value(active.summary()))
        .transpose()?;
    let summary_source = plan.candidate.summary.as_ref().map(|active| {
        (
            active.summary().source_seq_start,
            active.summary().source_seq_end,
        )
    });
    let checkpoint_model = plan
        .candidate
        .summary
        .as_ref()
        .map(|active| active.model().to_string());
    let checkpoint_usage = plan
        .candidate
        .summary
        .as_ref()
        .map(|active| serde_json::to_value(active.usage()))
        .transpose()?;
    let mut durable_passes = plan
        .passes
        .iter()
        .copied()
        .map(persisted_pass)
        .collect::<Vec<_>>();
    durable_passes.push(CompactionPassKind::AtomicCommit);
    let reason = match plan.before.level(input.config) {
        BudgetLevel::Soft => CompactionReason::SoftThreshold,
        BudgetLevel::Hard => CompactionReason::HardThreshold,
        BudgetLevel::Normal => return Err(CompactionError::InvalidThresholds),
    };
    let mut metadata = CompactionMetadata::new(reason, durable_passes);
    if let Some(usage) = plan.summary_usage {
        let generated_summary = summary_json
            .clone()
            .ok_or(CompactionError::InvalidLineage)?;
        metadata = metadata.with_generated_summary(
            generated_summary,
            input.summary_model,
            serde_json::to_value(usage)?,
        );
    }
    let checkpoint = NewCheckpoint {
        session_id: plan.session_id.clone(),
        covers_through_seq: plan.expected_head,
        source_seq_start: summary_source.map(|(start, _)| start),
        source_seq_end: summary_source.map(|(_, end)| end),
        active_messages,
        summary_json,
        model: checkpoint_model,
        token_usage_json: checkpoint_usage,
    };
    let prepared = plan.candidate.into_prepared()?;
    let commit = NewCompactionCommit {
        session_id: plan.session_id,
        expected_journal_head: plan.expected_head,
        event: CompactionEvent::new(
            plan.input_hash,
            output_hash,
            source_start..=source_end,
            metadata,
        ),
        checkpoint,
    };
    let committed = match input.store.commit_compaction(commit).await {
        Ok(receipt) => receipt,
        Err(error) => {
            if matches!(
                error,
                crate::session_store::SessionStoreError::CommitOutcomeUnknown { .. }
                    | crate::session_store::SessionStoreError::JournalHeadConflict { .. }
            ) {
                input.session.mark_persistence_failed(
                    "compaction persistence frontier is no longer trusted",
                );
            }
            return Err(error.into());
        }
    };
    let checkpoint_seq = committed.checkpoint_seq();
    if let Err(error) = input.session.install_compacted_context(prepared, committed) {
        input
            .session
            .mark_persistence_failed("committed compaction could not be installed");
        return Err(CompactionError::Apply(error.to_string()));
    }
    let mut passes = plan.passes;
    passes.push(CompactionPass::AtomicCommit);
    Ok(CompactionOutcome::Compacted(CompactionReport {
        before: plan.before,
        after: plan.after,
        passes,
        source_start,
        source_end,
        checkpoint_seq,
        artifact_count: plan.artifact_count,
        summary_usage: plan.summary_usage,
        summary_latency_ms: plan.summary_latency_ms,
        target_reached: plan.after.reached_target(input.config),
    }))
}

const fn persisted_pass(pass: CompactionPass) -> CompactionPassKind {
    match pass {
        CompactionPass::ArtifactSpill => CompactionPassKind::ArtifactSpill,
        CompactionPass::ToolResultSlimming => CompactionPassKind::ToolResultSlimming,
        CompactionPass::SemanticSummary => CompactionPassKind::SemanticSummary,
        CompactionPass::AtomicCommit => CompactionPassKind::AtomicCommit,
    }
}
