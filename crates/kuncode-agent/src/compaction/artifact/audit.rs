use super::{
    boundary::{ArtifactSpillError, ArtifactSpillInput},
    types::ArtifactStore,
};
use crate::session_store::{JournalKind, Seq};

pub(super) async fn audit_journal(
    input: &ArtifactSpillInput<'_>,
    store: &dyn ArtifactStore,
) -> Result<(), ArtifactSpillError> {
    let checkpoint = store
        .latest_checkpoint(input.durable.session_id())
        .await
        .map_err(|error| ArtifactSpillError::JournalAudit(error.to_string()))?;
    let replay_after = checkpoint
        .as_ref()
        .map_or(Seq::ZERO, |checkpoint| checkpoint.covers_through_seq);
    if replay_after > input.durable.frontier() {
        return Err(stale(input, replay_after));
    }
    let mut durable_messages = checkpoint.map_or_else(Vec::new, |value| value.active_messages);
    let entries = store
        .replay(input.durable.session_id(), replay_after)
        .await
        .map_err(|error| ArtifactSpillError::JournalAudit(error.to_string()))?;
    let mut observed_head = replay_after;
    for entry in entries {
        if entry.seq > input.durable.frontier() {
            return Err(stale(input, entry.seq));
        }
        observed_head = entry.seq;
        if entry.kind == JournalKind::Message.as_str() {
            durable_messages.push(
                entry
                    .into_message()
                    .map_err(|error| ArtifactSpillError::JournalAudit(error.to_string()))?,
            );
        }
    }
    if observed_head != input.durable.frontier() {
        return Err(stale(input, observed_head));
    }
    if durable_messages.len() != input.messages.len() {
        return Err(ArtifactSpillError::JournalMessageCountMismatch {
            active: input.messages.len(),
            durable: durable_messages.len(),
        });
    }
    if let Some(index) = input
        .messages
        .iter()
        .zip(&durable_messages)
        .position(|(active, durable)| active != durable)
    {
        return Err(ArtifactSpillError::JournalMessageMismatch { index });
    }
    Ok(())
}

fn stale(input: &ArtifactSpillInput<'_>, actual: Seq) -> ArtifactSpillError {
    ArtifactSpillError::JournalFrontierStale {
        frontier: input.durable.frontier().get(),
        actual: actual.get(),
    }
}
