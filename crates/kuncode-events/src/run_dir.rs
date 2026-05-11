//! Run directory layout: the single source of truth for on-disk paths.
//!
//! No other crate may construct paths into `$KUNCODE_HOME/runs/<id>/` directly.
//! All access goes through `RunDir` so the layout stays consistent.

use std::{
    io,
    path::{Path, PathBuf},
};

use kuncode_core::RunId;
use tokio::fs::{self, OpenOptions};

use crate::EventLogError;

/// Represents the on-disk directory for a single run.
///
/// Created via [`RunDir::create`], which builds the directory tree:
///
/// ```text
/// runs/<id>/events.jsonl
/// runs/<id>/artifacts.jsonl
/// runs/<id>/artifacts/
/// runs/<id>/metadata.json
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunDir {
    home: PathBuf,
    run_id: RunId,
    path: PathBuf,
    events_path: PathBuf,
    artifacts_index_path: PathBuf,
    artifacts_dir: PathBuf,
    metadata_path: PathBuf,
}

impl RunDir {
    /// Create the run directory tree under `home/runs/<run_id>/`.
    ///
    /// Idempotent: if the directory already exists, its structure is reused.
    pub async fn create(home: impl AsRef<Path>, run_id: RunId) -> Result<Self, EventLogError> {
        let home = home.as_ref().to_path_buf();
        let path = home.join("runs").join(run_id.to_string());
        let artifacts_dir = path.join("artifacts");
        let events_path = path.join("events.jsonl");
        let artifacts_index_path = path.join("artifacts.jsonl");
        let metadata_path = path.join("metadata.json");

        fs::create_dir_all(&artifacts_dir)
            .await
            .map_err(|source| EventLogError::Io { path: artifacts_dir.clone(), source })?;
        touch_file(&events_path).await?;
        touch_file(&artifacts_index_path).await?;
        create_metadata(&metadata_path).await?;

        Ok(Self {
            home,
            run_id,
            path,
            events_path,
            artifacts_index_path,
            artifacts_dir,
            metadata_path,
        })
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn events_path(&self) -> &Path {
        &self.events_path
    }

    pub fn artifacts_index_path(&self) -> &Path {
        &self.artifacts_index_path
    }

    pub fn artifacts_dir(&self) -> &Path {
        &self.artifacts_dir
    }

    pub fn metadata_path(&self) -> &Path {
        &self.metadata_path
    }
}

async fn touch_file(path: &Path) -> Result<(), EventLogError> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .map_err(|source| EventLogError::Io { path: path.to_path_buf(), source })?;
    Ok(())
}

async fn create_metadata(path: &Path) -> Result<(), EventLogError> {
    match fs::metadata(path).await {
        Ok(_) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => fs::write(path, b"{}")
            .await
            .map_err(|source| EventLogError::Io { path: path.to_path_buf(), source }),
        Err(source) => Err(EventLogError::Io { path: path.to_path_buf(), source }),
    }
}
