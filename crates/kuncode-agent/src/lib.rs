//! Agent runtime and tool orchestration for kuncode.
//!
//! This crate owns the harness layer around `kuncode-core`.

pub mod error;
pub mod glob;
pub mod hook;
pub mod observer;
pub mod permission;
pub mod registry;
pub mod runner;
pub mod session;
pub mod system_prompt;
#[cfg(test)]
pub(crate) mod test_support;
pub mod todo;
pub mod tool;
pub mod transcript;
pub mod workspace;
