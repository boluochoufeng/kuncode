//! Provider-visible tool-result counting through the active request estimator.

use async_trait::async_trait;
use kuncode_core::{
    completion::{CompletionRequestBuilder, Message, ToolResult, UserContent},
    non_empty_vec::NonEmptyVec,
};

use crate::compaction::{
    artifact::{ArtifactTokenCounter, ArtifactTokenCounterError},
    budget::TokenEstimator,
};

pub(super) struct RequestArtifactCounter<'a> {
    estimator: &'a dyn TokenEstimator,
}

impl<'a> RequestArtifactCounter<'a> {
    pub(super) const fn new(estimator: &'a dyn TokenEstimator) -> Self {
        Self { estimator }
    }
}

#[async_trait]
impl ArtifactTokenCounter for RequestArtifactCounter<'_> {
    async fn count(&self, result: &ToolResult) -> Result<u64, ArtifactTokenCounterError> {
        let message = Message::User {
            content: NonEmptyVec::new(UserContent::ToolResult(result.clone())),
        };
        let request = CompletionRequestBuilder::from_messages(NonEmptyVec::new(message)).build();
        self.estimator
            .estimate(&request)
            .await
            .map(|estimate| estimate.tokens())
            .map_err(|error| ArtifactTokenCounterError::provider(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use kuncode_core::completion::{CompletionRequest, ToolResultContent};

    use super::*;
    use crate::compaction::budget::{TokenCountPrecision, TokenEstimate, TokenEstimationError};

    struct FixedRequestEstimator {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl TokenEstimator for FixedRequestEstimator {
        async fn estimate(
            &self,
            request: &CompletionRequest,
        ) -> Result<TokenEstimate, TokenEstimationError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !matches!(
                request.chat_history.first(),
                Message::User { content }
                    if matches!(content.first(), UserContent::ToolResult(_))
            ) {
                return Err(TokenEstimationError::Provider {
                    provider: "test".to_string(),
                    message: "artifact projection omitted the tool result".to_string(),
                });
            }
            Ok(TokenEstimate::new(37, TokenCountPrecision::Exact))
        }
    }

    #[tokio::test]
    async fn artifact_count_uses_request_estimator_tokens_not_json_bytes() {
        // Given: a JSON result much larger than the estimator's provider-token count.
        let result = ToolResult {
            id: "call-large".to_string(),
            call_id: None,
            content: NonEmptyVec::new(ToolResultContent::text("x".repeat(9_000))),
        };
        let estimator = FixedRequestEstimator {
            calls: AtomicUsize::new(0),
        };
        let counter = RequestArtifactCounter::new(&estimator);

        // When: artifact eligibility asks the normal request estimator seam.
        let tokens = counter
            .count(&result)
            .await
            .expect("request projection should be countable");

        // Then: the provider-visible token unit wins over serialized byte length.
        assert_eq!(tokens, 37);
        assert_eq!(estimator.calls.load(Ordering::SeqCst), 1);
    }
}
