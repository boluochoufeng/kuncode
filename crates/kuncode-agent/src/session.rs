//! In-memory conversation state for agent turns.

use kuncode_core::completion::Message;

use crate::permission::{PermissionMode, PermissionSessionState};
use crate::session_store::SessionId;
use crate::todo::{TodoHandle, TodoItem};

/// Active conversation context owned by the caller between agent turns.
///
/// Besides the message history it carries the mutable
/// [`PermissionSessionState`]: keeping per-session grants and mode here — rather
/// than on the shared, `&self` runner — gives per-session isolation with no lock.
#[derive(Debug, Default)]
pub struct AgentSession {
    messages: Vec<Message>,
    permissions: PermissionSessionState,
    /// Monotonic counter handing out [`AgentEvent`](crate::observer::AgentEvent)
    /// sequence numbers. Lives on the session, not the `&self`/`Clone` runner,
    /// because ordering is per-session — same reasoning as the per-session
    /// permission grants above.
    seq: u64,
    /// The session task plan. Same per-session rationale as the permission
    /// state. The runner clones this handle into each
    /// [`ToolContext`](crate::tool::ToolContext) so `todo_write` writes here.
    todos: TodoHandle,
    session_id: Option<SessionId>,
    persistence_error: Option<String>,
    non_durable: bool,
}

impl Clone for AgentSession {
    /// Hand-written rather than derived: a derived clone would share the
    /// [`TodoHandle`]'s `Arc`, so two sessions would write the same plan.
    /// Deep-cloning the plan keeps per-session isolation — the same by-value
    /// isolation the permission state gets for free.
    ///
    /// The persisted session id is dropped, not shared: a clone represents a
    /// separate timeline unless a caller explicitly attaches a new id.
    fn clone(&self) -> Self {
        Self {
            messages: self.messages.clone(),
            permissions: self.permissions.clone(),
            seq: self.seq,
            todos: self.todos.deep_clone(),
            session_id: None,
            persistence_error: None,
            non_durable: false,
        }
    }
}

impl AgentSession {
    /// Creates an empty session in the default permission mode.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty session starting in `mode`.
    pub fn with_mode(mode: PermissionMode) -> Self {
        Self {
            permissions: PermissionSessionState::new(mode),
            ..Self::default()
        }
    }

    /// Starts a session from an existing active context in the default mode.
    ///
    /// The messages become in-memory state only; a caller resuming from
    /// [`SessionStore`](crate::session_store::SessionStore) attaches the
    /// returned session id separately.
    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self {
            messages,
            ..Self::default()
        }
    }

    pub fn attach_session_id(&mut self, id: SessionId) {
        self.session_id = Some(id);
        self.non_durable = false;
    }

    pub fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }

    pub(crate) fn is_durable(&self) -> bool {
        self.session_id.is_some() && !self.non_durable
    }

    pub fn mark_persistence_failed(&mut self, reason: impl Into<String>) {
        self.non_durable = true;
        if self.persistence_error.is_none() {
            self.persistence_error = Some(format!("session persistence failed: {}", reason.into()));
        }
    }

    /// Hands out a session-persistence failure once (take-and-clear), so
    /// the runner can surface exactly one warning per failure.
    pub(crate) fn take_persistence_error(&mut self) -> Option<String> {
        self.persistence_error.take()
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

    /// A shared clone of the session plan handle, for the runner to place on a
    /// [`ToolContext`](crate::tool::ToolContext). Shares the same plan, so a
    /// `todo_write` call writes back to this session.
    pub fn todo_handle(&self) -> TodoHandle {
        self.todos.clone()
    }

    /// The plan's write counter, for the runner to detect a change across a tool
    /// call without recognizing `todo_write` by name.
    pub fn todo_generation(&self) -> u64 {
        self.todos.generation()
    }

    /// A snapshot of the current plan items, for emitting in a
    /// [`TodoUpdate`](crate::observer::EventKind::TodoUpdate) event.
    pub fn todos_snapshot(&self) -> Vec<TodoItem> {
        self.todos.snapshot()
    }

    /// Appends a user turn to the in-memory active context.
    pub fn push_user(&mut self, prompt: impl Into<String>) {
        self.push(Message::user(prompt));
    }

    /// Returns the current active context.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Consumes the session and returns its active context.
    pub fn into_messages(self) -> Vec<Message> {
        self.messages
    }

    /// The single in-memory append chokepoint.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_user_routes_through_push() {
        let mut session = AgentSession::new();

        session.push_user("hello");

        assert_eq!(session.messages().len(), 1);
    }

    #[test]
    fn clone_does_not_carry_session_id() {
        let mut session = AgentSession::new();
        session.attach_session_id(SessionId::new("session-a"));
        session.push_user("one");

        let mut cloned = session.clone();
        cloned.push_user("two");
        assert!(cloned.session_id().is_none());
        assert_eq!(
            session.session_id().map(SessionId::as_str),
            Some("session-a")
        );
    }

    #[test]
    fn unattached_session_writes_nothing() {
        let mut session = AgentSession::new();
        session.push_user("hello");
        assert_eq!(session.messages().len(), 1);
        assert!(session.take_persistence_error().is_none());
    }

    #[test]
    fn persistence_failure_is_reported_once() {
        let mut session = AgentSession::new();

        session.mark_persistence_failed("disk on fire");

        assert_eq!(
            session.take_persistence_error(),
            Some("session persistence failed: disk on fire".to_string())
        );
        assert!(session.take_persistence_error().is_none());
        assert!(!session.is_durable());
    }
}
