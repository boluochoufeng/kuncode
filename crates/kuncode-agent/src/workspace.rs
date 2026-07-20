//! Workspace boundary checks for filesystem-backed tools.
//!
//! A workspace is the root directory the harness is allowed to inspect or
//! mutate. Tools resolve paths through this type so symlinks and `..` segments
//! cannot silently escape the configured root.

use std::{
    env, io,
    path::{Component, Path, PathBuf},
};

use thiserror::Error;

const MAX_DANGLING_SYMLINK_EXPANSIONS: usize = 40;

/// Filesystem root used by local tools.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Creates a workspace from the process current directory.
    ///
    /// The root is canonicalized immediately so later path checks compare
    /// against a stable absolute path.
    pub async fn from_current_dir() -> Result<Self, WorkspaceError> {
        let root = env::current_dir().map_err(WorkspaceError::CurrentDir)?;
        Self::new(root).await
    }

    /// Creates a workspace rooted at an existing directory.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] when the root cannot be resolved or is not a
    /// directory.
    pub async fn new(root: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        let input = root.as_ref();
        let root = tokio::fs::canonicalize(input).await.map_err(|source| {
            WorkspaceError::CanonicalizeRoot {
                path: input.to_path_buf(),
                source,
            }
        })?;

        let is_dir = tokio::fs::metadata(&root)
            .await
            .map(|meta| meta.is_dir())
            .unwrap_or(false);
        if !is_dir {
            return Err(WorkspaceError::RootNotDirectory { path: root });
        }

        Ok(Self { root })
    }

    /// Returns the canonical workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves an existing path and verifies that it stays under the root.
    ///
    /// Symlinks are resolved before the boundary check, so a link inside the
    /// workspace that points outside the root is rejected.
    pub async fn resolve_existing(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<PathBuf, WorkspaceError> {
        let candidate = self.candidate(path.as_ref());
        let resolved = tokio::fs::canonicalize(&candidate)
            .await
            .map_err(|source| WorkspaceError::CanonicalizePath {
                path: candidate.clone(),
                source,
            })?;

        self.ensure_inside(resolved)
    }

    /// Resolves the longest existing path prefix without requiring the final
    /// target to exist yet.
    ///
    /// This lets authorization happen before existence/type diagnostics, so a
    /// protected path does not reveal metadata merely by failing preparation.
    /// Existing symlinks are still resolved and checked against the workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing prefix escapes the workspace, a
    /// symbolic-link chain cannot be resolved safely, or a missing path
    /// contains parent traversal whose semantics would be ambiguous.
    pub async fn resolve_target(&self, path: impl AsRef<Path>) -> Result<PathBuf, WorkspaceError> {
        let input = path.as_ref();
        let mut candidate = self.candidate(input);
        let mut symlink_expansions = 0usize;

        'resolve: loop {
            match tokio::fs::canonicalize(&candidate).await {
                Ok(resolved) => return self.ensure_inside(resolved),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(WorkspaceError::CanonicalizePath {
                        path: candidate,
                        source,
                    });
                }
            }
            if candidate
                .components()
                .any(|component| component == Component::ParentDir)
            {
                return Err(WorkspaceError::AmbiguousMissingTraversal { path: candidate });
            }

            let mut existing = candidate.clone();
            let mut suffix = Vec::new();
            loop {
                match tokio::fs::canonicalize(&existing).await {
                    Ok(resolved) => {
                        let mut resolved = self.ensure_inside(resolved)?;
                        for component in suffix.iter().rev() {
                            resolved.push(component);
                        }
                        return Ok(resolved);
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        match tokio::fs::symlink_metadata(&existing).await {
                            Ok(metadata) if metadata.file_type().is_symlink() => {
                                if symlink_expansions >= MAX_DANGLING_SYMLINK_EXPANSIONS {
                                    return Err(WorkspaceError::SymbolicLinkLimit {
                                        path: candidate,
                                    });
                                }
                                let target =
                                    tokio::fs::read_link(&existing).await.map_err(|source| {
                                        WorkspaceError::ReadSymbolicLink {
                                            path: existing.clone(),
                                            source,
                                        }
                                    })?;
                                let mut expanded = if target.is_absolute() {
                                    target
                                } else {
                                    existing
                                        .parent()
                                        .ok_or_else(|| WorkspaceError::MissingParent {
                                            path: existing.clone(),
                                        })?
                                        .join(target)
                                };
                                for component in suffix.iter().rev() {
                                    expanded.push(component);
                                }
                                candidate = expanded;
                                symlink_expansions = symlink_expansions.saturating_add(1);
                                continue 'resolve;
                            }
                            Ok(_) => {
                                return Err(WorkspaceError::CanonicalizePath {
                                    path: existing,
                                    source: error,
                                });
                            }
                            Err(metadata_error)
                                if metadata_error.kind() == io::ErrorKind::NotFound => {}
                            Err(source) => {
                                return Err(WorkspaceError::CanonicalizePath {
                                    path: existing,
                                    source,
                                });
                            }
                        }
                        let Some(name) = existing.file_name().map(ToOwned::to_owned) else {
                            return Err(WorkspaceError::MissingFileName { path: candidate });
                        };
                        suffix.push(name);
                        if !existing.pop() {
                            return Err(WorkspaceError::MissingParent { path: candidate });
                        }
                    }
                    Err(source) => {
                        return Err(WorkspaceError::CanonicalizePath {
                            path: existing,
                            source,
                        });
                    }
                }
            }
        }
    }

    /// Re-resolves a possibly missing prepared target before resource access.
    ///
    /// # Errors
    /// Returns [`WorkspaceError::PathChanged`] when the current path resolves to
    /// a different target, or the normal resolution error when it is unsafe.
    pub async fn revalidate_target(
        &self,
        prepared: impl AsRef<Path>,
    ) -> Result<(), WorkspaceError> {
        let prepared = prepared.as_ref();
        let current = self.resolve_target(prepared).await?;
        if current == prepared {
            Ok(())
        } else {
            Err(WorkspaceError::PathChanged {
                prepared: prepared.to_path_buf(),
                current,
            })
        }
    }

    /// Displays paths under the workspace as relative paths.
    pub fn relative_display(&self, path: impl AsRef<Path>) -> String {
        let path = path.as_ref();
        match path.strip_prefix(&self.root) {
            Ok(relative) if relative.as_os_str().is_empty() => ".".to_string(),
            Ok(relative) => relative.display().to_string(),
            Err(_) => path.display().to_string(),
        }
    }

    fn candidate(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        }
    }

    fn ensure_inside(&self, path: PathBuf) -> Result<PathBuf, WorkspaceError> {
        if path.starts_with(&self.root) {
            Ok(path)
        } else {
            Err(WorkspaceError::EscapesRoot {
                path,
                root: self.root.clone(),
            })
        }
    }
}

