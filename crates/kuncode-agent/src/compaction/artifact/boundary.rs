use kuncode_core::completion::Message;
use thiserror::Error;

use crate::{
    compaction::protocol::{ProtectedRecentTail, ProtocolGroup, group_messages},
    session::{AgentSession, DurableSessionContext},
    tool::ToolResultRetention,
};

/// Validated immutable inputs for one spill pass.
pub struct ArtifactSpillInput<'a> {
    pub(super) groups: &'a [ProtocolGroup],
    pub(super) source_message_seqs: Vec<Option<crate::session_store::Seq>>,
    pub(super) source_message_retentions: Vec<ToolResultRetention>,
    pub(super) protected_start: usize,
    pub(super) durable: DurableSessionContext<'a>,
}

impl<'a> ArtifactSpillInput<'a> {
    /// Validates that caller-provided groups belong to the durable active session.
    ///
    /// # Errors
    /// Returns [`ArtifactSpillError`] when authority, protocol, or protection is invalid.
    pub fn new(
        groups: &'a [ProtocolGroup],
        protected: &ProtectedRecentTail,
        session: &'a AgentSession,
    ) -> Result<Self, ArtifactSpillError> {
        let durable = session
            .durable_context()
            .ok_or(ArtifactSpillError::NonDurableSession)?;
        if protected.group_range.end != groups.len()
            || protected.group_range.start > protected.group_range.end
            || (!groups.is_empty() && protected.group_range.start == groups.len())
        {
            return Err(ArtifactSpillError::InvalidProtectedTail);
        }
        let flattened = flatten(groups);
        if flattened != session.messages() {
            return Err(ArtifactSpillError::ActiveSessionMismatch);
        }
        if session.message_lineage().len() != flattened.len() {
            return Err(ArtifactSpillError::InvalidLineage);
        }
        match group_messages(&flattened) {
            Ok(regrouped) if regrouped == groups => {}
            Ok(_) => return Err(ArtifactSpillError::InvalidProtocolGroups),
            Err(error) => return Err(ArtifactSpillError::InvalidProtocol(error.to_string())),
        }
        if let Some(mandatory) = groups
            .iter()
            .rposition(|group| matches!(group, ProtocolGroup::ToolExchange { .. }))
            && protected.group_range.start > mandatory
        {
            return Err(ArtifactSpillError::InvalidProtectedTail);
        }
        Ok(Self {
            groups,
            source_message_seqs: session
                .message_lineage()
                .iter()
                .map(crate::session::MessageLineage::verbatim_journal_seq)
                .collect(),
            source_message_retentions: session
                .message_lineage()
                .iter()
                .map(crate::session::MessageLineage::tool_result_retention)
                .collect(),
            protected_start: protected.group_range.start,
            durable,
        })
    }
}

/// Boundary failures that invalidate the entire spill pass.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ArtifactSpillError {
    /// The active session cannot prove that persistence is healthy.
    #[error("artifact spill requires a durable active session")]
    NonDurableSession,
    /// Caller-provided groups do not represent the active session context.
    #[error("artifact spill groups differ from the active session messages")]
    ActiveSessionMismatch,
    /// The protected suffix does not end at the current group boundary.
    #[error("protected recent tail is not a suffix of the current groups")]
    InvalidProtectedTail,
    /// The supplied groups do not reconstruct the same closed exchanges.
    #[error("artifact spill requires canonical closed protocol groups")]
    InvalidProtocolGroups,
    /// The supplied history contains an open or malformed exchange.
    #[error("artifact spill protocol is invalid: {0}")]
    InvalidProtocol(String),
    /// Active messages and trusted provenance no longer align one-for-one.
    #[error("artifact spill requires aligned active-message lineage")]
    InvalidLineage,
    /// Journal replay failed before artifact writes were allowed.
    #[error("artifact spill journal audit failed: {0}")]
    JournalAudit(String),
    /// The observed journal head differs from the active session frontier.
    #[error("journal head differs from session frontier {frontier}: found {actual}")]
    JournalFrontierStale {
        /// Frontier acknowledged by the active session.
        frontier: i64,
        /// Journal head or newer fact observed during replay.
        actual: i64,
    },
    /// A journal message differs from active context at the same position.
    #[error("active context differs from durable journal at message {index}")]
    JournalMessageMismatch {
        /// Zero-based message position of the first mismatch.
        index: usize,
    },
    /// A concurrent journal write invalidated the entire spill candidate.
    #[error("artifact spill journal head conflict: expected {expected}, found {actual}")]
    JournalHeadConflict {
        /// Journal head authorized by the preceding audit or artifact receipt.
        expected: i64,
        /// Journal head observed by the artifact transaction.
        actual: i64,
    },
    /// A durable artifact write may have committed without returning a receipt.
    #[error("artifact persistence outcome is unknown after {operation}: {message}")]
    PersistenceOutcomeUnknown {
        /// Store operation that attempted the uncertain commit.
        operation: &'static str,
        /// Provider-safe storage failure context.
        message: String,
    },
    /// A store returned a receipt that does not prove the requested artifact write.
    #[error("artifact persistence returned a receipt for a different session or payload")]
    ReceiptMismatch,
}

fn flatten(groups: &[ProtocolGroup]) -> Vec<Message> {
    groups
        .iter()
        .flat_map(|group| match group {
            ProtocolGroup::Message(message) => vec![message.clone()],
            ProtocolGroup::ToolExchange { assistant, results } => {
                let mut messages = Vec::with_capacity(results.len() + 1);
                messages.push(assistant.clone());
                messages.extend(results.iter().cloned());
                messages
            }
        })
        .collect()
}
