//! Artifact storage: persist opaque byte blobs alongside a run's event log.
//!
//! Each artifact is written as `<artifact-id>.bin` under the run's `artifacts/`
//! directory. An `ArtifactRecord` is appended to `artifacts.jsonl` so the index
//! stays in sync with the files on disk.

use std::path::Path;

use async_trait::async_trait;
use kuncode_core::{ArtifactId, EventId, RunId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
};

use crate::{ArtifactError, RunDir};

/// Trait for saving artifact content and returning the recorded metadata.
#[async_trait]
pub trait ArtifactStore {
    /// Write `content` to the artifact store, returning the `ArtifactRecord`
    /// that is also appended to `artifacts.jsonl`.
    async fn save(
        &self,
        kind: String,
        source_event_id: EventId,
        content: &[u8],
    ) -> Result<ArtifactRecord, ArtifactError>;
}

/// Filesystem-backed artifact store rooted in a `RunDir`.
#[derive(Clone, Debug)]
pub struct FileArtifactStore {
    run_dir: RunDir,
}

impl FileArtifactStore {
    pub fn new(run_dir: RunDir) -> Self {
        Self { run_dir }
    }
}

#[async_trait]
impl ArtifactStore for FileArtifactStore {
    async fn save(
        &self,
        kind: String,
        source_event_id: EventId,
        content: &[u8],
    ) -> Result<ArtifactRecord, ArtifactError> {
        let artifact_id = ArtifactId::new();
        let size = u64::try_from(content.len())
            .map_err(|_| ArtifactError::SizeOverflow { size: content.len() })?;
        let mut hasher = Sha256::new();
        hasher.update(content);
        let sha256 = hex_lower(&hasher.finalize());
        let path = self.run_dir.artifacts_dir().join(format!("{artifact_id}.bin"));

        fs::write(&path, content)
            .await
            .map_err(|source| ArtifactError::Io { path: path.clone(), source })?;

        let record = ArtifactRecord {
            artifact_id,
            run_id: self.run_dir.run_id(),
            kind,
            size,
            sha256,
            source_event_id,
        };
        append_artifact_record(self.run_dir.artifacts_index_path(), &record).await?;
        Ok(record)
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

async fn append_artifact_record(path: &Path, record: &ArtifactRecord) -> Result<(), ArtifactError> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .map_err(|source| ArtifactError::Io { path: path.to_path_buf(), source })?;
    let json = serde_json::to_vec(record).map_err(|source| ArtifactError::Encode { source })?;
    file.write_all(&json)
        .await
        .map_err(|source| ArtifactError::Io { path: path.to_path_buf(), source })?;
    file.write_all(b"\n")
        .await
        .map_err(|source| ArtifactError::Io { path: path.to_path_buf(), source })?;
    file.flush().await.map_err(|source| ArtifactError::Io { path: path.to_path_buf(), source })
}

/// Metadata for one stored artifact, serialized as a single line in
/// `artifacts.jsonl`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub artifact_id: ArtifactId,
    pub run_id: RunId,
    pub kind: String,
    pub size: u64,
    pub sha256: String,
    pub source_event_id: EventId,
}
