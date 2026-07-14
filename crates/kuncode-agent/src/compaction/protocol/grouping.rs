//! Canonical message grouping that makes complete tool exchanges indivisible.
//!
//! Open or malformed exchanges are rejected before lossy passes can observe a
//! partial request/result sequence.

use std::collections::BTreeMap;

use kuncode_core::completion::{AssistantContent, Message, ToolCall, UserContent};
use thiserror::Error;

/// An indivisible portion of conversation history used by lossy passes.
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

/// Reconstructs the provider-visible message order from canonical groups.
pub(crate) fn flatten_groups(groups: &[ProtocolGroup]) -> Vec<Message> {
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

/// Clones history into complete tool exchanges and ordinary message groups.
///
/// Successful output is canonical: flattening and regrouping it preserves the
/// same boundaries, while every tool exchange contains all expected results.
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
