//! Workspace boundary checks for filesystem-backed tools.
//!
//! A workspace is the root directory the harness is allowed to inspect or
//! mutate. Tools resolve paths through this type so symlinks and `..` segments
//! cannot silently escape the configured root.

use std::{
    env, io,
    path::{Path, PathBuf},
};

use thiserror::Error;

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
        let resolved = tokio::fs::canonicalize(&candidate).await.map_err(|source| {
            WorkspaceError::CanonicalizePath {
                path: candidate.clone(),
                source,
            }
        })?;

        self.ensure_inside(resolved)
    }

    /// Resolves a path intended for writing.
    ///
    /// Existing targets are resolved like [`Self::resolve_existing`]. For new
    /// files, the parent directory must already exist and remain inside the
    /// workspace after canonicalization.
    pub async fn resolve_for_write(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<PathBuf, WorkspaceError> {
        let input = path.as_ref();
        let candidate = self.candidate(input);

        let file_name = candidate
            .file_name()
            .ok_or_else(|| WorkspaceError::MissingFileName {
                path: candidate.clone(),
            })?;

        match tokio::fs::symlink_metadata(&candidate).await {
            Ok(_) => self.resolve_existing(input).await,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let parent = candidate
                    .parent()
                    .ok_or_else(|| WorkspaceError::MissingParent {
                        path: candidate.clone(),
                    })?;
                let parent = tokio::fs::canonicalize(parent).await.map_err(|source| {
                    WorkspaceError::CanonicalizePath {
                        path: parent.to_path_buf(),
                        source,
                    }
                })?;

                let parent = self.ensure_inside(parent)?;
                Ok(parent.join(file_name))
            }
            Err(source) => Err(WorkspaceError::CanonicalizePath {
                path: candidate,
                source,
            }),
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
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{Workspace, WorkspaceError};

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "kuncode-workspace-test-{stamp}-{}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

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
    async fn resolves_new_write_target_under_existing_parent() {
        let tmp = TestDir::new();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("src")).expect("root should be created");
        let workspace = Workspace::new(&root)
            .await
            .expect("workspace should be valid");

        let resolved = workspace
            .resolve_for_write("src/new.rs")
            .await
            .expect("write target should resolve");

        assert_eq!(
            resolved,
            root.canonicalize().expect("root exists").join("src/new.rs")
        );
    }

    #[tokio::test]
    async fn rejects_new_write_target_with_missing_parent() {
        let tmp = TestDir::new();
        let root = tmp.path().join("root");
        fs::create_dir_all(&root).expect("root should be created");
        let workspace = Workspace::new(&root)
            .await
            .expect("workspace should be valid");

        let err = workspace
            .resolve_for_write("missing/new.rs")
            .await
            .expect_err("missing parent should fail");

        assert!(matches!(err, WorkspaceError::CanonicalizePath { .. }));
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
