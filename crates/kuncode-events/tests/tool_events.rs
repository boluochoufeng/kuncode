//! Round-trip tests for the Phase 2 tool lifecycle events.
//!
//! Verifies that `tool.started` / `tool.completed` / `tool.failed` /
//! `tool.cancelled` envelopes serialize to the wire format documented in
//! `docs/plans/kuncode-phase2-tool-runtime-plan.md` §6.1 and survive a full
//! `JsonlEventSink` → `EventLogReader` round-trip.

use futures_util::{StreamExt, pin_mut};
use kuncode_core::{ArtifactId, RiskFlag, RunId, ToolEffect, ToolErrorKind, ToolRequestId};
use kuncode_events::{
    EventEnvelope, EventKind, EventLogError, EventLogReader, EventSink, JsonlEventSink, RunDir, ToolCancelled,
    ToolCompleted, ToolFailed, ToolStarted,
};
use tempfile::tempdir;

async fn collect_events(reader: EventLogReader) -> Vec<Result<EventEnvelope, EventLogError>> {
    let stream = reader.stream();
    pin_mut!(stream);
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        items.push(item);
    }
    items
}

#[tokio::test]
async fn tool_lifecycle_events_round_trip_through_jsonl() {
    let temp = tempdir().expect("tempdir");
    let run_id = RunId::new();
    let run_dir = RunDir::create(temp.path(), run_id).await.expect("create run dir");
    let sink = JsonlEventSink::start(run_dir.clone()).await.expect("start sink");
    let handle = sink.handle();

    let request_id = ToolRequestId::new();
    let started = ToolStarted {
        tool_request_id: request_id,
        tool_name: "read_file".to_owned(),
        effects: vec![ToolEffect::ReadWorkspace],
        risk_flags: vec![RiskFlag::MutatesWorkspace],
    };
    let completed = ToolCompleted {
        tool_request_id: request_id,
        tool_name: "read_file".to_owned(),
        summary: "read src/lib.rs (1024 bytes)".to_owned(),
        content_ref: Some(ArtifactId::new()),
    };
    let failed = ToolFailed {
        tool_request_id: ToolRequestId::new(),
        tool_name: "read_file".to_owned(),
        error_kind: ToolErrorKind::Workspace,
        summary: "path escapes workspace root".to_owned(),
    };
    let cancelled = ToolCancelled {
        tool_request_id: ToolRequestId::new(),
        tool_name: "exec_argv".to_owned(),
        summary: "cancelled".to_owned(),
    };

    let envelopes = [
        EventEnvelope::new(run_id, EventKind::ToolStarted, serde_json::to_value(&started).expect("serialize started")),
        EventEnvelope::new(
            run_id,
            EventKind::ToolCompleted,
            serde_json::to_value(&completed).expect("serialize completed"),
        ),
        EventEnvelope::new(run_id, EventKind::ToolFailed, serde_json::to_value(&failed).expect("serialize failed")),
        EventEnvelope::new(
            run_id,
            EventKind::ToolCancelled,
            serde_json::to_value(&cancelled).expect("serialize cancelled"),
        ),
    ];

    for envelope in &envelopes {
        handle.emit(envelope.clone()).await.expect("emit envelope");
    }
    sink.shutdown().await.expect("shutdown sink");

    let items = collect_events(EventLogReader::for_run_dir(&run_dir)).await;
    let parsed: Vec<EventEnvelope> = items.into_iter().map(|item| item.expect("event")).collect();
    assert_eq!(parsed.len(), envelopes.len());
    for (got, expected) in parsed.iter().zip(envelopes.iter()) {
        assert_eq!(got, expected);
    }

    let parsed_started: ToolStarted = serde_json::from_value(parsed[0].payload.clone()).expect("decode started");
    let parsed_completed: ToolCompleted = serde_json::from_value(parsed[1].payload.clone()).expect("decode completed");
    let parsed_failed: ToolFailed = serde_json::from_value(parsed[2].payload.clone()).expect("decode failed");
    let parsed_cancelled: ToolCancelled = serde_json::from_value(parsed[3].payload.clone()).expect("decode cancelled");

    assert_eq!(parsed_started, started);
    assert_eq!(parsed_completed, completed);
    assert_eq!(parsed_failed, failed);
    assert_eq!(parsed_cancelled, cancelled);
}
