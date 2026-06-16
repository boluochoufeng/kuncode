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

/// Lexically normalizes a path argument into a workspace-relative, slash-form
/// string for permission-rule matching. Pure string work — **no filesystem
/// access**: an absolute path under the root is stripped textually, separators
/// are unified, and the result is [`lexical_fold`]-ed so `.`, `..`, and `//`
/// can't spell a path around a `deny` rule (`secrets/../secrets/x`). The
/// authoritative canonicalization + symlink/boundary check still happens in
/// each tool's `run`; this is only what a `Read(src/**)`-style rule matches.
pub(super) fn rule_path(workspace: &Workspace, path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let root = workspace.root().to_string_lossy().replace('\\', "/");
    let relative = normalized
        .strip_prefix(&format!("{root}/"))
        .unwrap_or(&normalized);
    lexical_fold(relative)
}

/// Folds a slash-separated path lexically: drops `.` and empty (`//`) segments
/// and resolves `..` by popping the preceding normal segment. A leading `/` is
/// preserved, and `..` that would climb above the start is kept verbatim (it
/// can't be folded without a parent, and `run`'s boundary rejects it anyway).
///
/// Symlinks are **not** resolved here — that needs the filesystem and would
/// reintroduce TOCTOU into the otherwise-pure permission layer. This only
/// closes the *textual* spellings of a path; the hard guarantee is still the
/// workspace-boundary canonicalize in each tool's `run`.
fn lexical_fold(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                // Pop a real segment; keep `..` only when there's nothing to
                // pop (a relative path climbing above its start), never for an
                // absolute path (which can't climb above root).
                if matches!(stack.last(), Some(&seg) if seg != "..") {
                    stack.pop();
                } else if !absolute {
                    stack.push("..");
                }
            }
            normal => stack.push(normal),
        }
    }
    let joined = stack.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

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

/// Non-blocking `stat` predicate. A missing path or stat error resolves to
/// `false`, matching the semantics of [`Path::is_file`].
pub(super) async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .map(|meta| meta.is_file())
        .unwrap_or(false)
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

    use super::{lexical_fold, workspace_error};
    use crate::workspace::WorkspaceError;

    #[test]
    fn lexical_fold_closes_textual_path_spellings() {
        // `.`, `..`, and `//` are folded so they can't spell a path around a
        // `deny(secrets/**)` rule.
        assert_eq!(lexical_fold("secrets/../secrets/x"), "secrets/x");
        assert_eq!(lexical_fold("./secrets/x"), "secrets/x");
        assert_eq!(lexical_fold("secrets//x"), "secrets/x");
        assert_eq!(lexical_fold("src/../README.md"), "README.md");
        // A leading climb is kept verbatim (run's boundary rejects it); an
        // absolute path keeps its leading slash and never climbs above root.
        assert_eq!(lexical_fold("../../etc/passwd"), "../../etc/passwd");
        assert_eq!(lexical_fold("/etc/../etc/passwd"), "/etc/passwd");
    }

    #[test]
    fn workspace_errors_are_model_recoverable() {
        let output: crate::tool::ToolOutput<()> =
            workspace_error(WorkspaceError::MissingFileName {
                path: PathBuf::from("."),
            });

        assert!(!output.ok);
        assert_eq!(output.error.expect("error present").kind, "workspace_path");
    }
}
