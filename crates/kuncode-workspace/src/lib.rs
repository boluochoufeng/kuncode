//! Workspace path safety and execution lane primitives.
//!
//! The `Workspace` type owns a canonical root directory and enforces that every
//! resolved `WorkspacePath` stays inside it. All write-facing tools must
//! receive a `WorkspacePath`, not a raw `&Path`, so path-escape attacks are
//! caught at the type level.
//!
//! MVP supports only the `MainWorkspace` lane; the `Worktree` lane is deferred.
//!
//! See `docs/specs/kuncode-mvp-development-plan.md` §4.4.

use std::{
    ffi::OsStr,
    io,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::fs;

/// Workspace-wide limits and behaviour toggles.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub max_file_size: u64,
    pub reject_binary: bool,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self { max_file_size: 1024 * 1024, reject_binary: true }
    }
}

/// A workspace rooted at a canonical directory. All path resolution goes
/// through this type so that every result is guaranteed to stay within the
/// workspace boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Workspace {
    root: PathBuf,
    config: WorkspaceConfig,
}

impl Workspace {
    /// Open a workspace at `root`. The path is canonicalized immediately, so
    /// the caller must ensure the directory exists.
    pub async fn open(
        root: impl AsRef<Path>,
        config: WorkspaceConfig,
    ) -> Result<Self, WorkspaceError> {
        let root = root.as_ref();
        let canonical_root = fs::canonicalize(root)
            .await
            .map_err(|source| WorkspaceError::IoError { path: root.to_path_buf(), source })?;

        Ok(Self { root: canonical_root, config })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> &WorkspaceConfig {
        &self.config
    }

    /// Resolve `path` for a read operation.
    ///
    /// Enforces: canonical path is inside root, target is a regular file,
    /// size within `max_file_size`, and — if `reject_binary` — content is
    /// valid UTF-8 without null bytes.
    pub async fn resolve_read_file(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<WorkspacePath, WorkspaceError> {
        let candidate = self.candidate(path.as_ref());
        let canonical = Self::canonicalize_existing(&candidate).await?;
        self.ensure_within_root(&candidate, &canonical).await?;

        let metadata = fs::metadata(&canonical)
            .await
            .map_err(|source| WorkspaceError::IoError { path: canonical.clone(), source })?;
        if !metadata.is_file() {
            return Err(WorkspaceError::NotFound { path: canonical });
        }

        if metadata.len() > self.config.max_file_size {
            return Err(WorkspaceError::TooLarge {
                path: canonical,
                size: metadata.len(),
                max: self.config.max_file_size,
            });
        }

        if self.config.reject_binary {
            let content = fs::read(&canonical)
                .await
                .map_err(|source| WorkspaceError::IoError { path: canonical.clone(), source })?;
            if content.contains(&0) || std::str::from_utf8(&content).is_err() {
                return Err(WorkspaceError::Binary { path: canonical });
            }
        }

        self.workspace_path(canonical)
    }

    /// Resolve `path` for a write operation.
    ///
    /// If the file already exists, its canonical location is verified against
    /// the root. If it does not yet exist, the *parent* directory is
    /// canonicalized and checked instead.
    pub async fn resolve_write_path(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<WorkspacePath, WorkspaceError> {
        let candidate = self.candidate(path.as_ref());

        match fs::symlink_metadata(&candidate).await {
            Ok(_) => {
                let canonical = Self::canonicalize_existing(&candidate).await?;
                self.ensure_within_root(&candidate, &canonical).await?;
                return self.workspace_path(canonical);
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(WorkspaceError::IoError { path: candidate.clone(), source });
            }
        }

        let parent = candidate
            .parent()
            .ok_or_else(|| WorkspaceError::NotFound { path: candidate.clone() })?;
        let parent_canonical = Self::canonicalize_existing(parent).await?;
        self.ensure_within_root(parent, &parent_canonical).await?;

        let file_name = candidate
            .file_name()
            .ok_or_else(|| WorkspaceError::NotFound { path: candidate.clone() })?;
        self.workspace_path(parent_canonical.join(file_name))
    }

    /// Returns `true` if the path contains a component that should be skipped
    /// by default (`.git`, `target`, `node_modules`, `.venv`).
    pub fn is_default_ignored(&self, path: impl AsRef<Path>) -> bool {
        path.as_ref().components().any(|component| {
            matches!(
                component,
                Component::Normal(name) if is_default_ignored_component(name)
            )
        })
    }

    fn candidate(&self, path: &Path) -> PathBuf {
        if path.is_absolute() { path.to_path_buf() } else { self.root.join(path) }
    }

    async fn canonicalize_existing(path: &Path) -> Result<PathBuf, WorkspaceError> {
        fs::canonicalize(path).await.map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                WorkspaceError::NotFound { path: path.to_path_buf() }
            } else {
                WorkspaceError::IoError { path: path.to_path_buf(), source }
            }
        })
    }

