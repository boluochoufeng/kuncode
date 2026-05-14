mod support;

use kuncode_core::ToolCapability;
use kuncode_events::EventKind;
use kuncode_tools::{ReadFileTool, ToolError, ToolInput, ToolLimits};
use serde_json::json;
use tokio::fs;
use tokio_util::sync::CancellationToken;

use support::{ToolFixture, tool_kinds};

#[tokio::test]
async fn read_file_happy_path() {
    let f = ToolFixture::new().await;
    fs::create_dir(f.root().join("src")).await.expect("src");
    fs::write(f.root().join("src/lib.rs"), "fn main() {}\n").await.expect("write");

    let result = f
        .run_tool(
            ReadFileTool::new(),
            ToolInput::new("read_file", json!({ "path": "src/lib.rs" })),
            &[ToolCapability::Explore],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect("read");

    assert_eq!(result.inline_content.as_deref(), Some("fn main() {}\n"));
    assert_eq!(result.metadata["path"], "src/lib.rs");
    assert_eq!(result.metadata["truncated"], false);

    let events = f.drain().await;
    assert_eq!(tool_kinds(&events), vec![EventKind::ToolStarted, EventKind::ToolCompleted]);
}

#[tokio::test]
async fn read_file_truncates_inline_content() {
    let f = ToolFixture::new().await;
    fs::write(f.root().join("large.txt"), "abcdef").await.expect("write");
    let limits = ToolLimits { max_inline_output_bytes: 3, ..ToolLimits::default() };

    let result = f
        .run_tool(
            ReadFileTool::new(),
            ToolInput::new("read_file", json!({ "path": "large.txt" })),
            &[ToolCapability::Explore],
            limits,
            CancellationToken::new(),
        )
        .await
        .expect("read");

    assert_eq!(result.inline_content.as_deref(), Some("abc"));
    assert_eq!(result.metadata["truncated"], true);
}

#[tokio::test]
async fn read_file_can_return_line_range_with_line_numbers() {
    let f = ToolFixture::new().await;
    fs::write(f.root().join("notes.txt"), "alpha\nbeta\ngamma\ndelta\n").await.expect("write");

    let result = f
        .run_tool(
            ReadFileTool::new(),
            ToolInput::new("read_file", json!({ "path": "notes.txt", "offset": 2, "limit": 2 })),
            &[ToolCapability::Explore],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect("read range");

    assert_eq!(result.inline_content.as_deref(), Some("2 | beta\n3 | gamma\n"));
    assert_eq!(result.metadata["start_line"], 2);
    assert_eq!(result.metadata["end_line"], 3);
    assert_eq!(result.metadata["returned_lines"], 2);
    assert_eq!(result.metadata["total_lines"], 4);
    assert_eq!(result.metadata["bytes"], 23);
    assert_eq!(result.metadata["selected_bytes"], 11);
    assert_eq!(result.metadata["range_truncated"], true);
    assert_eq!(result.metadata["line_numbered"], true);
    assert_eq!(result.metadata["truncated"], false);
    assert_eq!(result.summary, "read notes.txt lines 2-3 of 4 (11 selected bytes, 23 file bytes)");
}

#[tokio::test]
async fn read_file_limit_without_offset_starts_at_first_line() {
    let f = ToolFixture::new().await;
    fs::write(f.root().join("notes.txt"), "alpha\nbeta\ngamma\n").await.expect("write");

    let result = f
        .run_tool(
            ReadFileTool::new(),
            ToolInput::new("read_file", json!({ "path": "notes.txt", "limit": 1 })),
            &[ToolCapability::Explore],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect("read range");

    assert_eq!(result.inline_content.as_deref(), Some("1 | alpha\n"));
    assert_eq!(result.metadata["start_line"], 1);
    assert_eq!(result.metadata["end_line"], 1);
    assert_eq!(result.metadata["returned_lines"], 1);
    assert_eq!(result.metadata["range_truncated"], true);
}

#[tokio::test]
async fn read_file_offset_past_eof_returns_empty_range_without_inverted_end_line() {
    let f = ToolFixture::new().await;
    fs::write(f.root().join("notes.txt"), "alpha\nbeta\n").await.expect("write");

    let result = f
        .run_tool(
            ReadFileTool::new(),
            ToolInput::new("read_file", json!({ "path": "notes.txt", "offset": 10, "limit": 3 })),
            &[ToolCapability::Explore],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect("read range");

    assert_eq!(result.inline_content.as_deref(), Some(""));
    assert_eq!(result.summary, "read notes.txt from line 10 (0 lines of 2)");
    assert_eq!(result.metadata["start_line"], 10);
    assert!(result.metadata["end_line"].is_null());
    assert_eq!(result.metadata["returned_lines"], 0);
    assert_eq!(result.metadata["total_lines"], 2);
    assert_eq!(result.metadata["range_truncated"], false);
}

#[tokio::test]
async fn read_file_rejects_range_limit_above_schema_max_before_lifecycle_events() {
    let f = ToolFixture::new().await;
    fs::write(f.root().join("notes.txt"), "alpha\n").await.expect("write");

    let err = f
        .run_tool(
            ReadFileTool::new(),
            ToolInput::new("read_file", json!({ "path": "notes.txt", "limit": 1001 })),
            &[ToolCapability::Explore],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("invalid limit");

    assert!(matches!(err, ToolError::InvalidInput { .. }));
    assert!(tool_kinds(&f.drain().await).is_empty());
}

#[tokio::test]
async fn read_file_capability_denied_before_lifecycle_events() {
    let f = ToolFixture::new().await;
    fs::write(f.root().join("a.txt"), "a").await.expect("write");

    let err = f
        .run_tool(
            ReadFileTool::new(),
            ToolInput::new("read_file", json!({ "path": "a.txt" })),
            &[ToolCapability::Verify],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("denied");

    assert!(matches!(err, ToolError::CapabilityDenied { .. }));
    let events = f.drain().await;
    assert!(tool_kinds(&events).is_empty());
}

#[tokio::test]
async fn read_file_missing_path_is_workspace_error() {
    let f = ToolFixture::new().await;

    let err = f
        .run_tool(
            ReadFileTool::new(),
            ToolInput::new("read_file", json!({ "path": "missing.txt" })),
            &[ToolCapability::Explore],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("missing");

    assert!(matches!(err, ToolError::Workspace { .. }));
    let events = f.drain().await;
    assert_eq!(tool_kinds(&events), vec![EventKind::ToolStarted, EventKind::ToolFailed]);
}
