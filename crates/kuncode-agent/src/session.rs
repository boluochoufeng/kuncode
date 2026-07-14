//! In-memory conversation state for agent turns.
//!
//! Provider-visible messages are kept aligned one-for-one with harness-owned
//! lineage. Roles alone never establish human authorship or durable provenance.

use kuncode_core::completion::Message;

#[cfg(test)]
use crate::compaction::summary::ContinuitySummary;
use crate::permission::{PermissionMode, PermissionSessionState};
use crate::session_store::{Seq, SessionId};
use crate::todo::{TodoHandle, TodoItem};
use crate::tool::ToolResultRetention;

mod compaction;
mod lineage;
mod persistence;

pub(crate) use compaction::SummarySourceBinding;
pub use compaction::SummarySourceError;
pub(crate) use lineage::{ActiveSummary, MessageCoverage, MessageLineage, PreparedActiveContext};
pub(crate) use persistence::DurableSessionContext;
#[cfg(test)]
use persistence::SessionMutationError;
pub use persistence::{SessionAppendError, SessionAttachError, SessionStartError};

/// Active conversation context owned by the caller between agent turns.
///
/// Besides the message history it carries the mutable
/// [`PermissionSessionState`]: keeping per-session grants and mode here — rather
/// than on the shared, `&self` runner — gives per-session isolation with no lock.
///
/// `messages` and `message_lineage` always have equal length. A persistence
/// failure poisons the durable frontier for future lossy mutation, but retains
/// the last session id and acknowledged sequence as historical diagnostics.
#[derive(Debug, Default)]
pub struct AgentSession {
    messages: Vec<Message>,
    message_lineage: Vec<MessageLineage>,
    active_summary: Option<ActiveSummary>,
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
    last_durable_seq: Option<Seq>,
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
    /// separate timeline. Its copied messages receive untrusted lineage because
    /// cloning provider-visible content cannot reproduce durable or human-input
    /// provenance.
    fn clone(&self) -> Self {
        let messages = self.messages.clone();
        Self {
            message_lineage: lineage::untrusted_lineage(messages.len()),
            active_summary: None,
            messages,
            permissions: self.permissions.clone(),
            seq: self.seq,
            todos: self.todos.deep_clone(),
            session_id: None,
            last_durable_seq: None,
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
    /// The messages remain non-durable in the first release, and every role is
    /// assigned untrusted lineage. Resume must later reconstruct both messages
    /// and lineage through a dedicated store path; [`Self::start_durable_session`]
    /// intentionally rejects this non-empty state.
    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self {
            message_lineage: lineage::untrusted_lineage(messages.len()),
            active_summary: None,
            messages,
            ..Self::default()
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

    /// Appends a user turn only when no durable journal is attached.
    ///
    /// Attached sessions must use the runner so the journal commit precedes
    /// the in-memory mutation.
    ///
    /// # Errors
    /// Returns [`SessionAppendError::DurableReceiptRequired`] when this session
    /// already has a durable identity.
    pub fn push_user(&mut self, prompt: impl Into<String>) -> Result<(), SessionAppendError> {
        if self.session_id.is_some() {
            return Err(SessionAppendError::DurableReceiptRequired);
        }
        self.push(Message::user(prompt));
        Ok(())
    }

    /// Returns the current active context.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Consumes the session and returns its active context.
    ///
    /// This intentionally discards lineage and durable-frontier authority; the
    /// returned messages cannot later be reattached as a durable session.
    pub fn into_messages(self) -> Vec<Message> {
        self.messages
    }

    /// The single in-memory append chokepoint.
    pub(crate) fn push(&mut self, message: Message) {
        self.push_with_journal_seq(message, None);
    }

    pub(crate) fn push_with_journal_seq(&mut self, message: Message, journal_seq: Option<Seq>) {
        self.push_with_lineage(message, journal_seq, false);
    }

    pub(crate) fn push_human_with_journal_seq(
        &mut self,
        message: Message,
        journal_seq: Option<Seq>,
    ) {
        self.push_with_lineage(message, journal_seq, true);
    }

    pub(crate) fn push_tool_result_with_journal_seq(
        &mut self,
        message: Message,
        journal_seq: Option<Seq>,
        retention: ToolResultRetention,
    ) {
        if let Some(seq) = journal_seq {
            self.advance_durable_seq(seq);
        }
        self.messages.push(message);
        // Retention is accepted only at the live tool boundary and travels with
        // lineage so later slimming cannot infer authorization from payload shape.
        self.message_lineage.push(
            MessageLineage::appended(journal_seq, false).with_tool_result_retention(retention),
        );
    }

    fn push_with_lineage(
        &mut self,
        message: Message,
        journal_seq: Option<Seq>,
        human_authored: bool,
    ) {
        if let Some(seq) = journal_seq {
            self.advance_durable_seq(seq);
        }
        // Keep both vectors in lockstep at the only ordinary append boundary.
        self.messages.push(message);
        self.message_lineage
            .push(MessageLineage::appended(journal_seq, human_authored));
    }

    /// Returns provenance aligned one-for-one with [`Self::messages`].
    pub(crate) fn message_lineage(&self) -> &[MessageLineage] {
        &self.message_lineage
    }

    #[cfg(test)]
    pub(crate) const fn active_summary(&self) -> Option<&ContinuitySummary> {
        match self.active_summary.as_ref() {
            Some(active) => Some(active.summary()),
            None => None,
        }
    }

    pub(crate) const fn active_summary_record(&self) -> Option<&ActiveSummary> {
        self.active_summary.as_ref()
    }

    /// Returns only indices marked at the direct human-input boundary.
    pub(crate) fn trusted_human_message_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.message_lineage
            .iter()
            .enumerate()
            .filter_map(|(index, lineage)| lineage.human_authored().then_some(index))
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
mod tests;
