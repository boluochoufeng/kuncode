//! JSONL-based event log: single-writer sink and streaming reader.
//!
//! # Writer
//!
//! `JsonlEventSink` spawns a background Tokio task that owns the file handle.
//! Other components obtain an `EventSinkHandle` (cheaply clonable) and call
//! `emit` to send envelopes through an `mpsc` channel. Call `shutdown` to
//! flush and fsync before dropping.
//!
//! # Reader
//!
//! `EventLogReader::stream` returns an async stream of `EventEnvelope`. A line
//! that cannot be parsed produces `EventLogError::Corrupted` or
//! `UnknownKind`, but the stream continues with the next line.

use std::path::{Path, PathBuf};

use async_stream::stream;
use async_trait::async_trait;
use futures_core::Stream;
use serde_json::Value;
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::{EventEnvelope, EventKind, EventLogError, RunDir};

/// Trait for emitting events. Implemented by `EventSinkHandle` and `JsonlEventSink`.
#[async_trait]
pub trait EventSink {
    /// Append one event envelope to the log.
    async fn emit(&self, envelope: EventEnvelope) -> Result<(), EventLogError>;
}

/// A clonable handle to the writer task. This is what runtime components hold
/// to emit events.
#[derive(Clone)]
pub struct EventSinkHandle {
    sender: mpsc::Sender<WriterMessage>,
}

#[async_trait]
impl EventSink for EventSinkHandle {
    async fn emit(&self, envelope: EventEnvelope) -> Result<(), EventLogError> {
        self.sender.send(WriterMessage::Event(envelope)).await.map_err(|_| EventLogError::Closed)
    }
}

/// Background writer that appends `EventEnvelope`s to `events.jsonl`.
///
/// Created via [`JsonlEventSink::start`]. Drop the `EventSinkHandle` clones
/// or call [`JsonlEventSink::shutdown`] to drain and flush.
pub struct JsonlEventSink {
    sender: mpsc::Sender<WriterMessage>,
    join: JoinHandle<()>,
}

impl JsonlEventSink {
    /// Open (or create) `events.jsonl` inside `run_dir` and spawn the writer
    /// task. Returns the owning handle; call [`handle`](Self::handle) for
    /// cheaply clonable sender copies.
    pub async fn start(run_dir: RunDir) -> Result<Self, EventLogError> {
        let events_path = run_dir.events_path().to_path_buf();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&events_path)
            .await
            .map_err(|source| EventLogError::Io { path: events_path.clone(), source })?;

        let (sender, receiver) = mpsc::channel(128);
        let join_path = events_path.clone();
        let join = tokio::spawn(async move {
            writer_loop(file, join_path, receiver).await;
        });

        Ok(Self { sender, join })
    }

    /// Return a clonable handle that other components use to emit events.
    pub fn handle(&self) -> EventSinkHandle {
        EventSinkHandle { sender: self.sender.clone() }
    }

    /// Flush pending events, fsync the file, and join the writer task.
    /// Must be called before dropping to guarantee durability.
    pub async fn shutdown(self) -> Result<(), EventLogError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(WriterMessage::Shutdown(reply))
            .await
            .map_err(|_| EventLogError::Closed)?;

        let writer_result = result.await.map_err(|_| EventLogError::Closed)?;
        self.join.await.map_err(|source| EventLogError::Join { cause: source.to_string() })?;
        writer_result
    }
}

enum WriterMessage {
    Event(EventEnvelope),
    Shutdown(oneshot::Sender<Result<(), EventLogError>>),
}

async fn writer_loop(
    mut file: fs::File,
    path: PathBuf,
    mut receiver: mpsc::Receiver<WriterMessage>,
) {
    let mut pending_error = None;

    // TODO(durability): add configurable periodic fsync using both
    // fsync_every_n and fsync_every_ms thresholds, and force fsync after
    // terminal events such as run.completed/run.failed. Shutdown still drains
    // the queue and performs the final fsync.
    while let Some(message) = receiver.recv().await {
        match message {
            WriterMessage::Event(envelope) => {
                if pending_error.is_none() {
                    pending_error = write_event(&mut file, &path, &envelope).await.err();
                }
            }
            WriterMessage::Shutdown(reply) => {
                // Close the channel so cloned handles can no longer send, then
                // drain every message already queued before flushing.
                receiver.close();
                while let Some(message) = receiver.recv().await {
                    if let WriterMessage::Event(envelope) = message
                        && pending_error.is_none()
                    {
                        pending_error = write_event(&mut file, &path, &envelope).await.err();
                    }
                }
                let result = if let Some(error) = pending_error.take() {
                    Err(error)
                } else {
                    flush_and_sync(&mut file, &path).await
                };
                let _ = reply.send(result);
                return;
            }
        }
    }

    let _ = flush_and_sync(&mut file, &path).await;
}

