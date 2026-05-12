//! Integration tests for `ToolRuntime`. Covers plan §13.3 plus §11.2's
//! invariant that lifecycle events are only emitted *after* lookup, schema
//! validation, and the capability gate all pass.

use async_trait::async_trait;
use futures_util::{StreamExt, pin_mut};
use kuncode_core::{EventId, RiskFlag, RunId, ToolCapability, ToolEffect};
use kuncode_events::{EventEnvelope, EventKind, EventLogReader, JsonlEventSink, RunDir, ToolStarted};
use kuncode_tools::{
    ExecArgvTool, RegisterError, Tool, ToolContext, ToolDescriptor, ToolError, ToolInput, ToolLimits, ToolResult,
    ToolRuntime,
};
use kuncode_workspace::{ExecutionLane, Workspace, WorkspaceConfig};
use serde_json::{Value, json};
use std::time::Duration;
use tempfile::{TempDir, tempdir};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

// --- Fixtures -------------------------------------------------------------

struct Fixture {
    _temp: TempDir,
    run_id: RunId,
    run_dir: RunDir,
    workspace: Workspace,
    lane: ExecutionLane,
    sink: JsonlEventSink,
}

async fn fixture() -> Fixture {
    let temp = tempdir().expect("tempdir");
    let workspace = Workspace::open(temp.path(), WorkspaceConfig::default()).await.expect("workspace");
    let lane = ExecutionLane::main(&workspace);
    let run_id = RunId::new();
    let run_dir = RunDir::create(temp.path(), run_id).await.expect("run_dir");
    let sink = JsonlEventSink::start(run_dir.clone()).await.expect("sink");
    Fixture { _temp: temp, run_id, run_dir, workspace, lane, sink }
}

fn make_ctx(f: &Fixture, cancel: CancellationToken) -> ToolContext<'_> {
    ToolContext {
        run_id: f.run_id,
        agent_id: None,
        turn_id: None,
        source_event_id: EventId::new(), // dummy; runtime overwrites before Tool::execute
        workspace: &f.workspace,
        lane: &f.lane,
        event_sink: f.sink.handle(),
        artifact_store: None,
        cancel_token: cancel,
        limits: ToolLimits::default(),
    }
}

/// Drain the sink, then collect every envelope from `events.jsonl`.
async fn drain(f: Fixture) -> Vec<EventEnvelope> {
    f.sink.shutdown().await.expect("shutdown");
    let reader = EventLogReader::for_run_dir(&f.run_dir);
    let stream = reader.stream();
    pin_mut!(stream);
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        out.push(item.expect("event"));
    }
    out
}

// --- Mock tool ------------------------------------------------------------

type FailFn = Box<dyn Fn() -> ToolError + Send + Sync>;

enum MockBehavior {
    Succeed(ToolResult),
    Fail(FailFn),
    WaitForCancel,
}

struct MockTool {
    descriptor: ToolDescriptor,
    behavior: MockBehavior,
}

#[async_trait]
impl Tool for MockTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn execute(&self, _input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        match &self.behavior {
            MockBehavior::Succeed(result) => Ok(result.clone()),
            MockBehavior::Fail(make) => Err(make()),
            MockBehavior::WaitForCancel => {
                ctx.cancel_token.cancelled().await;
                Err(ToolError::Cancelled { tool: self.descriptor.name.clone() })
            }
        }
    }
}

fn explore_descriptor(name: &str) -> ToolDescriptor {
    ToolDescriptor {
        name: name.to_owned(),
        description: "test tool".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
            "additionalProperties": false,
        }),
        output_schema: None,
        effects: vec![ToolEffect::ReadWorkspace],
        default_capabilities: vec![ToolCapability::Explore],
        risk_flags: vec![],
    }
}

fn ok_result(summary: &str) -> ToolResult {
    ToolResult { summary: summary.to_owned(), inline_content: None, content_ref: None, metadata: json!({}) }
}

