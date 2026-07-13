//! Full-request token accounting and validated compaction rollout settings.

use std::io::{self, Write};

use async_trait::async_trait;
use kuncode_core::completion::CompletionRequest;
use thiserror::Error;

mod config;

use config::validate_window;
pub use config::{CompactionConfig, CompactionConfigError, CompactionMode};

const DEFAULT_PROVIDER_FRAMING_TOKENS: u64 = 16;

/// Identifies how closely an input count matches provider tokenization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// Provider-neutral fallback that intentionally overcounts serialized characters.
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
    /// Compaction should be attempted but failure remains recoverable.
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
#[path = "budget/tests.rs"]
mod tests;
