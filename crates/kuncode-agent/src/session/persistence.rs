//! Durable frontier authority and receipt-bound active-context installation.

use super::{AgentSession, PreparedActiveContext};
use crate::session_store::{CommittedCompaction, Seq, SessionId, active_messages_sha256};

/// Borrowed proof derived only from a persistence-healthy [`AgentSession`].
pub(crate) struct DurableSessionContext<'a> {
    session_id: &'a SessionId,
    frontier: Seq,
}

impl DurableSessionContext<'_> {
    /// Returns the durable session whose journal must authorize compaction.
    pub(crate) const fn session_id(&self) -> &SessionId {
        self.session_id
    }

    /// Returns the latest journal sequence acknowledged by the active session.
    pub(crate) const fn frontier(&self) -> Seq {
        self.frontier
    }
}

impl AgentSession {
    /// Attaches a newly created durable session at the empty journal frontier.
    ///
    /// Existing journals must be reconstructed through a future resume path;
    /// attaching their id here would skip facts that are not in memory.
    pub fn attach_session_id(&mut self, id: SessionId) {
        self.session_id = Some(id);
        self.last_durable_seq = Some(Seq::ZERO);
        self.non_durable = false;
    }

    /// Returns the durable session identity when persistence is attached.
    pub fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }

    pub(crate) fn is_durable(&self) -> bool {
        self.session_id.is_some() && self.last_durable_seq.is_some() && !self.non_durable
    }

    /// Derives compaction authority only while persistence remains healthy.
    pub(crate) fn durable_context(&self) -> Option<DurableSessionContext<'_>> {
        if !self.is_durable() {
            return None;
        }
        Some(DurableSessionContext {
            session_id: self.session_id.as_ref()?,
            frontier: self.last_durable_seq?,
        })
    }

    /// Returns the latest journal sequence acknowledged by the store.
    pub fn durable_seq(&self) -> Option<Seq> {
        self.last_durable_seq
    }

    pub(crate) fn advance_durable_seq(&mut self, seq: Seq) {
        if let Some(current) = &mut self.last_durable_seq
            && seq > *current
        {
            *current = seq;
        }
    }

    /// Installs a lossy candidate using a commit receipt bound to this session.
    ///
    /// # Errors
    /// Rejects the candidate without changing the active context when the
    /// session is non-durable, the receipt belongs to another session, or the
    /// receipt predates the current durable frontier.
    pub(crate) fn install_compacted_context(
        &mut self,
        prepared: PreparedActiveContext,
        committed: CommittedCompaction,
    ) -> Result<(), SessionMutationError> {
        if !self.is_durable() {
            return Err(SessionMutationError::NonDurable);
        }
        let current = self
            .last_durable_seq
            .ok_or(SessionMutationError::NonDurable)?;
        let session_id = self
            .session_id
            .as_ref()
            .ok_or(SessionMutationError::NonDurable)?;
        if committed.session_id() != session_id {
            return Err(SessionMutationError::SessionMismatch);
        }
        let output_hash = active_messages_sha256(&prepared.messages)
            .map_err(|error| SessionMutationError::CandidateEncoding(error.to_string()))?;
        if committed.output_hash() != output_hash {
            return Err(SessionMutationError::CandidateMismatch);
        }
        let committed_head = committed.journal_head();
        if committed_head < current {
            return Err(SessionMutationError::StaleCommit {
                committed: committed_head.get(),
                current: current.get(),
            });
        }
        self.messages = prepared.messages;
        self.message_lineage = prepared.lineage;
        self.active_summary = prepared.summary;
        self.last_durable_seq = Some(committed_head);
        Ok(())
    }

    /// Permanently disables lossy mutation after persistence authority is lost.
    ///
    /// The first failure is retained for one-shot observer reporting.
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
}

/// Prevents active-context replacement across an unproven durability boundary.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum SessionMutationError {
    /// The session lacks a provably complete durable journal.
    #[error("cannot replace active context for a non-durable session")]
    NonDurable,
    /// The receipt cannot be reused across sessions.
    #[error("compaction receipt belongs to a different session")]
    SessionMismatch,
    /// The receipt was committed for a different active-context payload.
    #[error("compaction receipt does not authorize this active context")]
    CandidateMismatch,
    /// Stable candidate encoding failed before any in-memory mutation.
    #[error("failed to encode compacted active context: {0}")]
    CandidateEncoding(String),
    /// The receipt predates the current durable frontier.
    #[error("compaction commit {committed} is older than durable journal head {current}")]
    StaleCommit { committed: i64, current: i64 },
}
