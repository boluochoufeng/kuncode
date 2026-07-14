//! Protocol-safe grouping and non-empty suffix protection for context compaction.
//!
//! Lossy passes operate on canonical groups so an assistant tool request and
//! all of its results are retained, moved, or summarized as one closed unit.

mod grouping;
mod protection;

pub(crate) use grouping::flatten_groups;
pub use grouping::{ProtocolError, ProtocolGroup, group_messages};
pub(crate) use protection::select_protected_recent_tail_from_estimates;
pub use protection::{
    HumanMessageIndex, HumanRequestAnchor, ProtectedRecentTail, current_human_request_anchor,
    select_protected_recent_tail,
};

#[cfg(test)]
mod test_support {
    use kuncode_core::{
        completion::{AssistantContent, Message, ToolResult, ToolResultContent, UserContent},
        non_empty_vec::NonEmptyVec,
    };

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
}
