//! Validated rollout thresholds and context-window reservations.

use thiserror::Error;

use super::TokenEstimationError;

const DEFAULT_TARGET_RATIO: f64 = 0.50;
const DEFAULT_SOFT_THRESHOLD: f64 = 0.75;
const DEFAULT_HARD_THRESHOLD: f64 = 0.90;
const DEFAULT_RECENT_RATIO: f64 = 0.10;

/// Controls whether compaction is bypassed, observed, or installed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompactionMode {
    /// Skips measurement and compaction work.
    #[default]
    Disabled,
    /// Measures candidates without changing durable or active state.
    Shadow,
    /// Runs the compaction pipeline and installs committed candidates.
    Enabled,
}

/// Invalid rollout ratios, context windows, or request accounting.
#[derive(Debug, Error)]
pub enum CompactionConfigError {
    /// Ratios must be finite and preserve target, soft, hard ordering.
    #[error(
        "invalid compaction ratios: target={target_ratio}, soft={soft_threshold}, hard={hard_threshold}, recent={recent_ratio}"
    )]
    InvalidRatios {
        /// Desired post-compaction load.
        target_ratio: f64,
        /// Automatic compaction trigger.
        soft_threshold: f64,
        /// Fail-closed request boundary.
        hard_threshold: f64,
        /// Protected recent-tail share.
        recent_ratio: f64,
    },
    /// Output reservation and safety margin must leave input capacity.
    #[error(
        "invalid context window: context_limit={context_limit}, reserved_output={reserved_output}, safety_margin={safety_margin}"
    )]
    InvalidWindow {
        /// Provider model context window.
        context_limit: u64,
        /// Output tokens unavailable to input.
        reserved_output: u64,
        /// Additional protection against estimation drift.
        safety_margin: u64,
    },
    /// Request estimation failed before a budget could be formed.
    #[error(transparent)]
    Estimation(#[from] TokenEstimationError),
}

/// Validated rollout and context-window settings.
///
/// The target is an optimization goal below the soft trigger, while the hard
/// threshold is the fail-closed request boundary. The recent ratio reserves a
/// separate safety suffix and does not weaken protocol-level protection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompactionConfig {
    mode: CompactionMode,
    context_limit: u64,
    reserved_output: u64,
    safety_margin: u64,
    target_ratio: f64,
    soft_threshold: f64,
    hard_threshold: f64,
    recent_ratio: f64,
}

impl CompactionConfig {
    /// Creates settings with the crate's built-in rollout ratios.
    ///
    /// # Errors
    /// Returns [`CompactionConfigError::InvalidWindow`] when reservations leave
    /// no provider input capacity.
    pub fn new(
        mode: CompactionMode,
        context_limit: u64,
        reserved_output: u64,
        safety_margin: u64,
    ) -> Result<Self, CompactionConfigError> {
        validate_window(context_limit, reserved_output, safety_margin)?;
        Ok(Self {
            mode,
            context_limit,
            reserved_output,
            safety_margin,
            target_ratio: DEFAULT_TARGET_RATIO,
            soft_threshold: DEFAULT_SOFT_THRESHOLD,
            hard_threshold: DEFAULT_HARD_THRESHOLD,
            recent_ratio: DEFAULT_RECENT_RATIO,
        })
    }

    /// Replaces all rollout ratios only when their joint invariant holds.
    ///
    /// # Errors
    /// Returns [`CompactionConfigError::InvalidRatios`] for non-finite,
    /// out-of-range, or incorrectly ordered ratios.
    pub fn with_ratios(
        mut self,
        target_ratio: f64,
        soft_threshold: f64,
        hard_threshold: f64,
        recent_ratio: f64,
    ) -> Result<Self, CompactionConfigError> {
        validate_ratios(target_ratio, soft_threshold, hard_threshold, recent_ratio)?;
        self.target_ratio = target_ratio;
        self.soft_threshold = soft_threshold;
        self.hard_threshold = hard_threshold;
        self.recent_ratio = recent_ratio;
        Ok(self)
    }

    /// Returns the configured rollout behavior.
    pub const fn mode(&self) -> CompactionMode {
        self.mode
    }
    /// Returns the provider model's complete context capacity.
    pub const fn context_limit(&self) -> u64 {
        self.context_limit
    }
    /// Returns the fallback output reservation.
    pub const fn reserved_output(&self) -> u64 {
        self.reserved_output
    }
    /// Returns the protection reserved for estimation and framing drift.
    pub const fn safety_margin(&self) -> u64 {
        self.safety_margin
    }
    /// Returns the desired post-compaction load.
    pub const fn target_ratio(&self) -> f64 {
        self.target_ratio
    }
    /// Returns the automatic compaction trigger.
    pub const fn soft_threshold(&self) -> f64 {
        self.soft_threshold
    }
    /// Returns the fail-closed request boundary.
    pub const fn hard_threshold(&self) -> f64 {
        self.hard_threshold
    }
    /// Returns the ordinary recent-tail budget share.
    pub const fn recent_ratio(&self) -> f64 {
        self.recent_ratio
    }
}

fn validate_ratios(
    target_ratio: f64,
    soft_threshold: f64,
    hard_threshold: f64,
    recent_ratio: f64,
) -> Result<(), CompactionConfigError> {
    let ordered = 0.0 < target_ratio
        && target_ratio < soft_threshold
        && soft_threshold < hard_threshold
        && hard_threshold < 1.0;
    if ordered && recent_ratio.is_finite() && 0.0 < recent_ratio && recent_ratio < 1.0 {
        return Ok(());
    }
    Err(CompactionConfigError::InvalidRatios {
        target_ratio,
        soft_threshold,
        hard_threshold,
        recent_ratio,
    })
}

pub(super) fn validate_window(
    context_limit: u64,
    reserved_output: u64,
    safety_margin: u64,
) -> Result<u64, CompactionConfigError> {
    let reserved = reserved_output.checked_add(safety_margin);
    match reserved.and_then(|value| context_limit.checked_sub(value)) {
        Some(usable) if usable > 0 => Ok(usable),
        Some(_) | None => Err(CompactionConfigError::InvalidWindow {
            context_limit,
            reserved_output,
            safety_margin,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{CompactionConfig, CompactionConfigError, CompactionMode};

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
}
