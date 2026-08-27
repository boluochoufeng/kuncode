//! Agent runtime and tool orchestration for kuncode.
//!
//! This crate owns the harness layer around `kuncode-core`.

pub mod agent_type;
pub mod compaction;
pub mod error;
pub(crate) mod frontmatter;
pub mod glob;
pub mod hook;
pub mod memory;
pub mod observer;
pub(crate) mod path_text;
pub mod permission;
pub mod registry;
pub mod runner;
pub mod session;
pub mod session_store;
pub mod skill;
pub mod system_prompt;
pub mod tasks;
#[cfg(test)]
pub(crate) mod test_support;
pub mod todo;
pub mod tool;
pub mod workspace;
