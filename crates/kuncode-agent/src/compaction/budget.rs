//! Full-request token accounting and validated compaction rollout settings.

use std::io::{self, Write};

use async_trait::async_trait;
use kuncode_core::completion::CompletionRequest;
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod config;

use config::validate_window;
pub use config::{CompactionConfig, CompactionConfigError, CompactionMode};

const DEFAULT_PROVIDER_FRAMING_TOKENS: u64 = 16;

/// Identifies how closely an input count matches provider tokenization.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenCountPrecision {
    /// Count returned by the provider's tokenizer or count endpoint.
    Exact,
    /// Count approximated by a provider-specific adapter.
    ProviderEstimate,
    /// Count produced by a provider-neutral local heuristic.
    LocalEstimate,
}

/// A token count paired with its precision for telemetry and rollout decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenEstimate {
    tokens: u64,
    precision: TokenCountPrecision,
}

impl TokenEstimate {
    /// Creates an estimate without discarding its accounting precision.
    pub const fn new(tokens: u64, precision: TokenCountPrecision) -> Self {
        Self { tokens, precision }
    }

    /// Returns the estimated provider-visible input tokens.
    pub const fn tokens(self) -> u64 {
        self.tokens
    }

    /// Returns the source precision of this estimate.
    pub const fn precision(self) -> TokenCountPrecision {
        self.precision
    }
}

/// Failures while projecting a provider request into a token estimate.
#[derive(Debug, Error)]
pub enum TokenEstimationError {
    /// The provider-neutral request projection could not be serialized.
    #[error("failed to serialize completion request for token estimation: {0}")]
    Serialize(#[from] serde_json::Error),
    /// The serialized request cannot be represented by the token counter.
    #[error("completion request is too large to represent as a u64 token estimate")]
    RequestTooLarge,
    /// The selected provider has no usable counting capability.
    #[error("token counting is unavailable for provider `{provider}`")]
    ProviderUnavailable {
        /// Adapter identifier used to select the counting capability.
        provider: String,
    },
    /// A provider counting capability failed while processing the request.
    #[error("token counting failed for provider `{provider}`: {message}")]
    Provider {
        /// Adapter identifier used to select the counting capability.
        provider: String,
        /// Provider-safe failure context supplied by the adapter.
        message: String,
    },
}

/// Estimates provider-visible input for an already assembled request.
#[async_trait]
pub trait TokenEstimator: Send + Sync {
    /// Counts the final request projection, including tools and framing.
    ///
    /// # Errors
    /// Returns [`TokenEstimationError`] when the request cannot be projected.
    async fn estimate(
        &self,
        request: &CompletionRequest,
    ) -> Result<TokenEstimate, TokenEstimationError>;
}

/// Provider-neutral fallback that counts serialized UTF-8 bytes plus fixed framing.
///
/// This heuristic reports [`TokenCountPrecision::LocalEstimate`]: it does not
/// reproduce any provider tokenizer and must not be treated as an exact count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConservativeTokenEstimator {
    provider_framing_tokens: u64,
}

impl ConservativeTokenEstimator {
    /// Creates a local estimator with adapter-supplied framing overhead.
    pub const fn new(provider_framing_tokens: u64) -> Self {
        Self {
            provider_framing_tokens,
        }
    }
}

impl Default for ConservativeTokenEstimator {
    fn default() -> Self {
        Self::new(DEFAULT_PROVIDER_FRAMING_TOKENS)
    }
}

#[async_trait]
impl TokenEstimator for ConservativeTokenEstimator {
    async fn estimate(
        &self,
        request: &CompletionRequest,
    ) -> Result<TokenEstimate, TokenEstimationError> {
        let mut writer = CountingWriter::default();
        if let Err(error) = serde_json::to_writer(&mut writer, request) {
            return if writer.overflowed {
                Err(TokenEstimationError::RequestTooLarge)
            } else {
                Err(TokenEstimationError::Serialize(error))
            };
        }
        let tokens = writer
            .bytes
            .checked_add(self.provider_framing_tokens)
            .ok_or(TokenEstimationError::RequestTooLarge)?;
        Ok(TokenEstimate::new(
            tokens,
            TokenCountPrecision::LocalEstimate,
        ))
    }
}

