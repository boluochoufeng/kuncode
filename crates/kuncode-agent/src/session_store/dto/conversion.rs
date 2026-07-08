use kuncode_core::{
    completion::{
        AssistantContent, Message, Reasoning, ReasoningContent, Text, ToolCall, ToolFunction,
        ToolResult, ToolResultContent, UserContent,
    },
    non_empty_vec::NonEmptyVec,
};

use crate::session_store::SessionStoreError;

use super::{
    StoredAssistantContent, StoredMessage, StoredReasoningContent, StoredToolResultContent,
    StoredUserContent,
};

impl StoredMessage {
    pub(super) fn from_message(message: &Message) -> Self {
        match message {
            Message::System { content } => Self::System {
                content: content.clone(),
            },
            Message::User { content } => Self::User {
                content: content
                    .iter()
                    .map(StoredUserContent::from_content)
                    .collect(),
            },
            Message::Assistant { id, content } => Self::Assistant {
                id: id.clone(),
                content: content
                    .iter()
                    .map(StoredAssistantContent::from_content)
                    .collect(),
            },
        }
    }

    pub(super) fn into_message(self) -> Result<Message, SessionStoreError> {
        match self {
            Self::System { content } => Ok(Message::System { content }),
            Self::User { content } => Ok(Message::User {
                content: non_empty_vec(
                    "message.user.content",
                    content
                        .into_iter()
                        .map(StoredUserContent::into_content)
                        .collect::<Result<Vec<_>, _>>()?,
                )?,
            }),
            Self::Assistant { id, content } => Ok(Message::Assistant {
                id,
                content: non_empty_vec(
                    "message.assistant.content",
                    content
                        .into_iter()
                        .map(StoredAssistantContent::into_content)
                        .collect::<Result<Vec<_>, _>>()?,
                )?,
            }),
        }
    }
}

impl StoredUserContent {
    fn from_content(content: &UserContent) -> Self {
        match content {
            UserContent::Text(text) => Self::Text {
                text: text.text_ref().to_string(),
            },
            UserContent::ToolResult(result) => Self::ToolResult {
                id: result.id.clone(),
                call_id: result.call_id.clone(),
                content: result
                    .content
                    .iter()
                    .map(StoredToolResultContent::from_content)
                    .collect(),
            },
        }
    }

    fn into_content(self) -> Result<UserContent, SessionStoreError> {
        match self {
            Self::Text { text } => Ok(UserContent::Text(Text::from(text))),
            Self::ToolResult {
                id,
                call_id,
                content,
            } => Ok(UserContent::ToolResult(ToolResult {
                id,
                call_id,
                content: non_empty_vec(
                    "message.user.tool_result.content",
                    content
                        .into_iter()
                        .map(StoredToolResultContent::into_content)
                        .collect(),
                )?,
            })),
        }
    }
}

impl StoredToolResultContent {
    fn from_content(content: &ToolResultContent) -> Self {
        match content {
            ToolResultContent::Text(text) => Self::Text {
                text: text.text_ref().to_string(),
            },
        }
    }

    fn into_content(self) -> ToolResultContent {
        match self {
            Self::Text { text } => ToolResultContent::Text(Text::from(text)),
        }
    }
}

impl StoredAssistantContent {
    fn from_content(content: &AssistantContent) -> Self {
        match content {
            AssistantContent::Text(text) => Self::Text {
                text: text.text_ref().to_string(),
            },
            AssistantContent::ToolCall(call) => Self::ToolCall {
                id: call.id.clone(),
                call_id: call.call_id.clone(),
                function_name: call.function.name.clone(),
                arguments_json: call.function.arguments.clone(),
                signature: call.signature.clone(),
                additional_params_json: call.additional_params.clone(),
            },
            AssistantContent::Reasoning(reasoning) => Self::Reasoning {
                id: reasoning.id.clone(),
                content: reasoning
                    .content
                    .iter()
                    .map(StoredReasoningContent::from_content)
                    .collect(),
            },
        }
    }

    fn into_content(self) -> Result<AssistantContent, SessionStoreError> {
        match self {
            Self::Text { text } => Ok(AssistantContent::Text(Text::from(text))),
            Self::ToolCall {
                id,
                call_id,
                function_name,
                arguments_json,
                signature,
                additional_params_json,
            } => Ok(AssistantContent::ToolCall(ToolCall {
                id,
                call_id,
                function: ToolFunction {
                    name: function_name,
                    arguments: arguments_json,
                },
                signature,
                additional_params: additional_params_json,
            })),
            Self::Reasoning { id, content } => Ok(AssistantContent::Reasoning(Reasoning {
                id,
                content: content
                    .into_iter()
                    .map(StoredReasoningContent::into_content)
                    .collect(),
            })),
        }
    }
}

impl StoredReasoningContent {
    fn from_content(content: &ReasoningContent) -> Self {
        match content {
            ReasoningContent::Text { text, signature } => Self::Text {
                text: text.clone(),
                signature: signature.clone(),
            },
            ReasoningContent::Encrypted(data) => Self::Encrypted { data: data.clone() },
            ReasoningContent::Redacted { data } => Self::Redacted { data: data.clone() },
            ReasoningContent::Summary(text) => Self::Summary { text: text.clone() },
        }
    }

    fn into_content(self) -> ReasoningContent {
        match self {
            Self::Text { text, signature } => ReasoningContent::Text { text, signature },
            Self::Encrypted { data } => ReasoningContent::Encrypted(data),
            Self::Redacted { data } => ReasoningContent::Redacted { data },
            Self::Summary { text } => ReasoningContent::Summary(text),
        }
    }
}

fn non_empty_vec<T: Clone>(
    field: &'static str,
    values: Vec<T>,
) -> Result<NonEmptyVec<T>, SessionStoreError> {
    NonEmptyVec::try_from(values).map_err(|_| {
        SessionStoreError::InvalidMessagePayload(format!("`{field}` must not be empty"))
    })
}
