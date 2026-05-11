//! `Budgets`: the set of independent hard-stop conditions for a `Run`.
//!
//! Any single field, once set, becomes a hard stop. Unset fields are
//! interpreted as unlimited. See
//! `docs/specs/kuncode-agent-harness-design.md` §6.1.

use serde::{Deserialize, Serialize};
use time::Duration;

/// Money expressed in integer cents to avoid floating-point drift.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MoneyCents(pub u64);

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Budgets {
    pub max_turns: Option<u32>,
    pub max_wall_time: Option<Duration>,
    pub max_tool_calls: Option<u32>,
    pub max_context_tokens: Option<u32>,
    pub max_model_tokens: Option<u64>,
    pub max_cost: Option<MoneyCents>,
}

impl Budgets {
    /// No budget enforced. Useful for tests; not recommended in production.
    pub const fn unlimited() -> Self {
        Self {
            max_turns: None,
            max_wall_time: None,
            max_tool_calls: None,
            max_context_tokens: None,
            max_model_tokens: None,
            max_cost: None,
        }
    }

    /// MVP default per `kuncode-mvp-development-plan.md` §2.1: enforce
    /// `max_turns`, `max_wall_time`, and `max_context_tokens`; leave the rest
    /// unset until later phases enable them.
    pub const fn mvp_default() -> Self {
        Self {
            max_turns: Some(40),
            max_wall_time: Some(Duration::seconds(1800)),
            max_tool_calls: None,
            max_context_tokens: Some(200_000),
            max_model_tokens: None,
            max_cost: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mvp_default_sets_three_required_fields() {
        let b = Budgets::mvp_default();
        assert!(b.max_turns.is_some());
        assert!(b.max_wall_time.is_some());
        assert!(b.max_context_tokens.is_some());
        assert!(b.max_tool_calls.is_none());
        assert!(b.max_model_tokens.is_none());
        assert!(b.max_cost.is_none());
    }

    #[test]
    fn unlimited_has_no_fields() {
        let b = Budgets::unlimited();
        assert!(b.max_turns.is_none());
        assert!(b.max_wall_time.is_none());
        assert!(b.max_tool_calls.is_none());
        assert!(b.max_context_tokens.is_none());
        assert!(b.max_model_tokens.is_none());
        assert!(b.max_cost.is_none());
    }
}