/// Errors raised while resolving workspace paths.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    /// The process current directory could not be read.
    #[error("failed to read current directory: {0}")]
    CurrentDir(#[source] io::Error),

    /// The configured workspace root could not be canonicalized.
    #[error("failed to resolve workspace root `{path}`: {source}")]
    CanonicalizeRoot {
        /// Root path supplied by the caller.
        path: PathBuf,
        /// Filesystem error returned by canonicalization.
        #[source]
        source: io::Error,
    },

    /// The configured root resolved to a non-directory path.
    #[error("workspace root `{path}` is not a directory")]
    RootNotDirectory {
        /// Canonicalized root path.
        path: PathBuf,
    },

    /// A candidate path could not be canonicalized.
    #[error("failed to resolve path `{path}`: {source}")]
    CanonicalizePath {
        /// Path that failed to resolve.
        path: PathBuf,
        /// Filesystem error returned by canonicalization.
        #[source]
        source: io::Error,
    },

    /// A resolved path points outside the workspace root.
    #[error("path `{path}` escapes workspace root `{root}`")]
    EscapesRoot {
        /// Resolved path outside the workspace.
        path: PathBuf,
        /// Canonical workspace root.
        root: PathBuf,
    },

    /// A write target has no parent directory.
    #[error("path `{path}` has no parent directory")]
    MissingParent {
        /// Write target path.
        path: PathBuf,
    },

    /// A write target has no final file name.
    #[error("path `{path}` has no final file name")]
    MissingFileName {
        /// Write target path.
        path: PathBuf,
    },

    /// A path changed after permission preparation.
    #[error("prepared path `{prepared}` now resolves to `{current}`")]
    PathChanged {
        /// Path covered by the authorization request.
        prepared: PathBuf,
        /// Target observed immediately before execution.
        current: PathBuf,
    },

    /// A symbolic link could not be read during canonicalization.
    #[error("failed to read symbolic link `{path}`: {source}")]
    ReadSymbolicLink {
        /// Link whose target was unavailable.
        path: PathBuf,
        /// Filesystem error returned by `read_link`.
        #[source]
        source: io::Error,
    },

    /// A dangling-link chain exceeded the platform-style expansion limit.
    #[error("symbolic-link chain for `{path}` exceeds the safe expansion limit")]
    SymbolicLinkLimit {
        /// Original unresolved target.
        path: PathBuf,
    },

    /// Parent traversal after a missing component has platform-dependent
    /// symlink semantics and cannot be normalized safely.
    #[error("missing path `{path}` contains ambiguous parent traversal")]
    AmbiguousMissingTraversal {
        /// Rejected candidate path.
        path: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{Workspace, WorkspaceError};
    use crate::test_support::TestDir;

    #[tokio::test]
    async fn canonicalizes_workspace_root() {
        let tmp = TestDir::new();
        let root = tmp.path().join("root");
        fs::create_dir_all(&root).expect("root should be created");

        let workspace = Workspace::new(&root)
            .await
            .expect("workspace should be valid");

        assert_eq!(workspace.root(), root.canonicalize().expect("root exists"));
    }

    #[tokio::test]
    async fn resolves_existing_relative_paths_under_root() {
        let tmp = TestDir::new();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("src")).expect("root should be created");
        fs::write(root.join("src/lib.rs"), "pub fn ok() {}").expect("file should be written");
        let workspace = Workspace::new(&root)
            .await
            .expect("workspace should be valid");

        let resolved = workspace
            .resolve_existing("src/lib.rs")
            .await
            .expect("path should resolve");

        assert_eq!(
            resolved,
            root.join("src/lib.rs").canonicalize().expect("file exists")
        );
    }

    #[tokio::test]
    async fn rejects_parent_traversal_outside_root() {
        let tmp = TestDir::new();
        let root = tmp.path().join("root");
        fs::create_dir_all(&root).expect("root should be created");
        fs::write(tmp.path().join("outside.txt"), "outside").expect("outside file should exist");
        let workspace = Workspace::new(&root)
            .await
            .expect("workspace should be valid");

        let err = workspace
            .resolve_existing("../outside.txt")
            .await
            .expect_err("path should escape root");

        assert!(matches!(err, WorkspaceError::EscapesRoot { .. }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinks_that_escape_root() {
        use std::os::unix::fs::symlink;

        let tmp = TestDir::new();
        let root = tmp.path().join("root");
        fs::create_dir_all(&root).expect("root should be created");
        let outside = tmp.path().join("outside.txt");
        fs::write(&outside, "outside").expect("outside file should exist");
        symlink(&outside, root.join("link")).expect("symlink should be created");
        let workspace = Workspace::new(&root)
            .await
            .expect("workspace should be valid");

        let err = workspace
            .resolve_existing("link")
            .await
            .expect_err("symlink should escape root");

        assert!(matches!(err, WorkspaceError::EscapesRoot { .. }));
    }

    #[tokio::test]
    async fn resolves_missing_target_through_longest_existing_prefix() {
        let tmp = TestDir::new();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("src")).expect("root should be created");
        let workspace = Workspace::new(&root)
            .await
            .expect("workspace should be valid");

        let resolved = workspace
            .resolve_target("src/generated/nested.rs")
            .await
            .expect("missing target should retain a canonical workspace prefix");

        assert_eq!(
            resolved,
            root.canonicalize()
                .expect("root exists")
                .join("src/generated/nested.rs")
        );
    }

    #[tokio::test]
    async fn rejects_missing_absolute_target_outside_root() {
        let tmp = TestDir::new();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&root).expect("root should be created");
        fs::create_dir_all(&outside).expect("outside should be created");
        let workspace = Workspace::new(&root)
            .await
            .expect("workspace should be valid");

        let err = workspace
            .resolve_target(outside.join("missing.txt"))
            .await
            .expect_err("outside target should be rejected");

        assert!(matches!(err, WorkspaceError::EscapesRoot { .. }));
    }

    #[tokio::test]
    async fn rejects_parent_traversal_when_target_is_missing() {
        let tmp = TestDir::new();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("src")).expect("root should be created");
        let workspace = Workspace::new(&root)
            .await
            .expect("workspace should be valid");

        let err = workspace
            .resolve_target("src/missing/../file.rs")
            .await
            .expect_err("ambiguous missing traversal should be rejected");

        assert!(matches!(
            err,
            WorkspaceError::AmbiguousMissingTraversal { .. }
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_dangling_symlink_that_targets_outside_workspace() {
        use std::os::unix::fs::symlink;

        let tmp = TestDir::new();
        let root = tmp.path().join("root");
        fs::create_dir_all(&root).expect("root should be created");
        symlink(tmp.path().join("absent"), root.join("link"))
            .expect("dangling symlink should be created");
        let workspace = Workspace::new(&root)
            .await
            .expect("workspace should be valid");

        let err = workspace
            .resolve_target("link/file.txt")
            .await
            .expect_err("outside dangling prefix should be rejected");

        assert!(matches!(err, WorkspaceError::EscapesRoot { .. }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolves_dangling_symlink_to_a_missing_target_inside_workspace() {
        use std::os::unix::fs::symlink;

        let tmp = TestDir::new();
        let root = tmp.path().join("root");
        fs::create_dir_all(&root).expect("root should be created");
        symlink("generated", root.join("link")).expect("dangling symlink should be created");
        let workspace = Workspace::new(&root)
            .await
            .expect("workspace should be valid");

        let resolved = workspace
            .resolve_target("link/file.txt")
            .await
            .expect("inside dangling prefix should resolve to its intended target");

        assert_eq!(
            resolved,
            root.canonicalize()
                .expect("root exists")
                .join("generated/file.txt")
        );
    }

    #[tokio::test]
    async fn displays_paths_under_root_as_relative() {
        let tmp = TestDir::new();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("src")).expect("root should be created");
        let workspace = Workspace::new(&root)
            .await
            .expect("workspace should be valid");

        let display = workspace.relative_display(workspace.root().join("src/main.rs"));

        assert_eq!(display, "src/main.rs");
        assert_eq!(workspace.relative_display(workspace.root()), ".");
    }
}
