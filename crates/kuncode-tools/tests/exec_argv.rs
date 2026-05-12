mod support;

use kuncode_core::ToolCapability;
use kuncode_events::{EventKind, ToolStarted};
use kuncode_tools::{ExecArgvTool, ToolError, ToolInput, ToolLimits};
use serde_json::json;
use tokio::{
    fs,
    time::{Duration, Instant, sleep},
};
use tokio_util::sync::CancellationToken;

use support::{ToolFixture, tool_kinds};

#[tokio::test]
async fn exec_argv_success() {
    let f = ToolFixture::new().await;

    let result = f
        .run_tool(
            ExecArgvTool::new(),
            ToolInput::new("exec_argv", json!({ "argv": ["rustc", "--version"] })),
            &[ToolCapability::Verify],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect("exec");

    assert_eq!(result.metadata["success"], true);
    assert!(result.inline_content.as_deref().expect("inline").contains("rustc"));
}

#[tokio::test]
async fn exec_argv_command_not_found_is_process_error() {
    let f = ToolFixture::new().await;

    let err = f
        .run_tool(
            ExecArgvTool::new(),
            ToolInput::new("exec_argv", json!({ "argv": ["__kuncode_missing_command__"] })),
            &[ToolCapability::Verify],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("missing command");

    assert!(matches!(err, ToolError::Process { .. }));
}

#[tokio::test]
async fn exec_argv_timeout_kills_process() {
    let f = ToolFixture::new().await;

    let err = f
        .run_tool(
            ExecArgvTool::new(),
            ToolInput::new("exec_argv", json!({ "argv": ["sh", "-c", "sleep 2"], "timeout_ms": 50 })),
            &[ToolCapability::Verify],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("timeout");

    assert!(matches!(err, ToolError::Timeout { .. }));
}

#[tokio::test]
async fn exec_argv_cancel_returns_within_five_seconds_and_emits_cancelled() {
    let f = ToolFixture::new().await;
    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();
    tokio::spawn(async move {
        sleep(Duration::from_millis(50)).await;
        cancel_for_task.cancel();
    });

    let started = Instant::now();
    let err = f
        .run_tool(
            ExecArgvTool::new(),
            ToolInput::new("exec_argv", json!({ "argv": ["sh", "-c", "sleep 5"] })),
            &[ToolCapability::Verify],
            ToolLimits::default(),
            cancel,
        )
        .await
        .expect_err("cancelled");

    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(matches!(err, ToolError::Cancelled { .. }));
    let events = f.drain().await;
    assert_eq!(tool_kinds(&events), vec![EventKind::ToolStarted, EventKind::ToolCancelled]);
}

#[tokio::test]
async fn exec_argv_long_stdout_writes_artifact_with_started_source_event() {
    let f = ToolFixture::new().await;
    let limits = ToolLimits { max_stdout_bytes: 4, max_inline_output_bytes: 4, ..ToolLimits::default() };

    let result = f
        .run_tool(
            ExecArgvTool::new(),
            ToolInput::new("exec_argv", json!({ "argv": ["sh", "-c", "printf abcdef"] })),
            &[ToolCapability::Verify],
            limits,
            CancellationToken::new(),
        )
        .await
        .expect("exec");

    assert!(result.content_ref.is_some());
    assert!(result.inline_content.as_deref().expect("inline").len() <= limits.max_inline_output_bytes);
    assert_eq!(result.metadata["stdout_bytes"], 6);
    assert_eq!(result.metadata["stderr_bytes"], 0);
    let records = f.artifact_records().await;
    assert_eq!(records.len(), 1);
    let artifact_id = result.content_ref.expect("artifact id");
    let artifact = fs::read(f.run_dir.artifacts_dir().join(format!("{artifact_id}.bin"))).await.expect("artifact");
    assert_eq!(artifact, b"stdout:\nabcdef\nstderr:\n");
    let events = f.drain().await;
    let started = events.iter().find(|event| event.kind == EventKind::ToolStarted).expect("started");
    assert_eq!(records[0].source_event_id, started.event_id);
}

#[tokio::test]
async fn exec_argv_long_stderr_writes_artifact() {
    let f = ToolFixture::new().await;
    let limits = ToolLimits { max_stderr_bytes: 4, max_inline_output_bytes: 4, ..ToolLimits::default() };

    let result = f
        .run_tool(
            ExecArgvTool::new(),
            ToolInput::new("exec_argv", json!({ "argv": ["sh", "-c", "printf abcdef >&2"] })),
            &[ToolCapability::Verify],
            limits,
            CancellationToken::new(),
        )
        .await
        .expect("exec");

    assert!(result.content_ref.is_some());
    assert_eq!(result.metadata["stdout_bytes"], 0);
    assert_eq!(result.metadata["stderr_bytes"], 6);
    let artifact_id = result.content_ref.expect("artifact id");
    let artifact = fs::read(f.run_dir.artifacts_dir().join(format!("{artifact_id}.bin"))).await.expect("artifact");
    assert_eq!(artifact, b"stdout:\n\nstderr:\nabcdef");
    assert_eq!(f.artifact_records().await.len(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn exec_argv_timeout_kills_process_group_children() {
    let f = ToolFixture::new().await;
    let survived = f.root().join("survived");

    let err = f
        .run_tool(
            ExecArgvTool::new(),
            ToolInput::new(
                "exec_argv",
                json!({ "argv": ["sh", "-c", "(sleep 1; touch survived) & wait"], "timeout_ms": 50 }),
            ),
            &[ToolCapability::Verify],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("timeout");

    assert!(matches!(err, ToolError::Timeout { .. }));
    sleep(Duration::from_millis(1200)).await;
    assert!(!survived.exists(), "background child should have been killed with the process group");
}

#[tokio::test]
async fn exec_argv_cwd_escape_is_workspace_error() {
    let f = ToolFixture::new().await;
    let outside = f.root().parent().expect("parent").join("outside");
    fs::create_dir(&outside).await.expect("outside");

    let err = f
        .run_tool(
            ExecArgvTool::new(),
            ToolInput::new("exec_argv", json!({ "argv": ["rustc", "--version"], "cwd": "../outside" })),
            &[ToolCapability::Verify],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("cwd escape");

    assert!(matches!(err, ToolError::Workspace { .. }));
}

#[tokio::test]
async fn exec_argv_started_records_dynamic_untrusted_risk_flag() {
    let f = ToolFixture::new().await;

    let _ = f
        .run_tool(
            ExecArgvTool::new(),
            ToolInput::new("exec_argv", json!({ "argv": ["__kuncode_missing_command__"] })),
            &[ToolCapability::Verify],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await;

    let events = f.drain().await;
    let started: ToolStarted = serde_json::from_value(events[0].payload.clone()).expect("tool.started payload");
    assert_eq!(started.risk_flags, vec![kuncode_core::RiskFlag::LongRunning, kuncode_core::RiskFlag::UntrustedCommand]);
}

#[tokio::test]
async fn exec_argv_started_records_only_static_risk_for_trusted_command() {
    let f = ToolFixture::new().await;

    f.run_tool(
        ExecArgvTool::new(),
        ToolInput::new("exec_argv", json!({ "argv": ["rustc", "--version"] })),
        &[ToolCapability::Verify],
        ToolLimits::default(),
        CancellationToken::new(),
    )
    .await
    .expect("exec");

    let events = f.drain().await;
    let started: ToolStarted = serde_json::from_value(events[0].payload.clone()).expect("tool.started payload");
    assert_eq!(started.risk_flags, vec![kuncode_core::RiskFlag::LongRunning]);
}
