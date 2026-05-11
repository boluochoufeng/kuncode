//! `RiskFlag`: tags attached to a `ToolRequest` that policy uses to decide
//! `Allow` / `Deny` / `Ask`. Distinct from `Effect`: an effect describes what
//! the tool can do, a flag describes a per-invocation risk signal.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskFlag {
    LongRunning,
    MutatesWorkspace,
    UntrustedCommand,
    Destructive,
    Network,
}
