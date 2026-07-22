//! Chat message types exchanged with a completion model.
//!
//! Messages are tagged by `role` on the wire so they can be deserialized from
//! the JSON shape used by the major LLM providers.

use serde::{Deserialize, Serialize};

use crate::non_empty_vec::NonEmptyVec;

/// A single turn in a chat conversation.
///
/// The variant determines the speaker (`system`, `user`, or `assistant`).
/// User and assistant turns carry a non-empty list of content blocks so that
/// multimodal or tool-augmented messages can be represented uniformly.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    /// System prompt that sets instructions or persona for the model.
    System {
        /// System instructions sent before user turns.
        content: String,
    },
    /// Input from the human (or a tool result acting on the user's behalf).
    User {
        /// User-role content blocks, guaranteed non-empty.
        content: NonEmptyVec<UserContent>,
    },
    /// Output produced by the model.
    Assistant {
        /// Provider-assigned identifier for the assistant message, when
        /// returned. Used to correlate streaming chunks or follow-up edits.
        id: Option<String>,
        /// Assistant-role content blocks, guaranteed non-empty.
        content: NonEmptyVec<AssistantContent>,
    },
}

impl Message {
    /// Convenience constructor for a system prompt.
    pub fn system(text: impl Into<String>) -> Self {
        Self::System {
            content: text.into(),
        }
    }

    /// Convenience constructor for a plain-text user message.
    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            content: NonEmptyVec::new(UserContent::text(text)),
        }
    }

    /// Convenience constructor for a plain-text assistant message.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::Assistant {
            id: None,
            content: NonEmptyVec::new(AssistantContent::text(text.into())),
        }
    }

    /// Builds a user-role message carrying the result of a previously
    /// requested tool call.
    ///
    /// `id` identifies the originating tool call so the model can match the
    /// result back to its request.
    pub fn tool_result(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::User {
            content: NonEmptyVec::new(UserContent::ToolResult(ToolResult {
                id: id.into(),
                call_id: None,
                content: NonEmptyVec::new(ToolResultContent::text(content)),
            })),
        }
    }
}

/// Content blocks that may appear inside a user-role message.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum UserContent {
    /// Plain user-authored text.
    Text(Text),
    /// Result from a tool call requested by the assistant.
    ToolResult(ToolResult),
}

impl UserContent {
    /// Wraps a string as a [`UserContent::Text`] block.
    pub fn text(text: impl Into<String>) -> Self {
        UserContent::Text(text.into().into())
    }
}

/// The outcome of a tool invocation, fed back to the model as user content.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolResult {
    /// Identifier of the [`ToolCall`] being responded to.
    pub id: String,
    /// Optional provider-specific call identifier; serialized only when set
    /// because not every provider distinguishes `id` from `call_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// Result payload, guaranteed to contain at least one content block.
    pub content: NonEmptyVec<ToolResultContent>,
}

/// Content blocks allowed inside a [`ToolResult`].
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolResultContent {
    /// Textual tool output.
    Text(Text),
}

impl ToolResultContent {
    /// Wraps a string as a textual tool result.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into().into())
    }
}

/// Constraint on which tools the model is allowed (or required) to invoke.
#[derive(Default, Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// The model decides whether to call a tool.
    #[default]
    Auto,
    /// Tool calling is disabled for this request.
    None,
    /// The model must call at least one tool.
    Required,
    /// The model must call the named tool.
    Specific {
        /// Function name that must be called.
        function_name: String,
    },
}

/// Content blocks that may appear inside an assistant-role message.
///
/// Serialized untagged because different providers omit the discriminator and
/// rely on field shape to distinguish the variants.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum AssistantContent {
    /// Visible assistant answer text.
    Text(Text),
    /// Tool call request emitted by the assistant.
    ToolCall(ToolCall),
    /// Reasoning/thinking content returned by reasoning-capable models.
    Reasoning(Reasoning),
    /// Safety refusal returned instead of ordinary assistant text.
    Refusal(Refusal),
}

