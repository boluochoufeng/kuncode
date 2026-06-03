//! In-memory conversation state for agent turns.

use kuncode_core::completion::Message;

/// Conversation transcript owned by the caller between agent turns.
#[derive(Clone, Debug, Default)]
pub struct AgentSession {
    messages: Vec<Message>,
}

impl AgentSession {
    /// Creates an empty session.
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a session from an existing transcript.
    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self { messages }
    }

    /// Appends a user turn to the transcript.
    pub fn push_user(&mut self, prompt: impl Into<String>) {
        self.messages.push(Message::user(prompt));
    }

    /// Returns the current transcript.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Consumes the session and returns its transcript.
    pub fn into_messages(self) -> Vec<Message> {
        self.messages
    }

    pub(crate) fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}
