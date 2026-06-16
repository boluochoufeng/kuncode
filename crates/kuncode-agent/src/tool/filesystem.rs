//! Workspace-scoped filesystem tools.
//!
//! One file per tool — read / write / edit / glob — over a shared `helpers`
//! base; [`Workspace`](crate::workspace::Workspace) stays the deep
//! path-resolution module the tools sit on. Each tool type is re-exported here,
//! so callers keep using `tool::filesystem::ReadFile` and friends.

mod edit_file;
mod glob;
mod helpers;
mod read_file;
mod write_file;

#[cfg(test)]
mod test_support;

pub use self::edit_file::{EditFile, EditFileArgs, EditFileOutput};
pub use self::glob::{Glob, GlobArgs, GlobOutput};
pub use self::read_file::{ReadFile, ReadFileArgs, ReadFileOutput};
pub use self::write_file::{WriteFile, WriteFileArgs, WriteFileOutput};
