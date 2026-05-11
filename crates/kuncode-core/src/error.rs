//! Top-level error category for every public KunCode API.
//!
//! Phase 0 carries a string payload per variant; later phases replace each
//! payload with a domain-specific error via `#[from]` (see
//! `docs/specs/kuncode-agent-harness-design.md` §17).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum KuncodeError {
    #[error("model error: {0}")]
    Model(String),
    #[error("tool error: {0}")]
    Tool(String),
    #[error("policy error: {0}")]
    Policy(String),
    #[error("workspace error: {0}")]
    Workspace(String),
    #[error("lane error: {0}")]
    Lane(String),
    #[error("context error: {0}")]
    Context(String),
    #[error("event log error: {0}")]
    EventLog(String),
    #[error("artifact error: {0}")]
    Artifact(String),
    #[error("task board error: {0}")]
    TaskBoard(String),
    #[error("config error: {0}")]
    Config(String),
}
