use kuncode_core::completion::Message;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::SessionStoreError;

mod conversion;

const SCHEMA_VERSION: u64 = 1;

#[derive(Serialize, Deserialize)]
struct StoredMessagePayload {
    schema_version: u64,
    message: StoredMessage,
}

#[derive(Serialize, Deserialize)]
struct StoredMessagesPayload {
    schema_version: u64,
    messages: Vec<StoredMessage>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
enum StoredMessage {
    System {
        content: String,
    },
    User {
        content: Vec<StoredUserContent>,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        content: Vec<StoredAssistantContent>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredUserContent {
    Text {
        text: String,
    },
    ToolResult {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        content: Vec<StoredToolResultContent>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredToolResultContent {
    Text { text: String },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredAssistantContent {
    Text {
        text: String,
    },
    ToolCall {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        function_name: String,
        arguments_json: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        additional_params_json: Option<Value>,
    },
    Reasoning {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        content: Vec<StoredReasoningContent>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredReasoningContent {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    Encrypted {
        data: String,
    },
    Redacted {
        data: String,
    },
    Summary {
        text: String,
    },
}

pub(crate) fn message_to_value(message: &Message) -> Result<Value, SessionStoreError> {
    Ok(serde_json::to_value(StoredMessagePayload {
        schema_version: SCHEMA_VERSION,
        message: StoredMessage::from_message(message),
    })?)
}

pub(crate) fn message_from_value(value: Value) -> Result<Message, SessionStoreError> {
    let payload: StoredMessagePayload = serde_json::from_value(value)?;
    ensure_version("message", payload.schema_version)?;
    payload.message.into_message()
}

pub(crate) fn messages_to_string(messages: &[Message]) -> Result<String, SessionStoreError> {
    let payload = StoredMessagesPayload {
        schema_version: SCHEMA_VERSION,
        messages: messages.iter().map(StoredMessage::from_message).collect(),
    };
    Ok(serde_json::to_string(&payload)?)
}

pub(crate) fn messages_from_str(input: &str) -> Result<Vec<Message>, SessionStoreError> {
    let payload: StoredMessagesPayload = serde_json::from_str(input)?;
    ensure_version("active_messages", payload.schema_version)?;
    payload
        .messages
        .into_iter()
        .map(StoredMessage::into_message)
        .collect()
}

fn ensure_version(payload: &'static str, version: u64) -> Result<(), SessionStoreError> {
    if version == SCHEMA_VERSION {
        Ok(())
    } else {
        Err(SessionStoreError::UnsupportedPayloadVersion { payload, version })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn message_payload_requires_store_shape() {
        let result = message_from_value(json!({
            "role": "user",
            "content": [{ "type": "text", "text": "raw-core-shape" }]
        }));

        assert!(matches!(result, Err(SessionStoreError::Json(_))));
    }

    #[test]
    fn active_messages_payload_requires_store_shape() {
        let result = messages_from_str(
            r#"[{"role":"user","content":[{"type":"text","text":"raw-core-shape"}]}]"#,
        );

        assert!(matches!(result, Err(SessionStoreError::Json(_))));
    }
}
