use std::io::Write;

use async_trait::async_trait;
use kuncode_core::completion::{
    CompletionRequest, CompletionRequestBuilder, Message, ToolDefinition,
};

use super::{config, request};
use crate::compaction::budget::{
    CompactionConfigError, ConservativeTokenEstimator, ContextBudget, CountingWriter,
    TokenCountPrecision, TokenEstimate, TokenEstimationError, TokenEstimator,
};

enum FakeProviderResult {
    Success(TokenEstimate),
    Unavailable,
    Failed,
}

struct FakeProviderEstimator(FakeProviderResult);

#[async_trait]
impl TokenEstimator for FakeProviderEstimator {
    async fn estimate(
        &self,
        _request: &CompletionRequest,
    ) -> Result<TokenEstimate, TokenEstimationError> {
        match self.0 {
            FakeProviderResult::Success(estimate) => Ok(estimate),
            FakeProviderResult::Unavailable => Err(TokenEstimationError::ProviderUnavailable {
                provider: "fake".to_owned(),
            }),
            FakeProviderResult::Failed => Err(TokenEstimationError::Provider {
                provider: "fake".to_owned(),
                message: "count endpoint failed".to_owned(),
            }),
        }
    }
}

#[tokio::test]
async fn conservative_estimator_counts_the_entire_final_request_projection() {
    // Given
    let estimator = ConservativeTokenEstimator::default();
    let base = CompletionRequestBuilder::new(Message::user("u")).build();
    let with_system = CompletionRequestBuilder::new(Message::system("system instructions"))
        .message(Message::user("u"))
        .build();
    let with_tool = CompletionRequestBuilder::new(Message::user("u"))
        .tool(ToolDefinition {
            name: "read".to_owned(),
            description: "read a file".to_owned(),
            parameters: serde_json::json!({"type": "object"}),
        })
        .build();
    let with_more_history = CompletionRequestBuilder::new(Message::user("u"))
        .message(Message::assistant("a long assistant response"))
        .build();

    // When
    let base_count = estimator
        .estimate(&base)
        .await
        .expect("request serializes")
        .tokens();

    // Then
    assert!(
        estimator
            .estimate(&with_system)
            .await
            .expect("request serializes")
            .tokens()
            > base_count
    );
    assert!(
        estimator
            .estimate(&with_tool)
            .await
            .expect("request serializes")
            .tokens()
            > base_count
    );
    assert!(
        estimator
            .estimate(&with_more_history)
            .await
            .expect("request serializes")
            .tokens()
            > base_count
    );
}

#[tokio::test]
async fn conservative_estimator_includes_adapter_framing() {
    // Given
    let request = request(None);
    let without_framing = ConservativeTokenEstimator::new(0);
    let with_framing = ConservativeTokenEstimator::new(32);

    // When
    let unframed = without_framing
        .estimate(&request)
        .await
        .expect("request serializes")
        .tokens();
    let framed = with_framing
        .estimate(&request)
        .await
        .expect("request serializes")
        .tokens();

    // Then
    assert_eq!(framed - unframed, 32);
}

#[tokio::test]
async fn conservative_estimator_reports_framing_overflow() {
    // Given
    let estimator = ConservativeTokenEstimator::new(u64::MAX);

    // When
    let result = estimator.estimate(&request(None)).await;

    // Then
    assert!(matches!(result, Err(TokenEstimationError::RequestTooLarge)));
}

#[test]
fn counting_writer_rejects_u64_overflow() {
    // Given
    let mut writer = CountingWriter {
        bytes: u64::MAX,
        overflowed: false,
    };

    // When
    let result = writer.write(&[0]);

    // Then
    assert!(result.is_err());
    assert!(writer.overflowed);
}

#[tokio::test]
async fn provider_estimator_preserves_provider_precision() {
    // Given
    let estimator = FakeProviderEstimator(FakeProviderResult::Success(TokenEstimate::new(
        321,
        TokenCountPrecision::ProviderEstimate,
    )));

    // When
    let budget = ContextBudget::for_request(&config(), &request(None), &estimator)
        .await
        .expect("fake provider succeeds");

    // Then
    assert_eq!(budget.current_input(), 321);
    assert_eq!(budget.precision(), TokenCountPrecision::ProviderEstimate);
}

#[tokio::test]
async fn provider_unavailable_is_propagated_without_local_fallback() {
    // Given
    let estimator = FakeProviderEstimator(FakeProviderResult::Unavailable);

    // When
    let result = ContextBudget::for_request(&config(), &request(None), &estimator).await;

    // Then
    assert!(matches!(
        result,
        Err(CompactionConfigError::Estimation(
            TokenEstimationError::ProviderUnavailable { provider }
        )) if provider == "fake"
    ));
}

#[tokio::test]
async fn provider_failure_is_propagated_without_local_fallback() {
    // Given
    let estimator = FakeProviderEstimator(FakeProviderResult::Failed);

    // When
    let result = ContextBudget::for_request(&config(), &request(None), &estimator).await;

    // Then
    assert!(matches!(
        result,
        Err(CompactionConfigError::Estimation(TokenEstimationError::Provider {
            provider,
            message,
        })) if provider == "fake" && message == "count endpoint failed"
    ));
}
