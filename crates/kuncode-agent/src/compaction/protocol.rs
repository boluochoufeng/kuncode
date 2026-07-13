//! Protocol-safe grouping for context compaction.

use std::{collections::BTreeMap, ops::Range};

use kuncode_core::completion::{AssistantContent, Message, ToolCall, UserContent};
use thiserror::Error;

/// An indivisible portion of conversation history.
#[derive(Clone, Debug, PartialEq)]
pub enum ProtocolGroup {
    /// A message that does not participate in a tool exchange.
    Message(Message),
    /// One assistant request and every message needed to answer all its calls.
    ToolExchange {
        /// The original assistant message, including non-tool blocks.
        assistant: Message,
        /// Consecutive user-role messages containing the matching results.
        results: Vec<Message>,
    },
}

/// Identifies a user message whose human provenance was established upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HumanMessageIndex(
    /// Position from the runtime's authoritative message provenance.
    pub usize,
);

/// Exact human-authored request retained outside a lossy summary.
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

/// Protocol defects that make lossy compaction unsafe.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    /// An assistant message repeats a primary tool-call identifier.
    #[error("assistant message {assistant_index} repeats tool call `{call_id}`")]
    DuplicateCallId {
        /// Position of the malformed assistant message.
        assistant_index: usize,
        /// Repeated primary identifier.
        call_id: String,
    },
    /// A closed exchange did not receive all expected results.
    #[error("assistant message {assistant_index} is missing results for {call_ids:?}")]
    MissingResults {
        /// Position of the assistant message that opened the exchange.
        assistant_index: usize,
        /// Primary identifiers still awaiting results.
        call_ids: Vec<String>,
    },
    /// A result does not belong to the currently open exchange.
    #[error("message {message_index} has unknown tool result `{result_id}`")]
    UnknownResult {
        /// Position of the malformed result message.
        message_index: usize,
        /// Unknown primary identifier.
        result_id: String,
    },
    /// More than one result answers the same call.
    #[error("message {message_index} repeats tool result `{result_id}`")]
    DuplicateResult {
        /// Position of the repeated result.
        message_index: usize,
        /// Repeated primary identifier.
        result_id: String,
    },
    /// Provider-specific correlation identifiers disagree.
    #[error(
        "message {message_index} result `{result_id}` has call_id `{actual}`, expected `{expected}`"
    )]
    CallIdMismatch {
        /// Position of the malformed result message.
        message_index: usize,
        /// Primary identifier that did match.
        result_id: String,
        /// Provider identifier carried by the assistant call.
        expected: String,
        /// Provider identifier carried by the result.
        actual: String,
    },
    /// A result appears without a preceding assistant tool call.
    #[error("message {message_index} is an orphan tool result")]
    OrphanResult {
        /// Position of the orphan result message.
        message_index: usize,
    },
    /// Runtime provenance points at a message that is not human text.
    #[error("message {message_index} is not a human text message")]
    InvalidHumanMessageIndex {
        /// Invalid position supplied by the provenance owner.
        message_index: usize,
    },
    /// The summary prefix extends beyond the active context.
    #[error("summary boundary {summarized_message_end} exceeds message count {message_count}")]
    InvalidSummaryBoundary {
        /// Exclusive end of the proposed summary prefix.
        summarized_message_end: usize,
        /// Number of active messages available.
        message_count: usize,
    },
}

/// Clones history into complete tool exchanges and ordinary message groups.
///
/// # Errors
///
/// Returns [`ProtocolError`] when a tool exchange is open or malformed.
pub fn group_messages(messages: &[Message]) -> Result<Vec<ProtocolGroup>, ProtocolError> {
    let mut groups = Vec::with_capacity(messages.len());
    let mut index = 0;
    while index < messages.len() {
        match &messages[index] {
            Message::Assistant { content, .. } => {
                let calls = content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::ToolCall(call) => Some(call),
                        AssistantContent::Text(_) | AssistantContent::Reasoning(_) => None,
                    })
                    .collect::<Vec<_>>();
                if calls.is_empty() {
                    groups.push(ProtocolGroup::Message(messages[index].clone()));
                    index += 1;
                } else {
                    let (group, next) = close_tool_exchange(messages, index, &calls)?;
                    groups.push(group);
                    index = next;
                }
            }
            Message::User { content }
                if content
                    .iter()
                    .any(|block| matches!(block, UserContent::ToolResult(_))) =>
            {
                return Err(ProtocolError::OrphanResult {
                    message_index: index,
                });
            }
            Message::System { .. } | Message::User { .. } => {
                groups.push(ProtocolGroup::Message(messages[index].clone()));
                index += 1;
            }
        }
    }
    Ok(groups)
}

fn close_tool_exchange(
    messages: &[Message],
    assistant_index: usize,
    calls: &[&ToolCall],
) -> Result<(ProtocolGroup, usize), ProtocolError> {
    let mut expected = BTreeMap::new();
    for call in calls {
        if expected
            .insert(call.id.as_str(), call.call_id.as_deref())
            .is_some()
        {
            return Err(ProtocolError::DuplicateCallId {
                assistant_index,
                call_id: call.id.clone(),
            });
        }
    }
    let mut pending = expected.clone();
    let mut results = Vec::new();
    let mut index = assistant_index + 1;
    while !pending.is_empty() {
        let Some(Message::User { content }) = messages.get(index) else {
            return Err(missing_results(assistant_index, &pending));
        };
        if !content
            .iter()
            .any(|block| matches!(block, UserContent::ToolResult(_)))
        {
            return Err(missing_results(assistant_index, &pending));
        }
        for result in content.iter().filter_map(|block| match block {
            UserContent::ToolResult(result) => Some(result),
            UserContent::Text(_) => None,
        }) {
            let Some(expected_call_id) = expected.get(result.id.as_str()) else {
                return Err(ProtocolError::UnknownResult {
                    message_index: index,
                    result_id: result.id.clone(),
                });
            };
            if !pending.contains_key(result.id.as_str()) {
                return Err(ProtocolError::DuplicateResult {
                    message_index: index,
                    result_id: result.id.clone(),
                });
            }
            if let (Some(expected), Some(actual)) = (*expected_call_id, result.call_id.as_deref())
                && expected != actual
            {
                return Err(ProtocolError::CallIdMismatch {
                    message_index: index,
                    result_id: result.id.clone(),
                    expected: expected.to_string(),
                    actual: actual.to_string(),
                });
            }
            pending.remove(result.id.as_str());
        }
        results.push(messages[index].clone());
        index += 1;
    }
    Ok((
        ProtocolGroup::ToolExchange {
            assistant: messages[assistant_index].clone(),
            results,
        },
        index,
    ))
}

fn missing_results(
    assistant_index: usize,
    pending: &BTreeMap<&str, Option<&str>>,
) -> ProtocolError {
    ProtocolError::MissingResults {
        assistant_index,
        call_ids: pending.keys().map(|id| (*id).to_string()).collect(),
    }
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
    let mandatory = groups
        .iter()
        .rposition(|group| matches!(group, ProtocolGroup::ToolExchange { .. }))
        .or_else(|| groups.len().checked_sub(1))?;
    let budget_tokens = recent_tail_budget_tokens;
    let mut start = mandatory;
    let mut estimated_tokens = groups[mandatory..]
        .iter()
        .fold(0_u64, |total, group| total.saturating_add(estimate(group)));
    while let Some(previous) = start.checked_sub(1) {
        let previous_tokens = estimate(&groups[previous]);
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
mod tests;