    async fn ensure_within_root(
        &self,
        original: &Path,
        canonical: &Path,
    ) -> Result<(), WorkspaceError> {
        if canonical.starts_with(&self.root) {
            return Ok(());
        }

        if has_symlink_component(original).await {
            return Err(WorkspaceError::SymlinkEscape {
                path: original.to_path_buf(),
                target: canonical.to_path_buf(),
                root: self.root.clone(),
            });
        }

        Err(WorkspaceError::PathEscape { path: canonical.to_path_buf(), root: self.root.clone() })
    }

    fn workspace_path(&self, absolute: PathBuf) -> Result<WorkspacePath, WorkspaceError> {
        let relative = absolute
            .strip_prefix(&self.root)
            .map_err(|_| WorkspaceError::PathEscape {
                path: absolute.clone(),
                root: self.root.clone(),
            })?
            .to_path_buf();

        Ok(WorkspacePath { absolute, relative })
    }
}

/// A path guaranteed to be inside the workspace root.
///
/// Carries both the absolute filesystem path and the workspace-relative path
/// so consumers never need to strip prefixes manually.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspacePath {
    absolute: PathBuf,
    relative: PathBuf,
}

impl WorkspacePath {
    pub fn as_path(&self) -> &Path {
        &self.absolute
    }

    pub fn relative_path(&self) -> &Path {
        &self.relative
    }
}

/// An execution lane — the directory scope within which tools operate.
///
/// MVP only provides `MainWorkspace`; future phases will add `Worktree`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionLane {
    kind: LaneKind,
    root_path: PathBuf,
}

impl ExecutionLane {
    pub fn main(workspace: &Workspace) -> Self {
        Self { kind: LaneKind::MainWorkspace, root_path: workspace.root().to_path_buf() }
    }

    pub fn kind(&self) -> LaneKind {
        self.kind
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }
}

/// Discriminant for the kind of execution lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneKind {
    MainWorkspace,
}

/// Errors produced by workspace path resolution and safety checks.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("path escapes workspace root: {path} outside {root}")]
    PathEscape { path: PathBuf, root: PathBuf },
    #[error("symlink escapes workspace root: {path} -> {target} outside {root}")]
    SymlinkEscape { path: PathBuf, target: PathBuf, root: PathBuf },
    #[error("file is too large: {path} is {size} bytes, max {max} bytes")]
    TooLarge { path: PathBuf, size: u64, max: u64 },
    #[error("binary or non-UTF-8 file rejected: {path}")]
    Binary { path: PathBuf },
    #[error("path not found: {path}")]
    NotFound { path: PathBuf },
    #[error("workspace IO error at {path}: {source}")]
    IoError {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn is_default_ignored_component(name: &OsStr) -> bool {
    [".git", "target", "node_modules", ".venv"].iter().any(|ignored| name == *ignored)
}

async fn has_symlink_component(path: &Path) -> bool {
    let mut current = PathBuf::new();

    for component in path.components() {
        current.push(component.as_os_str());
        if fs::symlink_metadata(&current)
            .await
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return true;
        }
    }

    false
}
