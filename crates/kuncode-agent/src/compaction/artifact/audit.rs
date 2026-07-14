//! Proves that artifact candidates are derived from the active durable journal view.
//!
//! Audit reads the journal head and requested messages from one snapshot so a
//! lossy replacement never borrows authority from observations made at different times.

use std::collections::{BTreeMap, BTreeSet};

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
    let requested = input
        .source_message_seqs
        .iter()
        .flatten()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    // The head and rows must share one observation; separate reads could accept
    // messages that were valid before a concurrent append under a newer head.
    let snapshot = store
        .journal_snapshot(input.durable.session_id(), &requested)
        .await
        .map_err(classify_audit_error)?;
    if snapshot.head() != input.durable.frontier() {
        return Err(stale(input, snapshot.head()));
    }
    let mut journal_messages = BTreeMap::new();
    for entry in snapshot.entries().iter().cloned() {
        if entry.kind == JournalKind::Message.as_str() {
            let seq = entry.seq;
            let message = entry.into_message().map_err(classify_audit_error)?;
            journal_messages.insert(seq, message);
        }
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
        // Missing requested facts fail the same equality check as contradictory
        // facts instead of being interpreted as permission to spill.
        if journal_messages.get(source_seq) != Some(active) {
            return Err(ArtifactSpillError::JournalMessageMismatch { index });
        }
    }
    Ok(JournalAudit {
        source_message_seqs: input.source_message_seqs.clone(),
        source_message_retentions: input.source_message_retentions.clone(),
    })
}

fn classify_audit_error(error: crate::session_store::SessionStoreError) -> ArtifactSpillError {
    // Integrity failures destroy the proof that live lineage names durable facts;
    // transient read failures merely make this pass unavailable for retry.
    if error.invalidates_journal_read_authority() {
        ArtifactSpillError::JournalIntegrity(error.to_string())
    } else {
        ArtifactSpillError::JournalAudit(error.to_string())
    }
}

fn stale(input: &ArtifactSpillInput<'_>, actual: Seq) -> ArtifactSpillError {
    ArtifactSpillError::JournalFrontierStale {
        frontier: input.durable.frontier().get(),
        actual: actual.get(),
    }
}
