//! In-memory conversation state for agent turns.

use std::path::PathBuf;

use kuncode_core::completion::Message;

use crate::permission::{PermissionMode, PermissionSessionState};
use crate::todo::{TodoHandle, TodoItem};
use crate::transcript::TranscriptLog;

/// Conversation transcript owned by the caller between agent turns.
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
    /// Best-effort append-only disk mirror of the transcript (see
    /// [`transcript`](crate::transcript)). Hooked into [`push`](Self::push) —
    /// the single append chokepoint — so the on-disk log always holds the full
    /// history ahead of any future in-memory compaction. `None` (the library
    /// default) disables persistence: the library never writes a caller's
    /// disk uninvited; the CLI opts in via
    /// [`attach_transcript`](Self::attach_transcript).
    transcript: Option<TranscriptLog>,
    /// Where large tool results may be persisted inside the workspace
    /// (`<workspace>/.kuncode/tool-results`). A pure path value: its consumer
    /// creates the directory on first use, and it stays `Some` even when the
    /// transcript log is poisoned — log health and tool-result persistence
    /// are deliberately decoupled.
    tool_results_dir: Option<PathBuf>,
}

impl Clone for AgentSession {
    /// Hand-written rather than derived: a derived clone would share the
    /// [`TodoHandle`]'s `Arc`, so two sessions would write the same plan.
    /// Deep-cloning the plan keeps per-session isolation — the same by-value
    /// isolation the permission state gets for free.
    ///
    /// The transcript writer is dropped (`None`), not shared: two sessions
    /// appending to one file would interleave two timelines into an
    /// unreadable log. A clone that wants persistence gets its own log
    /// attached explicitly. `tool_results_dir` is a plain path and survives —
    /// tool-result files are content-addressed, so two sessions writing the
    /// same directory only deduplicate.
    fn clone(&self) -> Self {
        Self {
            messages: self.messages.clone(),
            permissions: self.permissions.clone(),
            seq: self.seq,
            todos: self.todos.deep_clone(),
            transcript: None,
            tool_results_dir: self.tool_results_dir.clone(),
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

    /// Starts a session from an existing transcript in the default mode.
    ///
    /// The messages become in-memory history only: no disk mirror is attached
    /// here and nothing is replayed into one later — a caller resuming a
    /// persisted session must still call
    /// [`attach_transcript`](Self::attach_transcript), and only messages
    /// pushed *after* that attach reach the new log.
    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self {
            messages,
            ..Self::default()
        }
    }

    /// Attaches the disk mirror: every message [`push`](Self::push)ed from now
    /// on is also appended to `log`. The pre-existing messages (if any) are
    /// *not* replayed — the log mirrors appends, it does not reconstruct.
    pub fn attach_transcript(&mut self, log: TranscriptLog) {
        self.transcript = Some(log);
    }

    /// Sets where large tool results may be persisted
    /// (`<workspace>/.kuncode/tool-results`). A pure path value; nothing is
    /// created here.
    pub fn set_tool_results_dir(&mut self, dir: PathBuf) {
        self.tool_results_dir = Some(dir);
    }

    /// The tool-result persistence directory, `None` when persistence was
    /// never assembled. Independent of transcript-log health by design.
    pub fn tool_results_dir(&self) -> Option<PathBuf> {
        self.tool_results_dir.clone()
    }

    /// Hands out a transcript-persistence failure once (take-and-clear), so
    /// the runner can surface exactly one warning per failure.
    pub(crate) fn take_persistence_error(&mut self) -> Option<String> {
        self.transcript.as_mut()?.take_error()
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

    /// Appends a user turn to the transcript. Routes through
    /// [`push`](Self::push) so the disk mirror sees every append.
    pub fn push_user(&mut self, prompt: impl Into<String>) {
        self.push(Message::user(prompt));
    }

    /// Returns the current transcript.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Consumes the session and returns its transcript.
    pub fn into_messages(self) -> Vec<Message> {
        self.messages
    }

    /// The single append chokepoint: every message enters the transcript here
    /// (including [`push_user`](Self::push_user)), so the disk mirror — when
    /// attached — is a faithful, append-ordered copy of the full history.
    pub(crate) fn push(&mut self, message: Message) {
        if let Some(log) = &mut self.transcript {
            log.append(&message);
        }
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
    use crate::test_support::{TestDir, log_lines};

    /// `push_user` funnels through `push`, the single append chokepoint, so
    /// an attached log sees user turns too.
    #[test]
    fn push_user_routes_through_push() {
        let root = TestDir::new();
        let dir = root.path().join("bucket");
        let mut session = AgentSession::new();
        session.attach_transcript(TranscriptLog::new(dir.clone()));

        session.push_user("hello");

        assert_eq!(session.messages().len(), 1);
        assert_eq!(log_lines(&dir).len(), 1);
    }

    /// A cloned session must not append to the original's file (two writers
    /// would interleave timelines); the original keeps writing.
    #[test]
    fn clone_does_not_carry_writer() {
        let root = TestDir::new();
        let dir = root.path().join("bucket");
        let mut session = AgentSession::new();
        session.attach_transcript(TranscriptLog::new(dir.clone()));
        session.push_user("one");

        let mut cloned = session.clone();
        cloned.push_user("two");
        assert_eq!(log_lines(&dir).len(), 1, "clone must not write the log");

        session.push_user("three");
        assert_eq!(log_lines(&dir).len(), 2, "original keeps writing");
    }

    /// The library default is persistence off: a plain session performs no
    /// I/O and reports no persistence state.
    #[test]
    fn unattached_session_writes_nothing() {
        let mut session = AgentSession::new();
        session.push_user("hello");
        assert_eq!(session.messages().len(), 1);
        assert!(session.take_persistence_error().is_none());
        assert!(session.tool_results_dir().is_none());
    }

    /// `tool_results_dir` is a pure path value: set → visible, poisoned log →
    /// still visible (decoupled), clone → survives.
    #[test]
    fn tool_results_dir_derived_from_root() {
        let mut session = AgentSession::new();
        assert!(session.tool_results_dir().is_none());

        let dir = PathBuf::from("/ws/.kuncode/tool-results");
        session.set_tool_results_dir(dir.clone());
        assert_eq!(session.tool_results_dir(), Some(dir.clone()));

        session.attach_transcript(TranscriptLog::poisoned("disk on fire"));
        assert_eq!(
            session.tool_results_dir(),
            Some(dir.clone()),
            "log health must not hide the path value"
        );

        let cloned = session.clone();
        assert_eq!(cloned.tool_results_dir(), Some(dir));
    }
}