#[derive(Default)]
struct CountingWriter {
    bytes: u64,
    overflowed: bool,
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let length = u64::try_from(buffer.len()).map_err(|_| self.overflow_error())?;
        self.bytes = self
            .bytes
            .checked_add(length)
            .ok_or_else(|| self.overflow_error())?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl CountingWriter {
    fn overflow_error(&mut self) -> io::Error {
        self.overflowed = true;
        io::Error::other("serialized completion request exceeds u64 bytes")
    }
}

/// Pressure classification used by the harness at a request boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetLevel {
    /// The request may proceed without compaction.
    Normal,
    /// Compaction is attempted; fallback is allowed only when the attempt produces
    /// no authority-invalidating durable outcome.
    Soft,
    /// Compaction must succeed before the request is sent.
    Hard,
}

/// Validated accounting for the next provider request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextBudget {
    context_limit: u64,
    current_input: u64,
    reserved_output: u64,
    safety_margin: u64,
    usable_input_limit: u64,
    precision: TokenCountPrecision,
}

impl ContextBudget {
    /// Estimates a complete request, using its output limit when present.
    ///
    /// # Errors
    /// Returns [`CompactionConfigError`] when estimation fails or the request's
    /// output limit leaves no usable input capacity.
    pub async fn for_request(
        config: &CompactionConfig,
        request: &CompletionRequest,
        estimator: &dyn TokenEstimator,
    ) -> Result<Self, CompactionConfigError> {
        let estimate = estimator.estimate(request).await?;
        let reserved_output = request.max_tokens.unwrap_or(config.reserved_output());
        Self::new(
            config.context_limit(),
            estimate,
            reserved_output,
            config.safety_margin(),
        )
    }

    /// Creates a budget only when output and safety reservations leave input room.
    ///
    /// # Errors
    /// Returns [`CompactionConfigError::InvalidWindow`] when reservations leave
    /// no usable input capacity.
    pub fn new(
        context_limit: u64,
        estimate: TokenEstimate,
        reserved_output: u64,
        safety_margin: u64,
    ) -> Result<Self, CompactionConfigError> {
        let usable_input_limit = validate_window(context_limit, reserved_output, safety_margin)?;
        Ok(Self {
            context_limit,
            current_input: estimate.tokens(),
            reserved_output,
            safety_margin,
            usable_input_limit,
            precision: estimate.precision(),
        })
    }

    /// Classifies the load against exact soft and hard boundaries.
    pub fn level(&self, config: &CompactionConfig) -> BudgetLevel {
        let load = self.load_ratio();
        if load >= config.hard_threshold() {
            BudgetLevel::Hard
        } else if load >= config.soft_threshold() {
            BudgetLevel::Soft
        } else {
            BudgetLevel::Normal
        }
    }

    /// Returns the provider-visible input divided by usable capacity.
    pub fn load_ratio(&self) -> f64 {
        self.current_input as f64 / self.usable_input_limit as f64
    }

    /// Reports whether deterministic passes reached their optimization target.
    pub fn reached_target(&self, config: &CompactionConfig) -> bool {
        self.load_ratio() <= config.target_ratio()
    }

