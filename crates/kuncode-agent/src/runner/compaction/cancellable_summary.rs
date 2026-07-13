//! Cancellation boundary for the model-only portion of semantic compaction.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::compaction::summary::{
    ContextSummarizer, GeneratedSummary, SummarizerError, SummaryRequest,
};

use super::super::cancellation::cancellable;

pub(super) struct CancellableSummarizer<'a, S> {
    inner: &'a S,
    cancel: &'a CancellationToken,
}

impl<'a, S> CancellableSummarizer<'a, S> {
    pub(super) const fn new(inner: &'a S, cancel: &'a CancellationToken) -> Self {
        Self { inner, cancel }
    }
}

#[async_trait]
impl<S> ContextSummarizer for CancellableSummarizer<'_, S>
where
    S: ContextSummarizer,
{
    async fn summarize(
        &self,
        request: SummaryRequest,
    ) -> Result<GeneratedSummary, SummarizerError> {
        cancellable(self.cancel, self.inner.summarize(request))
            .await
            .ok_or(SummarizerError::Cancelled)?
    }
}
