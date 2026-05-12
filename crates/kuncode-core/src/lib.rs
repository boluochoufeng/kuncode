//! kuncode-core: protocol primitives shared by every other crate.
//!
//! See `docs/specs/kuncode-agent-harness-design.md` §6 and
//! `docs/plans/kuncode-mvp-development-plan.md` §4.2 for the source of truth.

pub mod budgets;
pub mod capability;
pub mod effect;
pub mod error;
pub mod ids;
pub mod risk;
pub mod role;
pub mod status;
pub mod tool_error_kind;

pub use budgets::{Budgets, MoneyCents};
pub use capability::ToolCapability;
pub use effect::ToolEffect;
pub use error::KuncodeError;
pub use ids::{AgentId, ArtifactId, EventId, RunId, ToolRequestId, TurnId};
pub use risk::RiskFlag;
pub use role::AgentRole;
pub use status::RunStatus;
pub use tool_error_kind::ToolErrorKind;
