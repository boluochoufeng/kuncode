//! Built-in DeepSeek model capabilities and conservative runtime defaults.

/// Model identifier for DeepSeek V4 Pro.
pub const DEEPSEEK_V4_PRO_MODEL_ID: &str = "deepseek-v4-pro";

/// Model identifier for DeepSeek V4 Flash.
pub const DEEPSEEK_V4_FLASH_MODEL_ID: &str = "deepseek-v4-flash";

/// Physical capabilities and Kuncode defaults for a known DeepSeek model.
///
/// Physical limits describe what the provider accepts. Defaults deliberately
/// stay below those limits so ordinary runs retain predictable cost and
/// latency unless the project opts into a larger budget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeepSeekModelProfile {
    model_id: &'static str,
    context_window_tokens: u64,
    max_output_tokens: u64,
    default_context_limit: u64,
    default_max_tokens: u64,
}

impl DeepSeekModelProfile {
    /// Returns the exact identifier sent to DeepSeek.
    pub const fn model_id(self) -> &'static str {
        self.model_id
    }

    /// Returns the provider's physical context-window limit.
    pub const fn context_window_tokens(self) -> u64 {
        self.context_window_tokens
    }

    /// Returns the provider's physical output-token limit.
    pub const fn max_output_tokens(self) -> u64 {
        self.max_output_tokens
    }

    /// Returns Kuncode's conservative default input-window budget.
    pub const fn default_context_limit(self) -> u64 {
        self.default_context_limit
    }

    /// Returns Kuncode's default output-token budget for ordinary turns.
    pub const fn default_max_tokens(self) -> u64 {
        self.default_max_tokens
    }
}

const V4_PRO: DeepSeekModelProfile = DeepSeekModelProfile {
    model_id: DEEPSEEK_V4_PRO_MODEL_ID,
    context_window_tokens: 1_000_000,
    max_output_tokens: 384_000,
    default_context_limit: 400_000,
    default_max_tokens: 65_536,
};

const V4_FLASH: DeepSeekModelProfile = DeepSeekModelProfile {
    model_id: DEEPSEEK_V4_FLASH_MODEL_ID,
    context_window_tokens: 1_000_000,
    max_output_tokens: 384_000,
    default_context_limit: 400_000,
    default_max_tokens: 65_536,
};

const MODEL_PROFILES: &[DeepSeekModelProfile] = &[V4_PRO, V4_FLASH];

/// Looks up a model profile by its exact provider identifier.
pub fn model_profile(model_id: &str) -> Option<DeepSeekModelProfile> {
    MODEL_PROFILES
        .iter()
        .copied()
        .find(|profile| profile.model_id == model_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_profiles_keep_defaults_within_physical_limits() {
        for profile in MODEL_PROFILES {
            assert!(profile.default_context_limit() <= profile.context_window_tokens());
            assert!(profile.default_max_tokens() <= profile.max_output_tokens());
        }
    }

    #[test]
    fn lookup_is_exact() {
        assert_eq!(model_profile(DEEPSEEK_V4_PRO_MODEL_ID), Some(V4_PRO));
        assert_eq!(model_profile("DEEPSEEK-V4-PRO"), None);
    }
}
