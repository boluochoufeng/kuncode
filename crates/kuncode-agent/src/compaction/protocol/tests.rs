use kuncode_core::{
    completion::{AssistantContent, Message, ToolResult, ToolResultContent, UserContent},
    non_empty_vec::NonEmptyVec,
};

use super::ProtocolGroup;

mod boundaries;
mod grouping;

pub(super) fn assistant_with_calls(calls: &[(&str, Option<&str>)]) -> Message {
    let blocks = calls
        .iter()
        .map(|(id, call_id)| match call_id {
            Some(call_id) => AssistantContent::tool_call_with_call_id(
                *id,
                *call_id,
                "bash",
                serde_json::json!({}),
            ),
            None => AssistantContent::tool_call(*id, "bash", serde_json::json!({})),
        })
        .collect::<Vec<_>>();
    Message::Assistant {
        id: None,
        content: NonEmptyVec::from_first_rest(AssistantContent::text("working"), blocks),
    }
}

pub(super) fn result_message(results: &[(&str, Option<&str>)], text: Option<&str>) -> Message {
    let mut blocks = results
        .iter()
        .map(|(id, call_id)| {
            UserContent::ToolResult(ToolResult {
                id: (*id).to_string(),
                call_id: call_id.map(str::to_string),
                content: NonEmptyVec::new(ToolResultContent::text("ok")),
            })
        })
        .collect::<Vec<_>>();
    if let Some(text) = text {
        blocks.push(UserContent::text(text));
    }
    let first = blocks.remove(0);
    Message::User {
        content: NonEmptyVec::from_first_rest(first, blocks),
    }
}

pub(super) fn flatten(groups: &[ProtocolGroup]) -> Vec<Message> {
    groups
        .iter()
        .flat_map(|group| match group {
            ProtocolGroup::Message(message) => vec![message.clone()],
            ProtocolGroup::ToolExchange { assistant, results } => {
                let mut messages = vec![assistant.clone()];
                messages.extend(results.iter().cloned());
                messages
            }
        })
        .collect()
}
