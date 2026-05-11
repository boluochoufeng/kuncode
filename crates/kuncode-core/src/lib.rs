//! kuncode-core: protocol primitives shared by every other crate.
//!
//! See `docs/specs/kuncode-agent-harness-design.md` §6 and
//! `docs/specs/kuncode-mvp-development-plan.md` §4.2 for the source of truth.

pub mod budgets;
pub mod error;
pub mod ids;
pub mod risk;
pub mod role;
pub mod status;

pub use budgets::{Budgets, MoneyCents};
pub use error::KuncodeError;
pub use ids::{AgentId, ArtifactId, EventId, RunId, ToolRequestId, TurnId};
pub use risk::RiskFlag;
pub use role::AgentRole;
pub use status::RunStatus;
