//! Receipt issued after an atomic compaction commit.

use super::NewCompactionCommit;
use crate::session_store::{Seq, SessionId, SessionStoreError, active_messages_sha256};

/// Store-issued receipt used to authorize installation of a compacted context.
///
/// The issuing store is responsible for atomic durability; this value binds the reported
/// journal coordinates and output digest to the in-memory installation boundary.
#[derive(Debug, PartialEq, Eq)]
pub struct CommittedCompaction {
    session_id: SessionId,
    compaction_seq: Seq,
    checkpoint_seq: Seq,
    output_hash: String,
}

impl CommittedCompaction {
    /// Builds a receipt after a store reports the supplied plan atomically committed.
    ///
    /// This validates plan-to-receipt session binding, increasing journal coordinates,
    /// and the installed-message digest. It does not query storage, so the caller remains
    /// responsible for reporting only durable coordinates from one atomic commit.
    ///
    /// # Errors
    /// Returns [`SessionStoreError::InvalidCompaction`] when the plan cannot authorize
    /// the reported sequences or active context. Returns the corresponding serialization
    /// error if the checkpoint messages cannot be encoded in canonical store form.
    pub fn from_committed_write(
        commit: &NewCompactionCommit,
        compaction_seq: Seq,
        checkpoint_seq: Seq,
    ) -> Result<Self, SessionStoreError> {
        if commit.checkpoint.session_id != commit.session_id {
            return Err(SessionStoreError::InvalidCompaction(
                "checkpoint session does not match commit session".to_string(),
            ));
        }
        if compaction_seq <= commit.expected_journal_head || checkpoint_seq <= compaction_seq {
            return Err(SessionStoreError::InvalidCompaction(
                "receipt journal sequences do not follow the expected head".to_string(),
            ));
        }
        let output_hash = active_messages_sha256(&commit.checkpoint.active_messages)?;
        if commit.event.output_hash() != output_hash {
            return Err(SessionStoreError::InvalidCompaction(
                "event output hash does not match checkpoint messages".to_string(),
            ));
        }
        Ok(Self::new(
            commit.session_id.clone(),
            compaction_seq,
            checkpoint_seq,
            output_hash,
        ))
    }

    pub(crate) fn new(
        session_id: SessionId,
        compaction_seq: Seq,
        checkpoint_seq: Seq,
        output_hash: String,
    ) -> Self {
        Self {
            session_id,
            compaction_seq,
            checkpoint_seq,
            output_hash,
        }
    }

    /// Returns the receipt-bound session to prevent cross-session candidate installation.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Returns the compaction audit event's journal sequence.
    pub const fn compaction_seq(&self) -> Seq {
        self.compaction_seq
    }

    /// Returns the `checkpoint_ref` journal sequence.
    pub const fn checkpoint_seq(&self) -> Seq {
        self.checkpoint_seq
    }

    /// Returns the committed active-message digest required before installation.
    pub fn output_hash(&self) -> &str {
        &self.output_hash
    }

    /// Returns the journal head after the atomic transaction completes.
    pub const fn journal_head(&self) -> Seq {
        self.checkpoint_seq
    }
}
