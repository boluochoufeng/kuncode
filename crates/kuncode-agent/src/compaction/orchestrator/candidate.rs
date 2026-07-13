use std::collections::BTreeSet;

use kuncode_core::completion::Message;

use super::types::CompactionError;
use crate::{
    compaction::{
        artifact::{ArtifactSpillOutcome, ArtifactSpillResult},
        protocol::{ProtocolGroup, flatten_groups},
        selection::CompactionSelection,
        summary::{ContinuitySummary, project_summary_message},
    },
    session::{
        ActiveSummary, AgentSession, MessageCoverage, MessageLineage, PreparedActiveContext,
    },
    session_store::Seq,
};

pub(super) struct CandidateState {
    pub(super) messages: Vec<Message>,
    pub(super) lineage: Vec<MessageLineage>,
    pub(super) summary: Option<ActiveSummary>,
    pub(super) source_start: Seq,
    pub(super) source_end: Seq,
}

impl CandidateState {
    pub(super) fn into_prepared(self) -> Result<PreparedActiveContext, CompactionError> {
        PreparedActiveContext::new(self.messages, self.lineage, self.summary)
            .ok_or(CompactionError::InvalidLineage)
    }
}

pub(super) fn deterministic_candidate(
    session: &AgentSession,
    groups: &[ProtocolGroup],
    artifacts: &ArtifactSpillResult,
) -> Result<CandidateState, CompactionError> {
    let mut lineage = session.message_lineage().to_vec();
    let messages = flatten_groups(groups);
    if messages.len() != lineage.len() || messages.len() != session.messages().len() {
        return Err(CompactionError::InvalidLineage);
    }
    let mut source_start = None;
    let mut source_end = None;
    for (index, (candidate, source_message)) in messages.iter().zip(session.messages()).enumerate()
    {
        if candidate == source_message {
            continue;
        }
        let source = lineage.get(index).ok_or(CompactionError::InvalidLineage)?;
        let coverage = source.coverage().ok_or(CompactionError::InvalidLineage)?;
        source_start = Some(source_start.map_or(coverage.start(), |current: Seq| {
            current.min(coverage.start())
        }));
        source_end =
            Some(source_end.map_or(coverage.end(), |current: Seq| current.max(coverage.end())));
        lineage[index] = MessageLineage::derived(
            coverage,
            source.human_authored(),
            source.artifact_refs().clone(),
        );
    }
    let starts = group_message_starts(groups);
    for outcome in artifacts.outcomes() {
        let ArtifactSpillOutcome::Spilled {
            location,
            artifact_id,
            ..
        } = outcome
        else {
            continue;
        };
        let index = starts
            .get(location.group_index)
            .and_then(|start| start.checked_add(1 + location.result_message_index))
            .ok_or(CompactionError::InvalidLineage)?;
        let source = lineage.get(index).ok_or(CompactionError::InvalidLineage)?;
        let coverage = source.coverage().ok_or(CompactionError::InvalidLineage)?;
        let mut refs = source.artifact_refs().clone();
        refs.insert(artifact_id.clone());
        lineage[index] = MessageLineage::derived(coverage, source.human_authored(), refs);
    }
    Ok(CandidateState {
        messages,
        lineage,
        summary: session.active_summary_record().cloned(),
        source_start: source_start.ok_or(CompactionError::InvalidLineage)?,
        source_end: source_end.ok_or(CompactionError::InvalidLineage)?,
    })
}

pub(super) fn semantic_candidate(
    session: &AgentSession,
    selection: &CompactionSelection,
    summary: ContinuitySummary,
    model: &str,
    usage: kuncode_core::completion::Usage,
) -> Result<CandidateState, CompactionError> {
    let prefix_messages = flatten_groups(selection.summarize()).len();
    let tail_lineage = session
        .message_lineage()
        .get(prefix_messages..)
        .ok_or(CompactionError::InvalidLineage)?;
    let mut messages = vec![project_summary_message(&summary)?];
    let refs = summary
        .artifact_refs
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let coverage = MessageCoverage::closed(summary.source_seq_start, summary.source_seq_end);
    let mut lineage = vec![MessageLineage::derived(coverage, false, refs)];
    if let Some(index) = selection.current_request_anchor_index() {
        let anchor = selection
            .current_request_anchor()
            .ok_or(CompactionError::InvalidLineage)?;
        if session.messages().get(index) != Some(anchor) {
            return Err(CompactionError::InvalidLineage);
        }
        messages.push(anchor.clone());
        lineage.push(
            session
                .message_lineage()
                .get(index)
                .cloned()
                .ok_or(CompactionError::InvalidLineage)?,
        );
    }
    messages.extend(flatten_groups(selection.retain_verbatim()));
    lineage.extend(tail_lineage.iter().cloned());
    Ok(CandidateState {
        messages,
        lineage,
        source_start: summary.source_seq_start,
        source_end: summary.source_seq_end,
        summary: Some(ActiveSummary::new(summary, model.to_string(), usage)),
    })
}

fn group_message_starts(groups: &[ProtocolGroup]) -> Vec<usize> {
    let mut next = 0;
    groups
        .iter()
        .map(|group| {
            let start = next;
            next += match group {
                ProtocolGroup::Message(_) => 1,
                ProtocolGroup::ToolExchange { results, .. } => results.len() + 1,
            };
            start
        })
        .collect()
}
