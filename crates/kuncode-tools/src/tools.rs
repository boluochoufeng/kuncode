mod common;

pub mod apply_patch;
pub mod exec_argv;
pub mod git_diff;
pub mod git_status;
pub mod read_file;
pub mod search;
pub mod write_file;

pub use apply_patch::ApplyPatchTool;
pub use exec_argv::ExecArgvTool;
pub use git_diff::GitDiffTool;
pub use git_status::GitStatusTool;
pub use read_file::ReadFileTool;
pub use search::SearchTool;
pub use write_file::WriteFileTool;

use crate::{RegisterError, Tool, ToolRuntime};

/// Construct the Phase 2 built-in tool set in deterministic registration order.
pub fn builtin_tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ReadFileTool::new()),
        Box::new(SearchTool::new()),
        Box::new(WriteFileTool::new()),
        Box::new(ApplyPatchTool::new()),
        Box::new(ExecArgvTool::new()),
        Box::new(GitStatusTool::new()),
        Box::new(GitDiffTool::new()),
    ]
}

/// Register every Phase 2 built-in tool into an existing runtime.
pub fn register_builtin_tools(runtime: &mut ToolRuntime) -> Result<(), RegisterError> {
    for tool in builtin_tools() {
        runtime.register(tool)?;
    }
    Ok(())
}