fn payload() -> Value {
    json!({ "path": "src/lib.rs" })
}

fn tool_kinds(events: &[EventEnvelope]) -> Vec<EventKind> {
    events
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                EventKind::ToolStarted | EventKind::ToolCompleted | EventKind::ToolFailed | EventKind::ToolCancelled
            )
        })
        .map(|e| e.kind)
        .collect()
}

// --- Registration tests ---------------------------------------------------

#[tokio::test]
async fn register_rejects_duplicate_name() {
    let mut runtime = ToolRuntime::new();
    let first =
        MockTool { descriptor: explore_descriptor("read_file"), behavior: MockBehavior::Succeed(ok_result("ok")) };
    let second =
        MockTool { descriptor: explore_descriptor("read_file"), behavior: MockBehavior::Succeed(ok_result("ok")) };

    runtime.register(Box::new(first)).expect("first registers");
    let err = runtime.register(Box::new(second)).expect_err("duplicate must reject");
    assert!(matches!(err, RegisterError::DuplicateName { ref name } if name == "read_file"));
}

#[tokio::test]
async fn register_rejects_descriptor_with_empty_name() {
    let mut runtime = ToolRuntime::new();
    let mut descriptor = explore_descriptor("read_file");
    descriptor.name.clear();
    let tool = MockTool { descriptor, behavior: MockBehavior::Succeed(ok_result("ok")) };

    let err = runtime.register(Box::new(tool)).expect_err("must reject");
    assert!(matches!(err, RegisterError::Descriptor { .. }));
}

#[tokio::test]
async fn register_rejects_descriptor_with_uncompilable_schema() {
    let mut runtime = ToolRuntime::new();
    let mut descriptor = explore_descriptor("read_file");
    descriptor.input_schema = json!(42);
    let tool = MockTool { descriptor, behavior: MockBehavior::Succeed(ok_result("ok")) };

    let err = runtime.register(Box::new(tool)).expect_err("must reject");
    assert!(matches!(err, RegisterError::Schema { .. }));
}

#[tokio::test]
async fn descriptors_iterate_in_registration_order() {
    let mut runtime = ToolRuntime::new();
    for name in ["read_file", "search", "write_file", "apply_patch"] {
        runtime
            .register(Box::new(MockTool {
                descriptor: explore_descriptor(name),
                behavior: MockBehavior::Succeed(ok_result("ok")),
            }))
            .expect("register");
    }

    let names: Vec<&str> = runtime.descriptors().map(|descriptor| descriptor.name.as_str()).collect();
    assert_eq!(names, ["read_file", "search", "write_file", "apply_patch"]);
}

// --- Execute: pre-event-emission failure modes ----------------------------

#[tokio::test]
async fn execute_returns_unknown_tool_for_unregistered_name() {
    let f = fixture().await;
    let runtime = ToolRuntime::new();

    let result = runtime
        .execute(ToolInput::new("nope", payload()), make_ctx(&f, CancellationToken::new()), &[ToolCapability::Explore])
        .await;

    assert!(matches!(result, Err(ToolError::UnknownTool { ref name }) if name == "nope"));
    assert!(tool_kinds(&drain(f).await).is_empty(), "no lifecycle events before lookup passes");
}

#[tokio::test]
async fn execute_rejects_payload_failing_input_schema() {
    let f = fixture().await;
    let mut runtime = ToolRuntime::new();
    runtime
        .register(Box::new(MockTool {
            descriptor: explore_descriptor("read_file"),
            behavior: MockBehavior::Succeed(ok_result("ok")),
        }))
        .expect("register");

    let result = runtime
        .execute(
            ToolInput::new("read_file", json!({ "wrong": 1 })),
            make_ctx(&f, CancellationToken::new()),
            &[ToolCapability::Explore],
        )
        .await;

    assert!(matches!(result, Err(ToolError::InvalidInput { .. })));
    assert!(tool_kinds(&drain(f).await).is_empty(), "no lifecycle events before schema passes");
}

