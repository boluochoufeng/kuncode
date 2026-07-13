use std::collections::BTreeSet;

use kuncode_core::completion::Message;
use thiserror::Error;

use super::AgentSession;
use crate::{
    compaction::{
        artifact::{ArtifactSpillOutcome, ArtifactSpillResult},
        protocol::ProtocolGroup,
        selection::CompactionSelection,
        summary::{ContinuitySummary, SummaryError, SummaryRequest},
    },
    session_store::Seq,
};

/// Failure to bind one summary request to current durable session facts.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SummarySourceError {
    /// The artifact pass belongs to another session or journal snapshot.
    #[error("summary source is not bound to the active durable session")]
    SnapshotMismatch,
    /// A selected message has no current-run journal lineage.
    #[error("summary source message {message_index} lacks durable journal provenance")]
    MissingMessageProvenance {
        /// Position in the active context that could not be audited.
        message_index: usize,
    },
    /// Bound facts violate the continuity-summary request contract.
    #[error("summary source failed request validation: {0}")]
    InvalidRequest(#[from] SummaryError),
}

pub(crate) struct SummarySourceBinding {
    existing_summary: Option<ContinuitySummary>,
    source_messages: Vec<Message>,
    source_seq_start: Seq,
    source_seq_end: Seq,
    durable_head: Seq,
    allowed_artifact_refs: BTreeSet<String>,
}

impl SummarySourceBinding {
    pub(crate) const fn existing_summary(&self) -> Option<&ContinuitySummary> {
        self.existing_summary.as_ref()
    }

    pub(crate) fn source_messages(&self) -> &[Message] {
        &self.source_messages
    }

    pub(crate) const fn source_range(&self) -> (Seq, Seq) {
        (self.source_seq_start, self.source_seq_end)
    }

    pub(crate) const fn durable_head(&self) -> Seq {
        self.durable_head
    }

    pub(crate) const fn allowed_artifact_refs(&self) -> &BTreeSet<String> {
        &self.allowed_artifact_refs
    }
}

impl AgentSession {
    /// Issues an opaque request from an audited artifact batch and selection.
    ///
    /// # Errors
    /// Returns [`SummarySourceError`] when session identity, frontier, message
    /// lineage, group boundaries, or summary validation disagree.
    pub fn issue_summary_request(
        &self,
        artifacts: &ArtifactSpillResult,
        selection: &CompactionSelection,
    ) -> Result<SummaryRequest, SummarySourceError> {
        let binding = self.bind_summary_source(artifacts, selection)?;
        Ok(SummaryRequest::from_bound_source(&binding)?)
    }

    fn bind_summary_source(
        &self,
        artifacts: &ArtifactSpillResult,
        selection: &CompactionSelection,
    ) -> Result<SummarySourceBinding, SummarySourceError> {
        let session_id = self
            .session_id
            .as_ref()
            .ok_or(SummarySourceError::SnapshotMismatch)?;
        let source_frontier = self
            .last_durable_seq
            .ok_or(SummarySourceError::SnapshotMismatch)?;
        if !self.is_durable()
            || artifacts.session_id() != session_id
            || artifacts.source_frontier() > artifacts.frontier()
            || source_frontier != artifacts.source_frontier()
        {
            return Err(SummarySourceError::SnapshotMismatch);
        }
        let prefix_groups = selection.summarize().len();
        let retained_groups = selection.retain_verbatim().len();
        if prefix_groups == 0
            || prefix_groups + retained_groups != artifacts.groups().len()
            || selection.summarize() != &artifacts.groups()[..prefix_groups]
            || selection.retain_verbatim() != &artifacts.groups()[prefix_groups..]
        {
            return Err(SummarySourceError::SnapshotMismatch);
        }
        let source_messages = flatten(selection.summarize());
        let source_message_count = source_messages.len();
        if flatten(artifacts.groups()).len() != self.messages.len()
            || source_message_count > self.message_journal_seqs.len()
        {
            return Err(SummarySourceError::SnapshotMismatch);
        }
        let mut source_seqs = Vec::with_capacity(source_message_count);
        for (message_index, source) in self.message_journal_seqs[..source_message_count]
            .iter()
            .enumerate()
        {
            source_seqs.push(
                source.ok_or(SummarySourceError::MissingMessageProvenance { message_index })?,
            );
        }
        let source_seq_start = *source_seqs
            .first()
            .ok_or(SummarySourceError::SnapshotMismatch)?;
        let source_seq_end = *source_seqs
            .last()
            .ok_or(SummarySourceError::SnapshotMismatch)?;
        let allowed_artifact_refs = artifacts
            .outcomes()
            .iter()
            .filter_map(|outcome| match outcome {
                ArtifactSpillOutcome::Spilled {
                    location,
                    artifact_id,
                    ..
                } if location.group_index < prefix_groups => Some(artifact_id.clone()),
                ArtifactSpillOutcome::BelowThreshold(_)
                | ArtifactSpillOutcome::Failed { .. }
                | ArtifactSpillOutcome::Spilled { .. } => None,
            })
            .collect();
        Ok(SummarySourceBinding {
            existing_summary: None,
            source_messages,
            source_seq_start,
            source_seq_end,
            durable_head: artifacts.frontier(),
            allowed_artifact_refs,
        })
    }
}

fn flatten(groups: &[ProtocolGroup]) -> Vec<Message> {
    groups
        .iter()
        .flat_map(|group| match group {
            ProtocolGroup::Message(message) => vec![message.clone()],
            ProtocolGroup::ToolExchange { assistant, results } => {
                let mut messages = Vec::with_capacity(results.len() + 1);
                messages.push(assistant.clone());
                messages.extend(results.iter().cloned());
                messages
            }
        })
        .collect()
}
