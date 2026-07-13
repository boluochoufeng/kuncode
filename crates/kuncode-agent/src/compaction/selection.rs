//! Target-aware selection of a complete summary prefix and verbatim tail.

use kuncode_core::completion::Message;
use thiserror::Error;

use crate::compaction::protocol::{
    HumanMessageIndex, HumanRequestAnchor, ProtectedRecentTail, ProtocolError, ProtocolGroup,
    current_human_request_anchor, flatten_groups, group_messages,
};

/// Exact token limits derived from one validated runtime window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionLimits {
    target_tokens: u64,
    soft_tokens: u64,
}

impl SelectionLimits {
    /// Creates ordered target and soft boundaries.
    ///
    /// # Errors
    /// Returns [`SelectionError::InvalidLimits`] unless `0 < target < soft`.
    pub fn new(target_tokens: u64, soft_tokens: u64) -> Result<Self, SelectionError> {
        if target_tokens == 0 || target_tokens >= soft_tokens {
            return Err(SelectionError::InvalidLimits);
        }
        Ok(Self {
            target_tokens,
            soft_tokens,
        })
    }
}

/// Validity class for one provider-visible candidate count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateLoad {
    /// Optimization target reached; later deterministic passes may stop.
    TargetReached,
    /// Above target but still below the strict soft validity boundary.
    BelowSoft,
    /// At or above soft and invalid without further compaction.
    RequiresCompaction,
}

impl CandidateLoad {
    /// Classifies exact token counts without treating target as a hard gate.
    pub const fn classify(tokens: u64, limits: SelectionLimits) -> Self {
        if tokens <= limits.target_tokens {
            Self::TargetReached
        } else if tokens < limits.soft_tokens {
            Self::BelowSoft
        } else {
            Self::RequiresCompaction
        }
    }
}

/// Protocol-safe split awaiting durable source binding by the orchestrator.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionSelection {
    summarize: Vec<ProtocolGroup>,
    retain_verbatim: Vec<ProtocolGroup>,
    current_request_anchor: Option<HumanRequestAnchor>,
}

impl CompactionSelection {
    /// Returns the contiguous complete prefix delegated to summarization.
    pub fn summarize(&self) -> &[ProtocolGroup] {
        &self.summarize
    }

    /// Returns the contiguous protected suffix retained exactly.
    pub fn retain_verbatim(&self) -> &[ProtocolGroup] {
        &self.retain_verbatim
    }

    /// Returns the exact current human request when it lies in the prefix.
    pub fn current_request_anchor(&self) -> Option<&Message> {
        self.current_request_anchor
            .as_ref()
            .map(|anchor| &anchor.message)
    }

    pub(crate) fn current_request_anchor_index(&self) -> Option<usize> {
        self.current_request_anchor
            .as_ref()
            .map(|anchor| anchor.source_message_index)
    }
}

/// Result of selection after deterministic passes have been remeasured.
#[derive(Clone, Debug, PartialEq)]
pub enum SelectionOutcome {
    /// Existing deterministic candidate can proceed directly to validation.
    DeterministicCandidate {
        /// Final classification of the deterministic candidate.
        load: CandidateLoad,
    },
    /// A non-empty safe prefix requires semantic summarization.
    Summarize(CompactionSelection),
    /// No group boundary can produce a candidate below soft.
    Uncompressible {
        /// Invalid load that cannot be reduced at a safe group boundary.
        load: CandidateLoad,
    },
}

/// Invalid caller-provided selection inputs.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SelectionError {
    /// Target and soft token boundaries are invalid.
    #[error("selection requires ordered non-zero target and soft token limits")]
    InvalidLimits,
    /// Protected tail is not a non-empty suffix of supplied groups.
    #[error("protected recent tail is not a non-empty suffix of selection groups")]
    InvalidProtectedTail,
    /// Candidate groups are not the canonical closure of their messages.
    #[error("selection requires canonical closed protocol groups")]
    InvalidProtocolGroups,
    /// Candidate and authoritative histories do not preserve message positions.
    #[error("selection candidate message count differs from authoritative active context")]
    ActiveMessageCountMismatch,
    /// Candidate projection changed a message inside the protected suffix.
    #[error("selection candidate changed the protected recent tail")]
    ProtectedTailChanged,
    /// Human provenance is invalid for the authoritative active context.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

/// Selects a summary prefix without splitting protocol groups.
///
/// # Errors
/// Returns [`SelectionError`] when protocol groups, the protected suffix,
/// authoritative messages, or human provenance disagree.
pub fn select_prefix_tail(
    groups: &[ProtocolGroup],
    authoritative_messages: &[Message],
    protected: &ProtectedRecentTail,
    human_messages: &[HumanMessageIndex],
    limits: SelectionLimits,
    candidate_tokens: u64,
) -> Result<SelectionOutcome, SelectionError> {
    if groups.is_empty()
        || protected.group_range.end != groups.len()
        || protected.group_range.start >= protected.group_range.end
        || groups
            .iter()
            .rposition(|group| matches!(group, ProtocolGroup::ToolExchange { .. }))
            .is_some_and(|mandatory| protected.group_range.start > mandatory)
    {
        return Err(SelectionError::InvalidProtectedTail);
    }
    let flattened = flatten(groups);
    if group_messages(&flattened).map_or(true, |regrouped| regrouped != groups) {
        return Err(SelectionError::InvalidProtocolGroups);
    }
    if flattened.len() != authoritative_messages.len() {
        return Err(SelectionError::ActiveMessageCountMismatch);
    }
    let prefix_message_end = flatten(&groups[..protected.group_range.start]).len();
    if flattened[prefix_message_end..] != authoritative_messages[prefix_message_end..] {
        return Err(SelectionError::ProtectedTailChanged);
    }
    let load = CandidateLoad::classify(candidate_tokens, limits);
    if load == CandidateLoad::TargetReached {
        return Ok(SelectionOutcome::DeterministicCandidate { load });
    }
    if protected.group_range.start == 0 {
        return Ok(if load == CandidateLoad::BelowSoft {
            SelectionOutcome::DeterministicCandidate { load }
        } else {
            SelectionOutcome::Uncompressible { load }
        });
    }
    let anchor =
        current_human_request_anchor(authoritative_messages, human_messages, prefix_message_end)?;
    Ok(SelectionOutcome::Summarize(CompactionSelection {
        summarize: groups[..protected.group_range.start].to_vec(),
        retain_verbatim: groups[protected.group_range.clone()].to_vec(),
        current_request_anchor: anchor,
    }))
}

fn flatten(groups: &[ProtocolGroup]) -> Vec<Message> {
    flatten_groups(groups)
}

#[cfg(test)]
#[path = "selection/tests.rs"]
mod tests;