#[tokio::test]
async fn execute_denies_when_capability_intersection_empty() {
    let f = fixture().await;
    let mut runtime = ToolRuntime::new();
    runtime
        .register(Box::new(MockTool {
            descriptor: explore_descriptor("read_file"),
            behavior: MockBehavior::Succeed(ok_result("ok")),
        }))
        .expect("register");

    let result = runtime
        .execute(
            ToolInput::new("read_file", payload()),
            make_ctx(&f, CancellationToken::new()),
            &[ToolCapability::Edit], // disjoint with descriptor's [Explore]
        )
        .await;

    assert!(matches!(result, Err(ToolError::CapabilityDenied { .. })));
    assert!(tool_kinds(&drain(f).await).is_empty(), "no lifecycle events before gate passes");
}

// --- Execute: lifecycle ordering ------------------------------------------

#[tokio::test]
async fn successful_execute_emits_started_then_completed() {
    let f = fixture().await;
    let mut runtime = ToolRuntime::new();
    runtime
        .register(Box::new(MockTool {
            descriptor: explore_descriptor("read_file"),
            behavior: MockBehavior::Succeed(ok_result("read src/lib.rs (10 bytes)")),
        }))
        .expect("register");

    let input = ToolInput::new("read_file", payload());
    let request_id = input.request_id;
    let result = runtime
        .execute(input, make_ctx(&f, CancellationToken::new()), &[ToolCapability::Explore])
        .await
        .expect("execute ok");
    assert_eq!(result.summary, "read src/lib.rs (10 bytes)");

    let events = drain(f).await;
    assert_eq!(tool_kinds(&events), vec![EventKind::ToolStarted, EventKind::ToolCompleted]);
    let started: ToolStarted = serde_json::from_value(events[0].payload.clone()).expect("decode started");
    assert_eq!(started.tool_request_id, request_id);
    assert_eq!(started.tool_name, "read_file");
}

#[tokio::test]
async fn failing_tool_emits_started_then_failed() {
    let f = fixture().await;
    let mut runtime = ToolRuntime::new();
    runtime
        .register(Box::new(MockTool {
            descriptor: explore_descriptor("read_file"),
            behavior: MockBehavior::Fail(Box::new(|| ToolError::Workspace {
                path: "src/missing".into(),
                message: "not found".into(),
            })),
        }))
        .expect("register");

    let result = runtime
        .execute(
            ToolInput::new("read_file", payload()),
            make_ctx(&f, CancellationToken::new()),
            &[ToolCapability::Explore],
        )
        .await;
    assert!(matches!(result, Err(ToolError::Workspace { .. })));

    let events = drain(f).await;
    assert_eq!(tool_kinds(&events), vec![EventKind::ToolStarted, EventKind::ToolFailed]);
}

#[tokio::test]
async fn cancelled_tool_emits_started_then_cancelled() {
    let f = fixture().await;
    let mut runtime = ToolRuntime::new();
    runtime
        .register(Box::new(MockTool {
            descriptor: explore_descriptor("read_file"),
            behavior: MockBehavior::WaitForCancel,
        }))
        .expect("register");

    let cancel = CancellationToken::new();
    let cancel_for_trigger = cancel.clone();
    tokio::spawn(async move {
        sleep(Duration::from_millis(50)).await;
        cancel_for_trigger.cancel();
    });

    let result =
        runtime.execute(ToolInput::new("read_file", payload()), make_ctx(&f, cancel), &[ToolCapability::Explore]).await;
    assert!(matches!(result, Err(ToolError::Cancelled { .. })));

    let events = drain(f).await;
    assert_eq!(tool_kinds(&events), vec![EventKind::ToolStarted, EventKind::ToolCancelled]);
}

