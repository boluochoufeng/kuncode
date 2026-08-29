//! Workspace-scoped filesystem tools.
//!
//! One file per tool — read / write / edit / glob / grep / ls — over a shared
//! `helpers` base; [`Workspace`](crate::workspace::Workspace) stays the deep
//! path-resolution module the tools sit on. Each tool type is re-exported here,
//! so callers keep using `tool::filesystem::ReadFile` and friends.

mod edit_file;
mod glob;
mod grep;
mod helpers;
mod ls;
mod read_file;
mod write_file;

// The memory tools reuse the same open-refusing-symlinks base and read stamps;
// widened here rather than made fully public because the helpers stay an
// implementation detail of this crate's tools.
pub(crate) use self::helpers::{OpenError, file_stamp, write_no_follow};

pub use self::edit_file::{EditFile, EditFileArgs, EditFileOutput};
pub use self::glob::{Glob, GlobArgs, GlobOutput};
pub use self::grep::{Grep, GrepArgs, GrepFile, GrepLine, GrepOutput, GrepOutputMode};
pub use self::ls::{Ls, LsArgs, LsEntry, LsEntryKind, LsOutput};
pub use self::read_file::{ReadFile, ReadFileArgs, ReadFileOutput, TruncatedLine};
pub use self::write_file::{WriteFile, WriteFileArgs, WriteFileOutput};
