//! Error types for the event log and artifact store.

use std::{io, path::PathBuf};

use thiserror::Error;

/// Errors that can occur while writing to or reading from the event log.
#[derive(Debug, Error)]
pub enum EventLogError {
    #[error("event log IO error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("event log encoding error: {source}")]
    Encode {
        #[source]
        source: serde_json::Error,
    },
    #[error("corrupted event log line at offset {offset}: {cause}")]
    Corrupted { offset: u64, line: String, cause: String },
    #[error("unknown event kind at offset {offset}: {kind}")]
    UnknownKind { offset: u64, line: String, kind: String },
    #[error("event log writer is closed")]
    Closed,
    #[error("event log writer task failed: {cause}")]
    Join { cause: String },
}

/// Errors that can occur while persisting artifacts.
#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("artifact IO error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("artifact encoding error: {source}")]
    Encode {
        #[source]
        source: serde_json::Error,
    },
    #[error("artifact content too large to represent: {size} bytes")]
    SizeOverflow { size: usize },
}
