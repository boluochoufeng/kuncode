use futures_util::{StreamExt, pin_mut};
use kuncode_core::{EventId, RunId};
use kuncode_events::{
    ArtifactRecord, ArtifactStore, EventEnvelope, EventKind, EventLogError, EventLogReader, EventSink,
    FileArtifactStore, JsonlEventSink, RunDir,
};
use serde_json::json;
use tempfile::tempdir;
use tokio::fs;

async fn collect_events(reader: EventLogReader) -> Vec<Result<EventEnvelope, EventLogError>> {
    let stream = reader.stream();
    pin_mut!(stream);

    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        items.push(item);
    }
    items
}

fn run_event(run_id: RunId, kind: EventKind) -> EventEnvelope {
    EventEnvelope::new(run_id, kind, json!({ "ok": true }))
}

#[tokio::test]
async fn run_dir_create_lays_out_phase_one_files() {
    let temp = tempdir().expect("tempdir");
    let run_id = RunId::new();
    let run_dir = RunDir::create(temp.path(), run_id).await.expect("create run dir");

    assert_eq!(run_dir.run_id(), run_id);
    assert!(run_dir.path().is_dir());
    assert!(run_dir.events_path().is_file());
    assert!(run_dir.artifacts_index_path().is_file());
    assert!(run_dir.artifacts_dir().is_dir());
    assert!(run_dir.metadata_path().is_file());
    assert!(!run_dir.path().join("taskboard.json").exists());
}

#[tokio::test]
async fn jsonl_event_sink_writes_events_in_order() {
    let temp = tempdir().expect("tempdir");
    let run_id = RunId::new();
    let run_dir = RunDir::create(temp.path(), run_id).await.expect("create run dir");
    let sink = JsonlEventSink::start(run_dir.clone()).await.expect("start sink");
    let handle = sink.handle();
    let first = run_event(run_id, EventKind::RunStarted);
    let second = run_event(run_id, EventKind::RunCompleted);

    handle.emit(first.clone()).await.expect("emit first");
    handle.emit(second.clone()).await.expect("emit second");
    sink.shutdown().await.expect("shutdown sink");

    let items = collect_events(EventLogReader::for_run_dir(&run_dir)).await;

    assert_eq!(items.len(), 2);
    assert_eq!(items[0].as_ref().expect("first event"), &first);
    assert_eq!(items[1].as_ref().expect("second event"), &second);
}

#[tokio::test]
async fn empty_event_log_streams_no_items() {
    let temp = tempdir().expect("tempdir");
    let run_dir = RunDir::create(temp.path(), RunId::new()).await.expect("create run dir");

    let items = collect_events(EventLogReader::for_run_dir(&run_dir)).await;

    assert!(items.is_empty());
}

#[tokio::test]
async fn file_artifact_store_writes_bytes_and_metadata() {
    let temp = tempdir().expect("tempdir");
    let run_id = RunId::new();
    let run_dir = RunDir::create(temp.path(), run_id).await.expect("create run dir");
    let store = FileArtifactStore::new(run_dir.clone());
    let source_event_id = EventId::new();
    let content = b"hello";

    let record = store.save("test".to_owned(), source_event_id, content).await.expect("save artifact");

    let artifact_path = run_dir.artifacts_dir().join(format!("{}.bin", record.artifact_id));
    let written = fs::read(artifact_path).await.expect("read artifact");
    let index = fs::read_to_string(run_dir.artifacts_index_path()).await.expect("read artifact index");
    let parsed: ArtifactRecord = serde_json::from_str(index.trim()).expect("parse record");

    assert_eq!(written, content);
    assert_eq!(record.run_id, run_id);
    assert_eq!(record.kind, "test");
    assert_eq!(record.size, 5);
    assert_eq!(record.sha256, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    assert_eq!(record.source_event_id, source_event_id);

    assert_eq!(parsed, record);
}

#[tokio::test]
async fn file_artifact_store_streams_file_and_metadata() {
    let temp = tempdir().expect("tempdir");
    let run_id = RunId::new();
    let run_dir = RunDir::create(temp.path(), run_id).await.expect("create run dir");
    let store = FileArtifactStore::new(run_dir.clone());
    let source_event_id = EventId::new();
    let source_path = temp.path().join("source.bin");
    let content = b"hello from a source file";
    fs::write(&source_path, content).await.expect("write source");

    let record =
        store.save_file("streamed".to_owned(), source_event_id, &source_path).await.expect("save file artifact");

    let artifact_path = run_dir.artifacts_dir().join(format!("{}.bin", record.artifact_id));
    let written = fs::read(artifact_path).await.expect("read artifact");
    let index = fs::read_to_string(run_dir.artifacts_index_path()).await.expect("read artifact index");
    let parsed: ArtifactRecord = serde_json::from_str(index.trim()).expect("parse record");

    assert_eq!(written, content);
    assert_eq!(record.run_id, run_id);
    assert_eq!(record.kind, "streamed");
    assert_eq!(record.size, u64::try_from(content.len()).expect("content size"));
    assert_eq!(record.sha256, "6a7487b547951fd787d75ef2d01d4be549fc97f6d8d550b54fe7508c71ecdc05");
    assert_eq!(record.source_event_id, source_event_id);
    assert_eq!(parsed, record);
}
