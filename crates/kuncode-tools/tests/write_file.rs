mod support;

use kuncode_core::ToolCapability;
use kuncode_events::EventKind;
use kuncode_tools::{ToolError, ToolInput, ToolLimits, WriteFileTool};
use serde_json::json;
use tokio::fs;
use tokio_util::sync::CancellationToken;

use support::{ToolFixture, tool_kinds};

#[tokio::test]
async fn write_file_happy_path() {
    let f = ToolFixture::new().await;
    fs::create_dir(f.root().join("src")).await.expect("src");

    let result = f
        .run_tool(
            WriteFileTool::new(),
            ToolInput::new("write_file", json!({ "path": "src/new.rs", "content": "pub fn x() {}\n" })),
            &[ToolCapability::Edit],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect("write");

    assert_eq!(fs::read_to_string(f.root().join("src/new.rs")).await.expect("read"), "pub fn x() {}\n");
    assert_eq!(result.metadata["path"], "src/new.rs");
    assert_eq!(result.metadata["bytes"], 14);
}

#[tokio::test]
async fn write_file_capability_denied_before_lifecycle_events() {
    let f = ToolFixture::new().await;

    let err = f
        .run_tool(
            WriteFileTool::new(),
            ToolInput::new("write_file", json!({ "path": "new.rs", "content": "" })),
            &[ToolCapability::Explore],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("denied");

    assert!(matches!(err, ToolError::CapabilityDenied { .. }));
    assert!(tool_kinds(&f.drain().await).is_empty());
}

#[tokio::test]
async fn write_file_missing_parent_is_workspace_error() {
    let f = ToolFixture::new().await;

    let err = f
        .run_tool(
            WriteFileTool::new(),
            ToolInput::new("write_file", json!({ "path": "missing/new.rs", "content": "" })),
            &[ToolCapability::Edit],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("missing parent");

    assert!(matches!(err, ToolError::Workspace { .. }));
    let events = f.drain().await;
    assert_eq!(tool_kinds(&events), vec![EventKind::ToolStarted, EventKind::ToolFailed]);
}

#[tokio::test]
async fn write_file_path_escape_is_workspace_error() {
    let f = ToolFixture::new().await;

    let err = f
        .run_tool(
            WriteFileTool::new(),
            ToolInput::new("write_file", json!({ "path": "../escape.txt", "content": "" })),
            &[ToolCapability::Edit],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("escape");

    assert!(matches!(err, ToolError::Workspace { .. }));
}
