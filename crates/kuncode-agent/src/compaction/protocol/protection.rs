//! Protected-tail selection and human-authored request anchoring.

use std::ops::Range;

use kuncode_core::completion::{Message, UserContent};

use super::grouping::{ProtocolError, ProtocolGroup};

/// Identifies a user message whose human provenance was established upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HumanMessageIndex(
    /// Position from the runtime's authoritative message provenance.
    pub usize,
);

/// Exact human-authored request retained outside a lossy summary.
#[derive(Clone, Debug, PartialEq)]
pub struct HumanRequestAnchor {
    /// Returns the source position in the uncompacted active context.
    pub source_message_index: usize,
    /// Verbatim cloned user message.
    pub message: Message,
}

/// A contiguous suffix excluded from every lossy compaction pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectedRecentTail {
    /// Returns the half-open range into the grouped history.
    pub group_range: Range<usize>,
    /// Estimator cost of the selected suffix.
    pub estimated_tokens: u64,
    /// Caller-provided budget used to expand the mandatory suffix.
    pub budget_tokens: u64,
}

/// Selects a protected suffix within `recent_tail_budget_tokens`.
///
/// The latest closed tool exchange, or final ordinary group when none exists,
/// is mandatory and remains whole even when its contiguous suffix exceeds the budget.
pub fn select_protected_recent_tail(
    groups: &[ProtocolGroup],
    recent_tail_budget_tokens: u64,
    mut estimate: impl FnMut(&ProtocolGroup) -> u64,
) -> Option<ProtectedRecentTail> {
    let estimates = groups.iter().map(&mut estimate).collect::<Vec<_>>();
    select_protected_recent_tail_from_estimates(groups, recent_tail_budget_tokens, &estimates)
}

pub(crate) fn select_protected_recent_tail_from_estimates(
    groups: &[ProtocolGroup],
    recent_tail_budget_tokens: u64,
    estimates: &[u64],
) -> Option<ProtectedRecentTail> {
    if groups.len() != estimates.len() {
        return None;
    }
    let mandatory = groups
        .iter()
        .rposition(|group| matches!(group, ProtocolGroup::ToolExchange { .. }))
        .or_else(|| groups.len().checked_sub(1))?;
    let budget_tokens = recent_tail_budget_tokens;
    let mut start = mandatory;
    let mut estimated_tokens = estimates[mandatory..]
        .iter()
        .fold(0_u64, |total, tokens| total.saturating_add(*tokens));
    while let Some(previous) = start.checked_sub(1) {
        let previous_tokens = estimates[previous];
        if estimated_tokens.saturating_add(previous_tokens) > budget_tokens {
            break;
        }
        start = previous;
        estimated_tokens = estimated_tokens.saturating_add(previous_tokens);
    }
    Some(ProtectedRecentTail {
        group_range: start..groups.len(),
        estimated_tokens,
        budget_tokens,
    })
}

/// Copies the latest authoritative human request when it lies in the summary prefix.
///
/// # Errors
///
/// Returns [`ProtocolError`] for an invalid prefix or provenance index.
pub fn current_human_request_anchor(
    messages: &[Message],
    human_messages: &[HumanMessageIndex],
    summarized_message_end: usize,
) -> Result<Option<HumanRequestAnchor>, ProtocolError> {
    if summarized_message_end > messages.len() {
        return Err(ProtocolError::InvalidSummaryBoundary {
            summarized_message_end,
            message_count: messages.len(),
        });
    }
    let mut latest = None;
    for HumanMessageIndex(index) in human_messages {
        let valid = matches!(
            messages.get(*index),
            Some(Message::User { content })
                if content.iter().all(|block| matches!(block, UserContent::Text(_)))
        );
        if !valid {
            return Err(ProtocolError::InvalidHumanMessageIndex {
                message_index: *index,
            });
        }
        latest = Some(latest.map_or(*index, |current: usize| current.max(*index)));
    }
    let Some(source_message_index) = latest.filter(|index| *index < summarized_message_end) else {
        return Ok(None);
    };
    Ok(Some(HumanRequestAnchor {
        source_message_index,
        message: messages[source_message_index].clone(),
    }))
}