#[tokio::test]
async fn summary_over_limit_returns_result_too_large_and_emits_failed() {
    let f = fixture().await;
    let mut runtime = ToolRuntime::new();
    let oversized =
        ToolResult { summary: "x".repeat(500), inline_content: None, content_ref: None, metadata: json!({}) };
    runtime
        .register(Box::new(MockTool {
            descriptor: explore_descriptor("read_file"),
            behavior: MockBehavior::Succeed(oversized),
        }))
        .expect("register");

    let result = runtime
        .execute(
            ToolInput::new("read_file", payload()),
            make_ctx(&f, CancellationToken::new()),
            &[ToolCapability::Explore],
        )
        .await;
    assert!(matches!(result, Err(ToolError::ResultTooLarge { .. })));

    let events = drain(f).await;
    assert_eq!(tool_kinds(&events), vec![EventKind::ToolStarted, EventKind::ToolFailed]);
}

// --- ctx.source_event_id propagation --------------------------------------

struct CapturingTool {
    descriptor: ToolDescriptor,
    observed: std::sync::Arc<std::sync::Mutex<Option<EventId>>>,
}

#[async_trait]
impl Tool for CapturingTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn execute(&self, _input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        *self.observed.lock().unwrap() = Some(ctx.source_event_id);
        Ok(ToolResult { summary: "ok".into(), inline_content: None, content_ref: None, metadata: json!({}) })
    }
}

#[tokio::test]
async fn tool_started_event_id_is_threaded_into_ctx_source_event_id() {
    use std::sync::Arc;

    let f = fixture().await;

    // Capture the source_event_id the runtime hands to Tool::execute.
    let observed = Arc::new(std::sync::Mutex::new(None::<EventId>));
    let observed_for_tool = Arc::clone(&observed);

    let mut runtime = ToolRuntime::new();
    runtime
        .register(Box::new(CapturingTool { descriptor: explore_descriptor("read_file"), observed: observed_for_tool }))
        .expect("register");

    runtime
        .execute(
            ToolInput::new("read_file", payload()),
            make_ctx(&f, CancellationToken::new()),
            &[ToolCapability::Explore],
        )
        .await
        .expect("execute ok");

    let events = drain(f).await;
    let started = events.iter().find(|e| e.kind == EventKind::ToolStarted).expect("tool.started exists");
    let captured = observed.lock().unwrap().expect("tool saw a source_event_id");
    assert_eq!(captured, started.event_id, "ctx.source_event_id must equal tool.started event_id");
}

#[tokio::test]
async fn started_event_uses_static_risk_flags_for_trusted_exec() {
    let f = fixture().await;
    let mut runtime = ToolRuntime::new();
    runtime.register(Box::new(ExecArgvTool::new())).expect("register");

    runtime
        .execute(
            ToolInput::new("exec_argv", json!({ "argv": ["rustc", "--version"] })),
            make_ctx(&f, CancellationToken::new()),
            &[ToolCapability::Verify],
        )
        .await
        .expect("exec ok");

    let events = drain(f).await;
    let started: ToolStarted = serde_json::from_value(events[0].payload.clone()).expect("decode started");
    assert_eq!(started.risk_flags, vec![RiskFlag::LongRunning]);
}

#[tokio::test]
async fn started_event_adds_dynamic_untrusted_flag_for_untrusted_exec() {
    let f = fixture().await;
    let mut runtime = ToolRuntime::new();
    runtime.register(Box::new(ExecArgvTool::new())).expect("register");

    let result = runtime
        .execute(
            ToolInput::new("exec_argv", json!({ "argv": ["__kuncode_missing_command__"] })),
            make_ctx(&f, CancellationToken::new()),
            &[ToolCapability::Verify],
        )
        .await;
    assert!(matches!(result, Err(ToolError::Process { .. })));

    let events = drain(f).await;
    let started: ToolStarted = serde_json::from_value(events[0].payload.clone()).expect("decode started");
    assert_eq!(started.risk_flags, vec![RiskFlag::LongRunning, RiskFlag::UntrustedCommand]);
}