async fn write_event(
    file: &mut fs::File,
    path: &Path,
    envelope: &EventEnvelope,
) -> Result<(), EventLogError> {
    let json = serde_json::to_vec(envelope).map_err(|source| EventLogError::Encode { source })?;
    file.write_all(&json)
        .await
        .map_err(|source| EventLogError::Io { path: path.to_path_buf(), source })?;
    file.write_all(b"\n")
        .await
        .map_err(|source| EventLogError::Io { path: path.to_path_buf(), source })?;
    Ok(())
}

async fn flush_and_sync(file: &mut fs::File, path: &Path) -> Result<(), EventLogError> {
    file.flush().await.map_err(|source| EventLogError::Io { path: path.to_path_buf(), source })?;
    file.sync_data().await.map_err(|source| EventLogError::Io { path: path.to_path_buf(), source })
}

/// Streaming reader for an `events.jsonl` file.
///
/// Created from a path or a `RunDir`. The `stream` method returns an async
/// stream that yields `Result<EventEnvelope, EventLogError>` per line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventLogReader {
    path: PathBuf,
}

impl EventLogReader {
    /// Create a reader pointed at an arbitrary `events.jsonl` path.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self { path: path.as_ref().to_path_buf() }
    }

    /// Convenience constructor from a `RunDir`.
    pub fn for_run_dir(run_dir: &RunDir) -> Self {
        Self::new(run_dir.events_path())
    }

    /// Return an async stream of parsed events.
    ///
    /// Malformed lines yield `Err(EventLogError::Corrupted)` or
    /// `Err(EventLogError::UnknownKind)`, but the stream continues with
    /// subsequent lines.
    pub fn stream(&self) -> impl Stream<Item = Result<EventEnvelope, EventLogError>> + '_ {
        let path = self.path.clone();

        stream! {
            let file = match fs::File::open(&path).await {
                Ok(file) => file,
                Err(source) => {
                    yield Err(EventLogError::Io { path: path.clone(), source });
                    return;
                }
            };
            let mut reader = BufReader::new(file);
            let mut offset = 0_u64;

            loop {
                let mut line = String::new();
                let bytes = match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(bytes) => bytes,
                    Err(source) => {
                        yield Err(EventLogError::Io { path: path.clone(), source });
                        break;
                    }
                };

                let line_offset = offset;
                offset += u64::try_from(bytes).unwrap_or(u64::MAX);
                let raw_line = line.trim_end_matches(['\r', '\n']).to_owned();
                yield parse_event_line(line_offset, &raw_line);
            }
        }
    }
}

fn parse_event_line(offset: u64, line: &str) -> Result<EventEnvelope, EventLogError> {
    let value: Value = serde_json::from_str(line).map_err(|source| EventLogError::Corrupted {
        offset,
        line: line.to_owned(),
        cause: source.to_string(),
    })?;

    let kind =
        value.get("kind").and_then(Value::as_str).ok_or_else(|| EventLogError::Corrupted {
            offset,
            line: line.to_owned(),
            cause: "missing string field `kind`".to_owned(),
        })?;

    if EventKind::from_wire(kind).is_none() {
        return Err(EventLogError::UnknownKind {
            offset,
            line: line.to_owned(),
            kind: kind.to_owned(),
        });
    }

    serde_json::from_value(value).map_err(|source| EventLogError::Corrupted {
        offset,
        line: line.to_owned(),
        cause: source.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuncode_core::RunId;
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn shutdown_drains_events_queued_after_shutdown_message() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("test.jsonl");
        let file =
            OpenOptions::new().create(true).append(true).open(&path).await.expect("create file");

        let (sender, receiver) = mpsc::channel(128);
        let run_id = RunId::new();
        let first = EventEnvelope::new(run_id, EventKind::RunStarted, json!({ "seq": 1 }));
        let second = EventEnvelope::new(run_id, EventKind::RunCompleted, json!({ "seq": 2 }));

        let (reply_tx, reply_rx) = oneshot::channel();

        // Directly construct the queue order: Event, Shutdown, Event.
        // The second Event sits after Shutdown in the channel — exactly the
        // scenario the old code dropped and the drain fix must preserve.
        sender.send(WriterMessage::Event(first.clone())).await.expect("send first");
        sender.send(WriterMessage::Shutdown(reply_tx)).await.expect("send shutdown");
        sender.send(WriterMessage::Event(second.clone())).await.expect("send second");
        // Drop the sender so the drain loop terminates after close.
        drop(sender);

        writer_loop(file, path.clone(), receiver).await;

        // Shutdown must have flushed successfully.
        reply_rx.await.expect("reply").expect("flush");

        // Both events must be on disk.
        let mut buf = Vec::new();
        fs::File::open(&path).await.expect("open").read_to_end(&mut buf).await.expect("read");

        let content = String::from_utf8(buf).expect("utf8");
        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "expected 2 events, got: {content}");

        let parsed_first: EventEnvelope = serde_json::from_str(lines[0]).expect("parse first");
        let parsed_second: EventEnvelope = serde_json::from_str(lines[1]).expect("parse second");
        assert_eq!(parsed_first.event_id, first.event_id);
        assert_eq!(parsed_second.event_id, second.event_id);
    }
}
