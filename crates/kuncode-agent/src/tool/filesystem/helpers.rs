//! Path resolution, tree walking, and error shaping shared by the workspace
//! filesystem tools.
//!
//! Every item here has at least two tool callers (read / write / edit / glob /
//! ls). A helper used by a single tool lives beside that tool instead, so this
//! module stays the genuinely shared base.

use std::{ffi::OsStr, io, path::Path};

use ignore::{
    Match, WalkBuilder,
    gitignore::{Gitignore, GitignoreBuilder},
};

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

/// Builds a walker over `walk_root` honoring the project's own notion of noise.
///
/// Which paths are noise is delegated to the project rather than a hardcoded
/// name list: `.gitignore` / `.ignore` / `.git/info/exclude` are honored and
/// hidden dotfiles are skipped; `include_ignored` turns all of that off. The
/// user's global gitignore and ignore files *above the workspace* are
/// deliberately not consulted, so behavior is reproducible and scoped to the
/// workspace.
///
/// `.git` entries are never traversed. Note that the walker exempts its own
/// root from every filter — including this one — so a caller that lets
/// `walk_root` be user-supplied must reject a root inside the VCS store itself.
///
/// The returned walker is synchronous and thread-based; callers run it on the
/// blocking pool.
pub(super) fn workspace_walk_builder(
    workspace: &Workspace,
    walk_root: &Path,
    include_ignored: bool,
) -> WalkBuilder {
    let enabled = !include_ignored;
    // `WalkBuilder` reads ignore files only from the tree it walks, so a walk
    // rooted below the workspace would escape every rule the project declares
    // higher up. Supplying those rules separately keeps a subdirectory listing
    // filtered exactly like the same paths seen from the workspace root.
    let ancestors = if enabled && walk_root != workspace.root() {
        AncestorIgnore::between(workspace.root(), walk_root)
    } else {
        AncestorIgnore::default()
    };
    let mut builder = WalkBuilder::new(walk_root);
    builder
        .hidden(enabled)
        .git_ignore(enabled)
        .git_exclude(enabled)
        .ignore(enabled)
        .git_global(false)
        .parents(false)
        .require_git(false)
        .filter_entry(move |entry| {
            entry.file_name() != OsStr::new(".git")
                && !ancestors.is_ignored(
                    entry.path(),
                    entry
                        .file_type()
                        .is_some_and(|file_type| file_type.is_dir()),
                )
        });
    builder
}

/// Ignore rules declared between the workspace root and a deeper walk root.
///
/// Matchers are stored shallowest-first and consulted deepest-first, so the
/// closest rule wins as it does in git.
#[derive(Default)]
struct AncestorIgnore {
    matchers: Vec<Gitignore>,
}

impl AncestorIgnore {
    /// Collects the ignore files from `workspace_root` down to, but excluding,
    /// `walk_root` — the ones the walker itself will not read.
    fn between(workspace_root: &Path, walk_root: &Path) -> Self {
        let mut matchers = Vec::new();
        let Ok(relative) = walk_root.strip_prefix(workspace_root) else {
            return Self { matchers };
        };

        let mut directory = workspace_root.to_path_buf();
        let mut components = relative.components().peekable();
        loop {
            if let Some(matcher) = ignore_files_in(&directory, directory == workspace_root) {
                matchers.push(matcher);
            }
            match components.next() {
                // The walk root's own ignore files are the walker's job.
                Some(component) if components.peek().is_some() => directory.push(component),
                _ => break,
            }
        }

        Self { matchers }
    }

    /// Whether the project ignores `path`.
    ///
    /// Only `path` itself is matched, never its parents: a walk root named
    /// explicitly by the caller has its own ignored-ness waived, while what
    /// lives inside it stays filtered.
    fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        for matcher in self.matchers.iter().rev() {
            match matcher.matched(path, is_dir) {
                Match::None => {}
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
            }
        }
        false
    }
}

/// Compiles one directory's ignore files, or `None` when it declares no rules.
fn ignore_files_in(directory: &Path, is_workspace_root: bool) -> Option<Gitignore> {
    let mut builder = GitignoreBuilder::new(directory);
    // A missing file and a glob git itself would reject look the same here;
    // both mean "no rule to apply", which is how the walker treats them too.
    builder.add(directory.join(".gitignore"));
    builder.add(directory.join(".ignore"));
    if is_workspace_root {
        builder.add(directory.join(".git").join("info").join("exclude"));
    }
    builder.build().ok().filter(|matcher| !matcher.is_empty())
}

/// Whether a count is zero, for `serde(skip_serializing_if)`.
///
/// Counters that report what a listing had to leave out are noise in the
/// common case where nothing was left out, so they stay out of the payload
/// entirely until they say something.
pub(super) fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// Where a symlink inside the workspace points.
///
/// Symlinks are listed but never followed, so a tool decides for itself what to
/// advertise. The `canonicalize` cost lands only on links, which are rare, and
/// callers are already on the blocking pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SymlinkTarget {
    /// Resolves to a path under the workspace root.
    Inside,
    /// Resolves outside the workspace root, where the file tools refuse to act.
    Outside,
    /// Resolves nowhere — dangling, a cycle, or an unreadable parent. Such a
    /// link cannot reach outside the workspace either, since it reaches nothing.
    Unresolvable,
}

pub(super) fn symlink_target(path: &Path, workspace: &Workspace) -> SymlinkTarget {
    match std::fs::canonicalize(path) {
        Ok(target) if target.starts_with(workspace.root()) => SymlinkTarget::Inside,
        Ok(_) => SymlinkTarget::Outside,
        Err(_) => SymlinkTarget::Unresolvable,
    }
}

/// Whether `path` names the VCS store or something inside it.
///
/// The walk filter drops `.git` entries, but never its own root, so any tool
/// taking a caller-supplied directory has to reject one there itself.
pub(super) fn is_inside_vcs_store(workspace: &Workspace, path: &Path) -> bool {
    path.strip_prefix(workspace.root()).is_ok_and(|relative| {
        relative
            .components()
            .any(|component| component.as_os_str() == OsStr::new(".git"))
    })
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
