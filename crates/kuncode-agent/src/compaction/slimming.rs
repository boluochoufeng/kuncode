//! Explicit-policy projection of old tool results into bounded markers.
//!
//! Production authorization is deny-by-default: only old, unprotected results
//! carrying harness-minted retention authority are considered, and marker
//! preparation still requires durable lineage, successful complete output, and
//! a strictly smaller bounded projection.

use std::collections::BTreeSet;

use crate::compaction::{
    artifact::{
        ArtifactResultLocation, ArtifactSpillOutcome, ArtifactSpillResult, ArtifactTokenCounter,
        BelowThresholdArtifact, tool_result_hash,
    },
    protocol::{ProtectedRecentTail, ProtocolGroup, group_messages},
};
use kuncode_core::{
    completion::{Message, ToolResult, UserContent},
    non_empty_vec::NonEmptyVec,
};

mod marker;
mod policy;
mod types;

use marker::{PreparedSlimming, prepare_slimmed_result};
pub(crate) use policy::production_slimming_candidates;
pub use types::{
    SlimmingOutcome, SlimmingRetention, ToolResultSlimmingError, ToolResultSlimmingResult,
};

/// Applies only selected projections authorized by the same artifact pass.
///
/// This lower-level boundary validates location, age, sidecar identity, and
/// artifact disposition. Production callers must obtain `authorized` from the
/// harness-owned retention policy; model-visible tool names or arguments are
/// not authorization. Failed, truncated, uncountable, or non-saving candidates
/// remain verbatim.
///
/// # Errors
/// Returns [`ToolResultSlimmingError`] when protocol groups, protection, or an
/// explicit candidate location is invalid.
pub async fn slim_tool_results<'source>(
    source: &'source ArtifactSpillResult,
    protected: &ProtectedRecentTail,
    authorized: &[ArtifactResultLocation],
    counter: &dyn ArtifactTokenCounter,
) -> Result<ToolResultSlimmingResult<'source>, ToolResultSlimmingError> {
    let groups = source.groups();
    if groups.is_empty()
        || protected.group_range.end != groups.len()
        || protected.group_range.start >= protected.group_range.end
        || groups
            .iter()
            .rposition(|group| matches!(group, ProtocolGroup::ToolExchange { .. }))
            .is_some_and(|mandatory| protected.group_range.start > mandatory)
    {
        return Err(ToolResultSlimmingError::InvalidProtectedTail);
    }
    let flattened = flatten(groups);
    if group_messages(&flattened).map_or(true, |regrouped| regrouped != groups) {
        return Err(ToolResultSlimmingError::InvalidProtocolGroups);
    }
    validate_candidates(source, protected, authorized)?;
    let mut projected = groups.to_vec();
    let mut outcomes = Vec::with_capacity(authorized.len());
    for &location in authorized {
        let authorization = below_threshold(source, location)?;
        let (assistant, result) = source_result(groups, location)
            .ok_or(ToolResultSlimmingError::InvalidCandidate { location })?;
        let Some(original_journal_seq) = authorization.source_journal_seq() else {
            outcomes.push(SlimmingOutcome::Retained {
                location,
                reason: SlimmingRetention::MissingProvenance,
            });
            continue;
        };
        let prepared = prepare_slimmed_result(
            assistant,
            result,
            original_journal_seq,
            authorization.tokens(),
            counter,
        )
        .await;
        match prepared {
            PreparedSlimming::Marker {
                result: marker,
                tokens,
            } => {
                replace_result(&mut projected, location, marker)?;
                outcomes.push(SlimmingOutcome::Slimmed {
                    location,
                    original_journal_seq,
                    original_tokens: authorization.tokens(),
                    slimmed_tokens: tokens,
                });
            }
            PreparedSlimming::Retain(reason) => {
                outcomes.push(SlimmingOutcome::Retained { location, reason })
            }
        }
    }
    Ok(ToolResultSlimmingResult {
        source,
        groups: projected,
        outcomes,
    })
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

fn validate_candidates(
    source: &ArtifactSpillResult,
    protected: &ProtectedRecentTail,
    authorized: &[ArtifactResultLocation],
) -> Result<(), ToolResultSlimmingError> {
    let mut locations = BTreeSet::new();
    for &location in authorized {
        if location.group_index >= protected.group_range.start {
            return Err(ToolResultSlimmingError::ProtectedCandidate {
                group_index: location.group_index,
            });
        }
        if !locations.insert(location) {
            return Err(ToolResultSlimmingError::DuplicateCandidate { location });
        }
        let Some((_, result)) = source_result(source.groups(), location) else {
            return Err(ToolResultSlimmingError::InvalidCandidate { location });
        };
        let authorization = below_threshold(source, location)?;
        let source_hash = tool_result_hash(result)
            .map_err(|_| ToolResultSlimmingError::ArtifactSidecarMismatch { location })?;
        if result.id != authorization.tool_call_id() || source_hash != authorization.source_hash() {
            return Err(ToolResultSlimmingError::ArtifactSidecarMismatch { location });
        }
    }
    Ok(())
}

fn below_threshold(
    source: &ArtifactSpillResult,
    location: ArtifactResultLocation,
) -> Result<&BelowThresholdArtifact, ToolResultSlimmingError> {
    let outcome = source
        .outcomes()
        .iter()
        .find(|outcome| outcome.location() == location)
        .ok_or(ToolResultSlimmingError::InvalidCandidate { location })?;
    match outcome {
        ArtifactSpillOutcome::BelowThreshold(authorization) => Ok(authorization),
        ArtifactSpillOutcome::Failed { .. } | ArtifactSpillOutcome::Spilled { .. } => {
            Err(ToolResultSlimmingError::IneligibleArtifactDisposition)
        }
    }
}

fn source_result(
    groups: &[ProtocolGroup],
    location: ArtifactResultLocation,
) -> Option<(&Message, &ToolResult)> {
    let ProtocolGroup::ToolExchange { assistant, results } = groups.get(location.group_index)?
    else {
        return None;
    };
    let Message::User { content } = results.get(location.result_message_index)? else {
        return None;
    };
    let UserContent::ToolResult(result) = content.iter().nth(location.content_index)? else {
        return None;
    };
    Some((assistant, result))
}

fn replace_result(
    groups: &mut [ProtocolGroup],
    location: ArtifactResultLocation,
    marker: ToolResult,
) -> Result<(), ToolResultSlimmingError> {
    let Some(ProtocolGroup::ToolExchange { results, .. }) = groups.get_mut(location.group_index)
    else {
        return Err(ToolResultSlimmingError::InvalidCandidate { location });
    };
    let Some(Message::User { content }) = results.get_mut(location.result_message_index) else {
        return Err(ToolResultSlimmingError::InvalidCandidate { location });
    };
    let mut blocks = content.clone().into_vec();
    let Some(UserContent::ToolResult(result)) = blocks.get_mut(location.content_index) else {
        return Err(ToolResultSlimmingError::InvalidCandidate { location });
    };
    *result = marker;
    let Some((first, rest)) = blocks.split_first() else {
        return Err(ToolResultSlimmingError::InvalidCandidate { location });
    };
    *content = NonEmptyVec::from_first_rest(first.clone(), rest.to_vec());
    Ok(())
}

#[cfg(test)]
mod tests;