impl AssistantContent {
    /// Wraps a string as an [`AssistantContent::Text`] block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into().into())
    }

    /// Builds a tool-call block where the provider does not distinguish a
    /// separate `call_id`.
    pub fn tool_call(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self::ToolCall(ToolCall::new(
            id.into(),
            ToolFunction {
                name: name.into(),
                arguments,
            },
        ))
    }

    /// Builds a tool-call block carrying both an `id` and a provider-specific
    /// `call_id` (e.g. OpenAI-style responses where the two differ).
    pub fn tool_call_with_call_id(
        id: impl Into<String>,
        call_id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self::ToolCall(
            ToolCall::new(
                id.into(),
                ToolFunction {
                    name: name.into(),
                    arguments,
                },
            )
            .with_call_id(call_id.into()),
        )
    }

    /// Wraps a chain-of-thought string as a [`Reasoning`] block.
    pub fn reasoning(reasoning: impl AsRef<str>) -> Self {
        Self::Reasoning(Reasoning::new(reasoning.as_ref()))
    }

    /// Preserves a provider refusal separately from ordinary assistant text.
    pub fn refusal(refusal: impl Into<String>) -> Self {
        Self::Refusal(Refusal {
            refusal: refusal.into(),
        })
    }
}

/// A safety refusal emitted instead of ordinary assistant content.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Refusal {
    refusal: String,
}

impl Refusal {
    /// Returns the refusal text.
    pub fn text_ref(&self) -> &str {
        &self.refusal
    }
}

/// A plain-text content block.
///
/// A named field rather than a newtype around `String`: the
/// internally-tagged enums that carry it ([`UserContent`],
/// [`ToolResultContent`]) must embed their `type` tag *into* the block, which
/// serde cannot do when the content serializes to a bare string — a derived
/// newtype here makes those variants fail at runtime on every
/// serialize/deserialize. Callers persist and reload messages through these
/// derives, so every variant must actually round-trip; the representation is
/// `{"type":"text","text":"…"}`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Text {
    text: String,
}

impl Text {
    /// Returns the wrapped text as a string slice.
    pub fn text_ref(&self) -> &str {
        &self.text
    }

    /// Consumes the block and returns the wrapped text.
    pub fn text(self) -> String {
        self.text
    }
}

impl<T> From<T> for Text
where
    T: Into<String>,
{
    fn from(value: T) -> Self {
        Text { text: value.into() }
    }
}

/// A request from the model to invoke a tool.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolCall {
    /// Stable identifier used by [`ToolResult::id`] to correlate the reply.
    pub id: String,
    /// Provider-specific secondary identifier, when the API exposes one.
    pub call_id: Option<String>,
    /// Function name and arguments requested by the model.
    pub function: ToolFunction,
    /// Provider-issued signature used to verify the call has not been
    /// tampered with on resubmission.
    pub signature: Option<String>,
    /// Catch-all for provider-specific fields not modeled directly.
    pub additional_params: Option<serde_json::Value>,
}

impl ToolCall {
    /// Creates a tool call with only the required identifier and function.
    pub fn new(id: String, function: ToolFunction) -> Self {
        Self {
            id,
            call_id: None,
            function,
            signature: None,
            additional_params: None,
        }
    }

    /// Returns the call with `call_id` populated; chainable on [`Self::new`].
    pub fn with_call_id(mut self, call_id: String) -> Self {
        self.call_id = Some(call_id);
        self
    }
}

/// The function name and JSON arguments of a [`ToolCall`].
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolFunction {
    /// Function name as declared in the matching tool definition.
    pub name: String,
    /// JSON argument object produced by the model.
    pub arguments: serde_json::Value,
}

impl ToolFunction {
    /// Creates a function call payload.
    pub fn new(name: String, arguments: serde_json::Value) -> Self {
        Self { name, arguments }
    }
}

/// A chain-of-thought / reasoning block emitted by reasoning-capable models.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Reasoning {
    /// Provider-assigned identifier, when supplied.
    pub id: Option<String>,
    /// Reasoning fragments preserved for replay to providers that require it.
    pub content: Vec<ReasoningContent>,
}

impl Reasoning {
    /// Single-block reasoning without a verification signature.
    pub fn new(input: &str) -> Self {
        Self::new_with_signature(input, None)
    }

