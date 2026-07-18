//! Durable frontier authority and receipt-bound active-context installation.
//!
//! Losing authority is irreversible for an attached session. Historical ids and
//! acknowledged frontiers remain observable for diagnostics, but cannot justify
//! later compaction or reattachment.

use super::{AgentSession, PreparedActiveContext};
use crate::session_store::{
    CommittedCompaction, NewSession, Seq, SessionId, SessionStore, SessionStoreError,
    active_messages_sha256,
};

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
    /// Creates and attaches a new empty durable journal for this session.
    ///
    /// The store is the authority that the returned identity names a newly
    /// created journal; callers cannot bind an arbitrary existing identity.
    ///
    /// # Errors
    /// Returns [`SessionStartError`] when this session is not pristine or the
    /// store cannot create the journal.
    pub async fn start_durable_session(
        &mut self,
        store: &dyn SessionStore,
        session: NewSession,
    ) -> Result<(), SessionStartError> {
        self.ensure_attachable()?;
        let id = store.create_session(session).await?;
        self.attach_session_id(id)?;
        Ok(())
    }

    /// Attaches a newly created durable session at the empty journal frontier.
    ///
    /// Existing journals must be reconstructed through a future resume path;
    /// attaching their id here would skip facts that are not in memory.
    ///
    /// # Errors
    /// Returns [`SessionAttachError`] without changing the session when it is
    /// non-empty, already attached, or previously lost persistence authority.
    pub(crate) fn attach_session_id(&mut self, id: SessionId) -> Result<(), SessionAttachError> {
        self.ensure_attachable()?;
        self.session_id = Some(id);
        self.last_durable_seq = Some(Seq::ZERO);
        self.non_durable = false;
        Ok(())
    }

    fn ensure_attachable(&self) -> Result<(), SessionAttachError> {
        if self.non_durable {
            return Err(SessionAttachError::PersistenceFailed);
        }
        if self.session_id.is_some() || self.last_durable_seq.is_some() {
            return Err(SessionAttachError::AlreadyAttached);
        }
        if !self.messages.is_empty() || self.todo_generation() != 0 {
            return Err(SessionAttachError::NotPristine);
        }
        Ok(())
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

    /// Returns the last durable frontier established before any authority loss.
    ///
    /// A value may remain visible after [`Self::mark_persistence_failed`]; use
    /// the internal durable context gate before relying on it for lossy mutation.
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
    /// [`PreparedActiveContext`] proves message-lineage alignment, while the
    /// receipt binds the exact encoded messages to this session and a frontier
    /// no older than the currently acknowledged one.
    ///
    /// # Errors
    /// Returns [`SessionMutationError::NonDurable`] when persistence authority is
    /// unhealthy, [`SessionMutationError::SessionMismatch`] for another session's
    /// receipt, [`SessionMutationError::CandidateEncoding`] when the candidate
    /// cannot be hashed, [`SessionMutationError::CandidateMismatch`] when its hash
    /// differs from the receipt, or [`SessionMutationError::StaleCommit`] when the
    /// receipt predates the current durable frontier. The active context remains
    /// unchanged for every failure.
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
    /// The first failure is retained for one-shot observer reporting. Session id
    /// and frontier are intentionally not cleared because their historical values
    /// remain useful for diagnosis, but they no longer constitute authority.
    pub fn mark_persistence_failed(&mut self, reason: impl Into<String>) {
        self.non_durable = true;
        if self.persistence_error.is_none() {
            let reason = reason.into();
            tracing::warn!(
                target: "kuncode::persistence",
                session_id = self.session_id.as_ref().map_or("-", SessionId::as_str),
                diagnostic_chars = reason.chars().count(),
                "session persistence authority lost",
            );
            self.persistence_error = Some(format!("session persistence failed: {reason}"));
        }
    }

    /// Hands out a session-persistence failure once (take-and-clear), so
    /// the runner can surface exactly one warning per failure.
    pub(crate) fn take_persistence_error(&mut self) -> Option<String> {
        self.persistence_error.take()
    }
}

/// Rejects in-memory appends that would bypass an attached journal.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum SessionAppendError {
    /// Attached sessions must receive the store's append receipt before mutation.
    #[error("attached sessions require a durable journal receipt before appending messages")]
    DurableReceiptRequired,
}

/// Failure to create and bind a new durable journal.
#[derive(Debug, thiserror::Error)]
pub enum SessionStartError {
    /// The in-memory session cannot accept a new durable identity.
    #[error(transparent)]
    Attach(#[from] SessionAttachError),
    /// The store could not create the new isolated journal.
    #[error(transparent)]
    Store(#[from] SessionStoreError),
}

/// Rejects attempts to manufacture durable authority from existing state.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum SessionAttachError {
    /// A durable identity has already been installed.
    #[error("session already has a durable identity")]
    AlreadyAttached,
    /// Existing messages or runtime state lack lineage for the new identity.
    #[error("durable identity can only be attached to a pristine session")]
    NotPristine,
    /// Fail-closed persistence state cannot be reset by attaching another id.
    #[error("session persistence authority was previously lost")]
    PersistenceFailed,
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
