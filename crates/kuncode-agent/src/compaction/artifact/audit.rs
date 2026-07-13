use std::collections::BTreeMap;

use super::{
    boundary::{ArtifactSpillError, ArtifactSpillInput},
    types::ArtifactStore,
};
use crate::session_store::{JournalKind, Seq};
use crate::tool::ToolResultRetention;

pub(super) struct JournalAudit {
    source_message_seqs: Vec<Option<Seq>>,
    source_message_retentions: Vec<ToolResultRetention>,
}

impl JournalAudit {
    pub(super) fn message_seq(&self, message_index: usize) -> Option<Seq> {
        self.source_message_seqs
            .get(message_index)
            .copied()
            .flatten()
    }

    pub(super) fn message_retention(&self, message_index: usize) -> ToolResultRetention {
        self.source_message_retentions
            .get(message_index)
            .copied()
            .unwrap_or_default()
    }
}

pub(super) async fn audit_journal(
    input: &ArtifactSpillInput<'_>,
    store: &dyn ArtifactStore,
) -> Result<JournalAudit, ArtifactSpillError> {
    let entries = store
        .replay(input.durable.session_id(), Seq::ZERO)
        .await
        .map_err(|error| ArtifactSpillError::JournalAudit(error.to_string()))?;
    let mut observed_head = Seq::ZERO;
    let mut journal_messages = BTreeMap::new();
    for entry in entries {
        if entry.seq > input.durable.frontier() {
            return Err(stale(input, entry.seq));
        }
        observed_head = entry.seq;
        if entry.kind == JournalKind::Message.as_str() {
            let seq = entry.seq;
            let message = entry
                .into_message()
                .map_err(|error| ArtifactSpillError::JournalAudit(error.to_string()))?;
            journal_messages.insert(seq, message);
        }
    }
    if observed_head != input.durable.frontier() {
        return Err(stale(input, observed_head));
    }
    let active_messages = crate::compaction::protocol::flatten_groups(input.groups);
    for (index, (active, source_seq)) in active_messages
        .iter()
        .zip(&input.source_message_seqs)
        .enumerate()
    {
        let Some(source_seq) = source_seq else {
            continue;
        };
        if journal_messages.get(source_seq) != Some(active) {
            return Err(ArtifactSpillError::JournalMessageMismatch { index });
        }
    }
    Ok(JournalAudit {
        source_message_seqs: input.source_message_seqs.clone(),
        source_message_retentions: input.source_message_retentions.clone(),
    })
}

fn stale(input: &ArtifactSpillInput<'_>, actual: Seq) -> ArtifactSpillError {
    ArtifactSpillError::JournalFrontierStale {
        frontier: input.durable.frontier().get(),
        actual: actual.get(),
    }
}