    /// Single-block reasoning carrying a provider-issued `signature` that
    /// must be replayed verbatim on follow-up requests.
    pub fn new_with_signature(input: &str, signature: Option<String>) -> Self {
        Self {
            id: None,
            content: vec![ReasoningContent::Text {
                text: input.to_string(),
                signature,
            }],
        }
    }

    /// Returns the reasoning with `id` populated; chainable on the
    /// constructors.
    pub fn with_id(mut self, id: String) -> Self {
        self.id = Some(id);
        self
    }

    /// Reasoning whose contents the provider has redacted; the opaque `data`
    /// payload must still be echoed back to preserve continuity.
    pub fn redacted(data: impl Into<String>) -> Self {
        Self {
            id: None,
            content: vec![ReasoningContent::Redacted { data: data.into() }],
        }
    }

    /// Reasoning returned only in encrypted form by the provider.
    pub fn encrypted(data: impl Into<String>) -> Self {
        Self {
            id: None,
            content: vec![ReasoningContent::Encrypted(data.into())],
        }
    }

    /// Reasoning expressed as a list of plain-text summaries (one block per
    /// summary).
    pub fn summaries(input: Vec<String>) -> Self {
        Self {
            id: None,
            content: input.into_iter().map(ReasoningContent::Summary).collect(),
        }
    }

    /// Multi-block textual reasoning without signatures.
    pub fn multi(input: Vec<String>) -> Self {
        Self {
            id: None,
            content: input
                .into_iter()
                .map(|text| ReasoningContent::Text {
                    text,
                    signature: None,
                })
                .collect(),
        }
    }
}

/// Individual entry inside a [`Reasoning`] block.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum ReasoningContent {
    /// Plain reasoning text, optionally accompanied by a provider signature.
    Text {
        /// Plain reasoning text.
        text: String,
        /// Provider signature that must be replayed verbatim when present.
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Opaque encrypted reasoning payload returned by the provider.
    Encrypted(String),
    /// Reasoning the provider has redacted; the `data` blob must still be
    /// preserved and replayed.
    Redacted {
        /// Opaque redacted payload to preserve across turns.
        data: String,
    },
    /// A short summary of a longer hidden reasoning trace.
    Summary(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(message: &Message) -> Message {
        let json = serde_json::to_string(message).expect("message should serialize");
        serde_json::from_str(&json).expect("message should deserialize")
    }

    /// Every shape these types can express must survive serialize →
    /// deserialize unchanged. Callers persist messages through these derives,
    /// and a tagging mismatch (e.g. an internally-tagged variant wrapping a
    /// bare string) fails only at runtime — this test is the compile-time
    /// check serde cannot give.
    #[test]
    fn every_message_shape_roundtrips() {
        let messages = [
            Message::system("be helpful"),
            Message::user("fix the bug"),
            Message::tool_result("call_1", "exit 0"),
            Message::assistant("done"),
            Message::Assistant {
                id: Some("msg_1".into()),
                content: NonEmptyVec::from_first_rest(
                    AssistantContent::text("running"),
                    vec![
                        AssistantContent::tool_call_with_call_id(
                            "call_1",
                            "fc_1",
                            "bash",
                            serde_json::json!({ "cmd": "ls" }),
                        ),
                        AssistantContent::Reasoning(Reasoning::new_with_signature(
                            "the user wants ls",
                            Some("sig".into()),
                        )),
                        AssistantContent::Reasoning(Reasoning::redacted("blob")),
                        AssistantContent::Reasoning(Reasoning::encrypted("cipher")),
                        AssistantContent::Reasoning(
                            Reasoning::summaries(vec!["s1".into(), "s2".into()])
                                .with_id("r_1".to_string()),
                        ),
                        AssistantContent::refusal("cannot comply"),
                    ],
                ),
            },
        ];

        for message in &messages {
            assert_eq!(&roundtrip(message), message);
        }
    }

    /// Pins the on-wire shape of tagged text blocks —
    /// `{"type":"text","text":"…"}` — the exact JSON persisted logs contain.
    #[test]
    fn user_text_embeds_its_type_tag() {
        let json = serde_json::to_value(Message::user("hi")).expect("message should serialize");
        assert_eq!(
            json,
            serde_json::json!({
                "role": "user",
                "content": [{ "type": "text", "text": "hi" }]
            })
        );
    }
}
