use async_trait::async_trait;
use kuncode_core::completion::{CompletionRequest, Message, Usage};
use thiserror::Error;

use crate::{
    compaction::{
        artifact::{ArtifactSpillError, ArtifactTokenCounter},
        budget::{
            CompactionConfig, CompactionConfigError, ContextBudget, TokenEstimationError,
            TokenEstimator,
        },
        protocol::{ProtocolError, ProtocolGroup},
        selection::SelectionError,
        slimming::ToolResultSlimmingError,
        summary::{ContextSummarizer, SummarizerError},
    },
    session::{AgentSession, SummarySourceError},
    session_store::{Seq, SessionStore, SessionStoreError},
};

/// Rebuilds the exact normal provider request for each active-context candidate.
pub(crate) trait CompactionRequestProjector: Send + Sync {
    fn project(&self, messages: &[Message]) -> Result<CompletionRequest, RequestProjectionError>;
}

/// Failure to rebuild the frozen normal-request shape.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("failed to project compaction candidate request: {message}")]
pub(crate) struct RequestProjectionError {
    message: String,
}

impl RequestProjectionError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Counts intrinsic group cost for selecting the protected recent suffix.
#[async_trait]
pub(crate) trait GroupTokenEstimator: Send + Sync {
    async fn estimate(&self, group: &ProtocolGroup) -> Result<u64, CompactionError>;
}

/// Collaborators frozen for one compaction attempt.
pub(crate) struct CompactionDependencies<'a> {
    pub(crate) config: &'a CompactionConfig,
    pub(crate) measured_before: ContextBudget,
    pub(crate) session: &'a mut AgentSession,
    pub(crate) store: &'a dyn SessionStore,
    pub(crate) projector: &'a dyn CompactionRequestProjector,
    pub(crate) estimator: &'a dyn TokenEstimator,
    pub(crate) group_estimator: &'a dyn GroupTokenEstimator,
    pub(crate) artifact_counter: &'a dyn ArtifactTokenCounter,
    pub(crate) summarizer: &'a dyn ContextSummarizer,
    pub(crate) summary_model: &'a str,
}

/// Passes that materially contributed to an installed candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompactionPass {
    ArtifactSpill,
    ToolResultSlimming,
    SemanticSummary,
    AtomicCommit,
}

impl CompactionPass {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ArtifactSpill => "artifact_spill",
            Self::ToolResultSlimming => "tool_result_slimming",
            Self::SemanticSummary => "semantic_summary",
            Self::AtomicCommit => "atomic_commit",
        }
    }
}

/// Measurements and durable coverage of one installed candidate.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CompactionReport {
    pub(crate) before: ContextBudget,
    pub(crate) after: ContextBudget,
    pub(crate) passes: Vec<CompactionPass>,
    pub(crate) source_start: Seq,
    pub(crate) source_end: Seq,
    pub(crate) checkpoint_seq: Seq,
    pub(crate) artifact_count: usize,
    pub(crate) summary_usage: Option<Usage>,
    pub(crate) summary_latency_ms: Option<u64>,
    pub(crate) target_reached: bool,
}

/// Result class that preserves disabled and shadow no-write semantics.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CompactionOutcome {
    Bypassed,
    Observed(ContextBudget),
    NotNeeded(ContextBudget),
    Compacted(CompactionReport),
}

/// Typed failure from a pass that leaves the old active context installed.
#[derive(Debug, Error)]
pub(crate) enum CompactionError {
    #[error("lossy compaction requires a durable active session")]
    NonDurableSession,
    #[error("compaction thresholds cannot be represented as ordered token limits")]
    InvalidThresholds,
    #[error("no protocol-safe prefix can reduce the request below the soft threshold")]
    NoSafeBoundary,
    #[error("compaction candidate did not strictly reduce provider-visible input")]
    InsufficientReduction,
    #[error("compaction candidate remains at or above the soft threshold")]
    AboveSoftThreshold,
    #[error("active context changed while the compaction candidate was prepared")]
    StaleActiveContext,
    #[error("compaction candidate changed the protected recent tail")]
    ProtectedTailChanged,
    #[error("compaction candidate lineage is missing or inconsistent")]
    InvalidLineage,
    #[error(transparent)]
    Budget(#[from] CompactionConfigError),
    #[error(transparent)]
    TokenEstimation(#[from] TokenEstimationError),
    #[error(transparent)]
    Projection(#[from] RequestProjectionError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Artifact(#[from] ArtifactSpillError),
    #[error(transparent)]
    Slimming(#[from] ToolResultSlimmingError),
    #[error(transparent)]
    Selection(#[from] SelectionError),
    #[error(transparent)]
    SummarySource(#[from] SummarySourceError),
    #[error(transparent)]
    Summary(#[from] SummarizerError),
    #[error("compaction state JSON encoding failed: {0}")]
    Encoding(#[from] serde_json::Error),
    #[error(transparent)]
    Store(#[from] SessionStoreError),
    #[error("compaction receipt could not install the prepared context: {0}")]
    Apply(String),
}
