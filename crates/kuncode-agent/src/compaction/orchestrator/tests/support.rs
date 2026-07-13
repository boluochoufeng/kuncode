use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use kuncode_core::{
    completion::{
        CompletionRequest, CompletionRequestBuilder, Message, ToolResult, ToolResultContent,
    },
    non_empty_vec::NonEmptyVec,
};

use super::super::{
    CompactionDependencies, CompactionError, CompactionRequestProjector, GroupTokenEstimator,
    RequestProjectionError,
};
use crate::{
    compaction::{
        artifact::{ArtifactTokenCounter, ArtifactTokenCounterError},
        budget::{
            CompactionConfig, CompactionMode, ContextBudget, TokenCountPrecision, TokenEstimate,
            TokenEstimationError, TokenEstimator,
        },
        protocol::ProtocolGroup,
    },
    session::AgentSession,
};

#[path = "support_fixture.rs"]
mod fixture;
#[path = "support_store.rs"]
mod store;
#[path = "support_summary.rs"]
mod summary;

pub(super) use fixture::{DurableFixture, artifact_history, ordinary_history};
pub(super) use store::{RejectedReceiptStore, UnknownCommitStore};
pub(super) use summary::{SummaryBehavior, TestSummarizer};

#[derive(Default)]
pub(super) struct CountingProjector {
    calls: AtomicUsize,
}

impl CountingProjector {
    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl CompactionRequestProjector for CountingProjector {
    fn project(&self, messages: &[Message]) -> Result<CompletionRequest, RequestProjectionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let Some((first, rest)) = messages.split_first() else {
            return Err(RequestProjectionError::new("empty active context"));
        };
        Ok(
            CompletionRequestBuilder::from_messages(NonEmptyVec::from_first_rest(
                first.clone(),
                rest.to_vec(),
            ))
            .build(),
        )
    }
}

pub(super) struct ScriptedEstimator {
    calls: AtomicUsize,
    values: Mutex<VecDeque<u64>>,
}

impl ScriptedEstimator {
    pub(super) fn new(values: impl IntoIterator<Item = u64>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            values: Mutex::new(values.into_iter().collect()),
        }
    }

    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl TokenEstimator for ScriptedEstimator {
    async fn estimate(
        &self,
        _request: &CompletionRequest,
    ) -> Result<TokenEstimate, TokenEstimationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let tokens = self
            .values
            .lock()
            .expect("estimator script should not be poisoned")
            .pop_front()
            .expect("estimator script should cover every projection");
        Ok(TokenEstimate::new(tokens, TokenCountPrecision::Exact))
    }
}

pub(super) struct FixedGroupEstimator {
    calls: AtomicUsize,
    tokens: u64,
}

impl FixedGroupEstimator {
    pub(super) const fn new(tokens: u64) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            tokens,
        }
    }

    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl GroupTokenEstimator for FixedGroupEstimator {
    async fn estimate(&self, _group: &ProtocolGroup) -> Result<u64, CompactionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.tokens)
    }
}

pub(super) struct CountingArtifactCounter {
    calls: AtomicUsize,
    original: u64,
    marker: u64,
}

impl CountingArtifactCounter {
    pub(super) const fn new(original: u64, marker: u64) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            original,
            marker,
        }
    }

    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ArtifactTokenCounter for CountingArtifactCounter {
    async fn count(&self, result: &ToolResult) -> Result<u64, ArtifactTokenCounterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let ToolResultContent::Text(text) = result.content.first();
        Ok(
            if text.text_ref().contains("\"artifact_id\"")
                || text.text_ref().contains("\"kind\":\"slimmed_tool_result\"")
            {
                self.marker
            } else {
                self.original
            },
        )
    }
}

pub(super) fn config(mode: CompactionMode) -> CompactionConfig {
    CompactionConfig::new(mode, 100, 0, 0).expect("test window should be valid")
}

pub(super) struct TestDependencies<'a> {
    pub(super) config: &'a CompactionConfig,
    pub(super) session: &'a mut AgentSession,
    pub(super) store: &'a dyn crate::session_store::SessionStore,
    pub(super) projector: &'a CountingProjector,
    pub(super) estimator: &'a ScriptedEstimator,
    pub(super) group_estimator: &'a FixedGroupEstimator,
    pub(super) artifact_counter: &'a CountingArtifactCounter,
    pub(super) summarizer: &'a TestSummarizer,
}

pub(super) fn dependencies(input: TestDependencies<'_>) -> CompactionDependencies<'_> {
    CompactionDependencies {
        config: input.config,
        measured_before: ContextBudget::new(
            input.config.context_limit(),
            TokenEstimate::new(80, TokenCountPrecision::Exact),
            input.config.reserved_output(),
            input.config.safety_margin(),
        )
        .expect("test baseline should fit the configured window"),
        session: input.session,
        store: input.store,
        projector: input.projector,
        estimator: input.estimator,
        group_estimator: input.group_estimator,
        artifact_counter: input.artifact_counter,
        summarizer: input.summarizer,
        summary_model: "test-summary-model",
    }
}
