//! Path resolution and error shaping shared by the workspace filesystem tools.
//!
//! Every item here has at least two tool callers (read / write / edit). A helper
//! used by a single tool lives beside that tool instead, so this module stays
//! the genuinely shared base.

use std::{io, path::Path};

use crate::{
    tool::ToolOutput,
    workspace::{Workspace, WorkspaceError},
};

pub(super) fn non_empty_path<D>(path: &str) -> Result<&Path, ToolOutput<D>> {
    if path.trim().is_empty() {
        Err(ToolOutput::failure(
            "invalid_arguments",
            "`path` must not be empty",
        ))
    } else {
        Ok(Path::new(path))
    }
}

pub(super) fn workspace_error<D>(err: WorkspaceError) -> ToolOutput<D> {
    ToolOutput::failure("workspace_path", err.to_string())
}

pub(super) fn io_error<D>(
    kind: &str,
    path: &Path,
    err: io::Error,
    workspace: &Workspace,
) -> ToolOutput<D> {
    ToolOutput::failure(
        kind,
        format!(
            "failed to {kind} `{}`: {err}",
            workspace.relative_display(path)
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::workspace_error;
    use crate::workspace::WorkspaceError;

    #[test]
    fn workspace_errors_are_model_recoverable() {
        let output: crate::tool::ToolOutput<()> =
            workspace_error(WorkspaceError::MissingFileName {
                path: PathBuf::from("."),
            });

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "workspace_path"
        );
    }
}
