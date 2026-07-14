//! Non-empty protected-tail selection and human-authored request anchoring.

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

/// A non-empty contiguous suffix excluded from every lossy compaction pass.
///
/// The suffix always starts no later than the most recent closed tool exchange,
/// so protocol safety takes precedence over the caller's nominal token budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectedRecentTail {
    /// Returns the half-open range into the grouped history.
    pub group_range: Range<usize>,
    /// Estimator cost of the selected suffix.
    pub estimated_tokens: u64,
    /// Caller-provided budget used to expand, but never shrink, the mandatory suffix.
    pub budget_tokens: u64,
}

/// Selects a protected suffix within `recent_tail_budget_tokens`.
///
/// For non-empty canonical input, the latest closed tool exchange and everything
/// after it form the mandatory suffix. When no exchange exists, the final group
/// is mandatory. That suffix remains whole even when it exceeds the budget;
/// earlier complete groups are included only while the complete suffix fits.
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

#[cfg(test)]
mod tests {
    use kuncode_core::completion::Message;

    use super::super::test_support::{assistant_with_calls, result_message};
    use crate::compaction::protocol::{
        HumanMessageIndex, ProtocolError, ProtocolGroup, current_human_request_anchor,
        group_messages, select_protected_recent_tail,
    };

    #[test]
    fn anchor_copies_latest_authoritative_human_text_only_when_summarized() {
        let messages = vec![
            Message::user("first"),
            Message::assistant("work"),
            Message::user("latest"),
            Message::assistant("answer"),
        ];
        let human = [HumanMessageIndex(0), HumanMessageIndex(2)];

        let anchor = current_human_request_anchor(&messages, &human, 3)
            .expect("indices are valid")
            .expect("latest human message is summarized");

        assert_eq!(anchor.source_message_index, 2);
        assert_eq!(anchor.message, Message::user("latest"));
        assert!(matches!(
            current_human_request_anchor(&messages, &human, 2),
            Ok(None)
        ));
    }

    #[test]
    fn anchor_rejects_index_that_is_not_human_text() {
        let messages = vec![result_message(&[("one", None)], None)];

        let error = current_human_request_anchor(&messages, &[HumanMessageIndex(0)], 1)
            .expect_err("tool result is not human text");

        assert_eq!(
            error,
            ProtocolError::InvalidHumanMessageIndex { message_index: 0 }
        );
    }

    #[test]
    fn protected_tail_keeps_latest_exchange_and_contiguous_suffix() {
        let groups = group_messages(&[
            Message::user("old"),
            assistant_with_calls(&[("one", None)]),
            result_message(&[("one", None)], None),
            Message::assistant("recent"),
        ])
        .expect("fixture is valid");

        let tail = select_protected_recent_tail(&groups, 5, |group| match group {
            ProtocolGroup::Message(_) => 2,
            ProtocolGroup::ToolExchange { .. } => 4,
        })
        .expect("non-empty history has a tail");

        assert_eq!(tail.group_range, 1..3);
        assert_eq!(tail.estimated_tokens, 6);
        assert_eq!(tail.budget_tokens, 5);
    }

    #[test]
    fn protected_tail_uses_last_ordinary_group_without_tools() {
        let groups = group_messages(&[
            Message::user("old"),
            Message::assistant("middle"),
            Message::user("latest"),
        ])
        .expect("ordinary messages are valid");

        let tail =
            select_protected_recent_tail(&groups, 10, |_| 4).expect("non-empty history has a tail");

        assert_eq!(tail.group_range, 1..3);
    }

    #[test]
    fn mandatory_group_may_exceed_recent_tail_budget() {
        let groups = group_messages(&[
            Message::user("old"),
            assistant_with_calls(&[("one", None)]),
            result_message(&[("one", None)], None),
        ])
        .expect("fixture is valid");

        let tail = select_protected_recent_tail(&groups, 1, |group| match group {
            ProtocolGroup::Message(_) => 1,
            ProtocolGroup::ToolExchange { .. } => 9,
        })
        .expect("mandatory exchange is always retained");

        assert_eq!(tail.group_range, 1..2);
        assert_eq!(tail.estimated_tokens, 9);
        assert_eq!(tail.budget_tokens, 1);
    }

    #[test]
    fn protected_tail_respects_non_default_budget_from_caller() {
        let groups = group_messages(&[
            Message::user("old"),
            Message::assistant("middle"),
            Message::user("latest"),
        ])
        .expect("ordinary messages are valid");

        let tail =
            select_protected_recent_tail(&groups, 7, |_| 3).expect("non-empty history has a tail");

        assert_eq!(tail.group_range, 1..3);
        assert_eq!(tail.estimated_tokens, 6);
        assert_eq!(tail.budget_tokens, 7);
    }
}