    /// Returns the provider-visible input estimate.
    pub const fn current_input(&self) -> u64 {
        self.current_input
    }
    /// Returns the provider model's complete context capacity.
    pub const fn context_limit(&self) -> u64 {
        self.context_limit
    }
    /// Returns the output reservation applied to this request.
    pub const fn reserved_output(&self) -> u64 {
        self.reserved_output
    }
    /// Returns the protection reserved for estimation and framing drift.
    pub const fn safety_margin(&self) -> u64 {
        self.safety_margin
    }
    /// Returns the context capacity available to input.
    pub const fn usable_input_limit(&self) -> u64 {
        self.usable_input_limit
    }
    /// Returns the source precision of the input count.
    pub const fn precision(&self) -> TokenCountPrecision {
        self.precision
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use kuncode_core::completion::{CompletionRequest, CompletionRequestBuilder, Message};

    use super::{
        BudgetLevel, CompactionConfig, CompactionConfigError, CompactionMode, ContextBudget,
        TokenCountPrecision, TokenEstimate, TokenEstimationError, TokenEstimator,
    };

    const CONTEXT_LIMIT: u64 = 1_000;
    const RESERVED_OUTPUT: u64 = 100;
    const SAFETY_MARGIN: u64 = 100;

    fn config() -> CompactionConfig {
        CompactionConfig::new(
            CompactionMode::Enabled,
            CONTEXT_LIMIT,
            RESERVED_OUTPUT,
            SAFETY_MARGIN,
        )
        .expect("fixture is valid")
    }

    #[derive(Clone, Copy)]
    struct FixedEstimator(TokenEstimate);

    #[async_trait]
    impl TokenEstimator for FixedEstimator {
        async fn estimate(
            &self,
            _request: &CompletionRequest,
        ) -> Result<TokenEstimate, TokenEstimationError> {
            Ok(self.0)
        }
    }

    pub(super) fn request(max_tokens: Option<u64>) -> CompletionRequest {
        CompletionRequestBuilder::new(Message::user("hello"))
            .max_tokens(max_tokens)
            .build()
    }

    #[tokio::test]
    async fn exact_thresholds_are_classified_as_soft_and_hard() {
        for (current_input, expected) in [
            (599, BudgetLevel::Normal),
            (600, BudgetLevel::Soft),
            (719, BudgetLevel::Soft),
            (720, BudgetLevel::Hard),
        ] {
            // Given
            let estimator = FixedEstimator(TokenEstimate::new(
                current_input,
                TokenCountPrecision::Exact,
            ));

            // When
            let budget = ContextBudget::for_request(&config(), &request(None), &estimator)
                .await
                .expect("fixture budget is valid");

            // Then
            assert_eq!(budget.level(&config()), expected);
        }
    }

    #[tokio::test]
    async fn request_max_tokens_changes_the_usable_limit() {
        // Given
        let estimator = FixedEstimator(TokenEstimate::new(
            400,
            TokenCountPrecision::ProviderEstimate,
        ));

        // When
        let budget = ContextBudget::for_request(&config(), &request(Some(250)), &estimator)
            .await
            .expect("request-specific output reserve is valid");

        // Then
        assert_eq!(budget.reserved_output(), 250);
        assert_eq!(budget.usable_input_limit(), 650);
        assert_eq!(budget.current_input(), 400);
        assert_eq!(budget.precision(), TokenCountPrecision::ProviderEstimate);
    }

    #[tokio::test]
    async fn safety_margin_changes_capacity_without_changing_input_estimate() {
        // Given
        let estimator = FixedEstimator(TokenEstimate::new(
            400,
            TokenCountPrecision::ProviderEstimate,
        ));
        let wider = CompactionConfig::new(CompactionMode::Shadow, 1_000, 100, 100)
            .expect("fixture is valid");
        let narrower = CompactionConfig::new(CompactionMode::Shadow, 1_000, 100, 200)
            .expect("fixture is valid");

        // When
        let wider_budget = ContextBudget::for_request(&wider, &request(None), &estimator)
            .await
            .expect("fixture budget is valid");
        let narrower_budget = ContextBudget::for_request(&narrower, &request(None), &estimator)
            .await
            .expect("fixture budget is valid");

        // Then
        assert_eq!(
            wider_budget.current_input(),
            narrower_budget.current_input()
        );
        assert_eq!(wider_budget.usable_input_limit(), 800);
        assert_eq!(narrower_budget.usable_input_limit(), 700);
    }

    #[tokio::test]
    async fn request_output_reserve_can_invalidate_the_window() {
        // Given
        let estimator = FixedEstimator(TokenEstimate::new(1, TokenCountPrecision::LocalEstimate));

        // When
        let result = ContextBudget::for_request(&config(), &request(Some(900)), &estimator).await;

        // Then
        assert!(matches!(
            result,
            Err(CompactionConfigError::InvalidWindow {
                reserved_output: 900,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn target_ratio_is_an_optimization_boundary() {
        // Given
        let at_target = FixedEstimator(TokenEstimate::new(400, TokenCountPrecision::Exact));
        let above_target = FixedEstimator(TokenEstimate::new(401, TokenCountPrecision::Exact));

        // When
        let reached = ContextBudget::for_request(&config(), &request(None), &at_target)
            .await
            .expect("fixture budget is valid");
        let not_reached = ContextBudget::for_request(&config(), &request(None), &above_target)
            .await
            .expect("fixture budget is valid");

        // Then
        assert!(reached.reached_target(&config()));
        assert!(!not_reached.reached_target(&config()));
    }

    mod estimator {
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
                    FakeProviderResult::Unavailable => {
                        Err(TokenEstimationError::ProviderUnavailable {
                            provider: "fake".to_owned(),
                        })
                    }
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
    }
}
