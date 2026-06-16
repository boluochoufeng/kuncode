//! In-memory conversation state for agent turns.

use kuncode_core::completion::Message;

use crate::permission::{PermissionMode, PermissionSessionState};

/// Conversation transcript owned by the caller between agent turns.
///
/// Besides the message history it carries the mutable
/// [`PermissionSessionState`]: keeping per-session grants and mode here — rather
/// than on the shared, `&self` runner — gives per-session isolation with no lock.
#[derive(Clone, Debug, Default)]
pub struct AgentSession {
    messages: Vec<Message>,
    permissions: PermissionSessionState,
    /// Monotonic counter handing out [`AgentEvent`](crate::observer::AgentEvent)
    /// sequence numbers. Lives on the session, not the `&self`/`Clone` runner,
    /// because ordering is per-session — same reasoning as the per-session
    /// permission grants above.
    seq: u64,
}

impl AgentSession {
    /// Creates an empty session in the default permission mode.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty session starting in `mode`.
    pub fn with_mode(mode: PermissionMode) -> Self {
        Self {
            messages: Vec::new(),
            permissions: PermissionSessionState::new(mode),
            seq: 0,
        }
    }

    /// Starts a session from an existing transcript in the default mode.
    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self {
            messages,
            permissions: PermissionSessionState::default(),
            seq: 0,
        }
    }

    /// The session's permission state (mode + grants).
    pub fn permissions(&self) -> &PermissionSessionState {
        &self.permissions
    }

    /// Mutable access to the permission state, for recording grants and mode
    /// changes during a turn.
    pub fn permissions_mut(&mut self) -> &mut PermissionSessionState {
        &mut self.permissions
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

    /// Hands out the next event sequence number, then advances. Starts at `0`.
    pub(crate) fn next_seq(&mut self) -> u64 {
        let seq = self.seq;
        self.seq += 1;
        seq
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}
