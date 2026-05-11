//! Corruption recovery tests per §9.1.
//!
//! Verifies that `EventLogReader` can parse past damaged lines and continue
//! yielding subsequent events.

use futures_util::{StreamExt, pin_mut};
use kuncode_core::RunId;
use kuncode_events::{EventEnvelope, EventKind, EventLogError, EventLogReader, RunDir};
use serde_json::Value;
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
    EventEnvelope::new(run_id, kind, serde_json::json!({ "ok": true }))
}

#[tokio::test]
async fn truncated_json_line_reports_corruption_and_reader_continues() {
    let temp = tempdir().expect("tempdir");
    let run_id = RunId::new();
    let run_dir = RunDir::create(temp.path(), run_id).await.expect("create run dir");
    let first = run_event(run_id, EventKind::RunStarted);
    let second = run_event(run_id, EventKind::RunFailed);
    let content = format!(
        "{}\n{{\"schema_version\":\n{}\n",
        serde_json::to_string(&first).expect("serialize first"),
        serde_json::to_string(&second).expect("serialize second")
    );
    fs::write(run_dir.events_path(), content).await.expect("write event log");

    let items = collect_events(EventLogReader::for_run_dir(&run_dir)).await;

    assert_eq!(items.len(), 3);
    assert!(items[0].is_ok());
    assert!(matches!(items[1], Err(EventLogError::Corrupted { .. })));
    assert_eq!(items[2].as_ref().expect("second event"), &second);
}

#[tokio::test]
async fn unknown_kind_reports_classified_error_and_reader_continues() {
    let temp = tempdir().expect("tempdir");
    let run_id = RunId::new();
    let run_dir = RunDir::create(temp.path(), run_id).await.expect("create run dir");
    let unknown = run_event(run_id, EventKind::RunStarted);
    let valid = run_event(run_id, EventKind::RunCompleted);
    let mut unknown_value = serde_json::to_value(&unknown).expect("event to value");
    unknown_value["kind"] = Value::String("future.event".to_owned());
    let content = format!(
        "{}\n{}\n",
        serde_json::to_string(&unknown_value).expect("serialize unknown"),
        serde_json::to_string(&valid).expect("serialize valid")
    );
    fs::write(run_dir.events_path(), content).await.expect("write event log");

    let items = collect_events(EventLogReader::for_run_dir(&run_dir)).await;

    assert_eq!(items.len(), 2);
    assert!(matches!(
        &items[0],
        Err(EventLogError::UnknownKind { kind, .. }) if kind == "future.event"
    ));
    assert_eq!(items[1].as_ref().expect("valid event"), &valid);
}
