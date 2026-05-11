//! `AgentRole`: a label that picks a capability set and context policy.
//!
//! Role does not authorize directly; policy still adjudicates each tool
//! request. See `docs/specs/kuncode-agent-harness-design.md` §6.2.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Lead,
    Explorer,
    Worker,
    Verifier,
    Reviewer,
}
