//! Event envelopes, JSONL event logs, run directories and artifact storage.
//!
//! This crate owns the durable event stream that every KunCode run produces.
//! The on-disk format is [JSONL](https://jsonlines.org/) — one JSON object per
//! line — written by a single background task (`JsonlEventSink`) and readable
//! via `EventLogReader`.
//!
//! # Layout
//!
//! ```text
//! $KUNCODE_HOME/runs/<run-id>/
//! ├── events.jsonl          ← append-only event stream
//! ├── artifacts.jsonl        ← artifact index (one ArtifactRecord per line)
//! ├── artifacts/             ← artifact content files (<artifact-id>.bin)
//! └── metadata.json          ← run metadata
//! ```
//!
//! See `docs/specs/kuncode-mvp-development-plan.md` §4.3 and §6.

mod artifact;
mod envelope;
mod error;
mod jsonl;
mod run_dir;

pub use artifact::{ArtifactRecord, ArtifactStore, FileArtifactStore};
pub use envelope::{EVENT_SCHEMA_VERSION, EventEnvelope, EventKind};
pub use error::{ArtifactError, EventLogError};
pub use jsonl::{EventLogReader, EventSink, EventSinkHandle, JsonlEventSink};
pub use run_dir::RunDir;
