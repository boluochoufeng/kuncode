use super::{
    audit::audit_journal,
    boundary::{ArtifactSpillError, ArtifactSpillInput},
    types::{ArtifactSpillResult, ArtifactStore, ArtifactTokenCounter},
};

mod runtime;

use runtime::{SpillRuntime, group_message_starts};

/// Spills eligible results after durable writes and never mutates `input`.
///
/// # Errors
/// Returns [`ArtifactSpillError`] when durable history cannot be audited or the
/// journal advances while an artifact is being committed.
pub async fn spill_artifacts(
    input: ArtifactSpillInput<'_>,
    store: &dyn ArtifactStore,
    counter: &dyn ArtifactTokenCounter,
) -> Result<ArtifactSpillResult, ArtifactSpillError> {
    let audit = audit_journal(&input, store).await?;
    let group_message_starts = group_message_starts(input.groups);
    let runtime = SpillRuntime::new(
        input.durable.session_id(),
        store,
        counter,
        &audit,
        &group_message_starts,
    );
    let mut pass =
        ArtifactSpillResult::new(input.groups.to_vec(), input.durable.frontier(), Vec::new());
    for group_index in 0..input.protected_start {
        runtime.spill_group(group_index, &mut pass).await?;
    }
    Ok(pass)
}
