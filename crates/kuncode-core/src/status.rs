//! Terminal state of a `Run`.
//!
//! See `docs/specs/kuncode-agent-harness-design.md` §6.1.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Completed,
    Failed,
    Blocked,
    Cancelled,
    BudgetExceeded,
}

impl RunStatus {
    /// Returns `true` when the run has reached a terminal state and the agent
    /// loop should not be re-entered.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Blocked | Self::Cancelled | Self::BudgetExceeded)
    }
}

#[cfg(test)]
mod tests {
    use super::RunStatus::*;

    #[test]
    fn terminal_truth_table() {
        assert!(!Running.is_terminal());
        assert!(Completed.is_terminal());
        assert!(Failed.is_terminal());
        assert!(Blocked.is_terminal());
        assert!(Cancelled.is_terminal());
        assert!(BudgetExceeded.is_terminal());
    }
}
