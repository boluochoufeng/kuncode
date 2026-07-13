use async_trait::async_trait;
use kuncode_core::completion::{CompletionRequest, CompletionRequestBuilder, Message};

use super::{
    BudgetLevel, CompactionConfig, CompactionConfigError, CompactionMode, ContextBudget,
    TokenCountPrecision, TokenEstimate, TokenEstimationError, TokenEstimator,
};

const CONTEXT_LIMIT: u64 = 1_000;
const RESERVED_OUTPUT: u64 = 100;
const SAFETY_MARGIN: u64 = 100;

#[path = "tests/estimator.rs"]
mod estimator;

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

#[test]
fn default_ratios_match_rollout_design() {
    // Given
    let config = config();

    // When / Then
    assert_eq!(config.mode(), CompactionMode::Enabled);
    assert_eq!(config.target_ratio(), 0.50);
    assert_eq!(config.soft_threshold(), 0.75);
    assert_eq!(config.hard_threshold(), 0.90);
    assert_eq!(config.recent_ratio(), 0.10);
}

#[test]
fn invalid_ratio_order_is_rejected_when_config_is_constructed() {
    // Given / When
    let result = CompactionConfig::new(
        CompactionMode::Shadow,
        CONTEXT_LIMIT,
        RESERVED_OUTPUT,
        SAFETY_MARGIN,
    )
    .and_then(|config| config.with_ratios(0.75, 0.50, 0.90, 0.10));

    // Then
    assert!(matches!(
        result,
        Err(CompactionConfigError::InvalidRatios { .. })
    ));
}

#[test]
fn non_finite_or_out_of_range_ratios_are_rejected() {
    for invalid in [f64::NAN, f64::INFINITY, 0.0, 1.0, -0.1] {
        // Given / When
        let result = CompactionConfig::new(
            CompactionMode::Disabled,
            CONTEXT_LIMIT,
            RESERVED_OUTPUT,
            SAFETY_MARGIN,
        )
        .and_then(|config| config.with_ratios(0.50, 0.75, 0.90, invalid));

        // Then
        assert!(matches!(
            result,
            Err(CompactionConfigError::InvalidRatios { .. })
        ));
    }
}

#[test]
fn unusable_context_window_is_rejected_without_underflow() {
    // Given / When
    let result =
        CompactionConfig::new(CompactionMode::Enabled, 200, RESERVED_OUTPUT, SAFETY_MARGIN);

    // Then
    assert!(matches!(
        result,
        Err(CompactionConfigError::InvalidWindow {
            context_limit: 200,
            reserved_output: RESERVED_OUTPUT,
            safety_margin: SAFETY_MARGIN,
        })
    ));
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
    let wider =
        CompactionConfig::new(CompactionMode::Shadow, 1_000, 100, 100).expect("fixture is valid");
    let narrower =
        CompactionConfig::new(CompactionMode::Shadow, 1_000, 100, 200).expect("fixture is valid");

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
