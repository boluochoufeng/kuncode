//! Binds semantic summaries to audited durable session provenance.
//!
//! Summary input is authorized from harness lineage, never reconstructed from
//! provider-visible roles or from marker-shaped message content.

use std::collections::BTreeSet;

use kuncode_core::completion::Message;
use thiserror::Error;

use super::AgentSession;
use crate::{
    compaction::{
        artifact::{ArtifactSpillOutcome, ArtifactSpillResult},
        protocol::{ProtocolGroup, flatten_groups},
        selection::CompactionSelection,
        slimming::ToolResultSlimmingResult,
        summary::{ContinuitySummary, SummaryError, SummaryRequest, project_summary_message},
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

/// Audited facts that authorize one continuity-summary request.
///
/// The source range comes from the selected prefix's active lineage. The durable
/// head may be newer because committed artifact receipts append journal facts;
/// the allowlist combines those prefix artifacts with references already carried
/// by its lineage. `source_messages` omits a leading projection of the existing
/// summary when present because that summary is supplied separately for recursive
/// continuity.
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
        let binding = self.bind_summary_source(artifacts, artifacts.groups(), selection)?;
        Ok(SummaryRequest::from_bound_source(&binding)?)
    }

    pub(crate) fn issue_slimmed_summary_request(
        &self,
        slimmed: &ToolResultSlimmingResult<'_>,
        selection: &CompactionSelection,
    ) -> Result<SummaryRequest, SummarySourceError> {
        let binding = self.bind_summary_source(slimmed.source(), slimmed.groups(), selection)?;
        Ok(SummaryRequest::from_bound_source(&binding)?)
    }

    fn bind_summary_source(
        &self,
        artifacts: &ArtifactSpillResult,
        candidate_groups: &[ProtocolGroup],
        selection: &CompactionSelection,
    ) -> Result<SummarySourceBinding, SummarySourceError> {
        let session_id = self
            .session_id
            .as_ref()
            .ok_or(SummarySourceError::SnapshotMismatch)?;
        let source_frontier = self
            .last_durable_seq
            .ok_or(SummarySourceError::SnapshotMismatch)?;
        // Reject artifacts from another session or a frontier that no longer
        // matches the active lineage before exposing any source to the model.
        if !self.is_durable()
            || artifacts.session_id() != session_id
            || artifacts.source_frontier() > artifacts.frontier()
            || (source_frontier != artifacts.source_frontier()
                && source_frontier != artifacts.frontier())
        {
            return Err(SummarySourceError::SnapshotMismatch);
        }
        let prefix_groups = selection.summarize().len();
        let retained_groups = selection.retain_verbatim().len();
        if prefix_groups == 0
            || prefix_groups + retained_groups != candidate_groups.len()
            || selection.summarize() != &candidate_groups[..prefix_groups]
            || selection.retain_verbatim() != &candidate_groups[prefix_groups..]
        {
            return Err(SummarySourceError::SnapshotMismatch);
        }
        let mut source_messages = flatten_groups(selection.summarize());
        let source_message_count = source_messages.len();
        if flatten_groups(artifacts.groups()).len() != self.messages.len()
            || self.messages.len() != self.message_lineage.len()
            || source_message_count > self.message_lineage.len()
        {
            return Err(SummarySourceError::SnapshotMismatch);
        }
        let mut source_seq_start = None;
        let mut source_seq_end = None;
        let mut allowed_artifact_refs = BTreeSet::new();
        // Only the selected prefix contributes durable coverage and authorized
        // artifact references; retained recent groups remain verbatim.
        for (message_index, lineage) in self.message_lineage[..source_message_count]
            .iter()
            .enumerate()
        {
            let coverage = lineage
                .coverage()
                .ok_or(SummarySourceError::MissingMessageProvenance { message_index })?;
            source_seq_start = Some(source_seq_start.map_or(coverage.start(), |current: Seq| {
                current.min(coverage.start())
            }));
            source_seq_end = Some(
                source_seq_end.map_or(coverage.end(), |current: Seq| current.max(coverage.end())),
            );
            allowed_artifact_refs.extend(lineage.artifact_refs().iter().cloned());
        }
        allowed_artifact_refs.extend(artifacts.outcomes().iter().filter_map(
            |outcome| match outcome {
                ArtifactSpillOutcome::Spilled {
                    location,
                    artifact_id,
                    ..
                } if location.group_index < prefix_groups => Some(artifact_id.clone()),
                ArtifactSpillOutcome::BelowThreshold(_)
                | ArtifactSpillOutcome::Failed { .. }
                | ArtifactSpillOutcome::Spilled { .. } => None,
            },
        ));
        let existing_summary = self
            .active_summary
            .as_ref()
            .map(|active| active.summary().clone());
        if let Some(previous) = existing_summary.as_ref() {
            let projected = project_summary_message(previous)
                .map_err(|error| SummaryError::PromptEncoding(error.to_string()))?;
            if source_messages.first() == Some(&projected) {
                // Recursive summaries receive the previous structured summary
                // separately, so retaining its user-role projection here would
                // duplicate evidence and encourage nested summary text.
                source_messages.remove(0);
            }
        }
        Ok(SummarySourceBinding {
            existing_summary,
            source_messages,
            source_seq_start: source_seq_start.ok_or(SummarySourceError::SnapshotMismatch)?,
            source_seq_end: source_seq_end.ok_or(SummarySourceError::SnapshotMismatch)?,
            durable_head: artifacts.frontier(),
            allowed_artifact_refs,
        })
    }
}
