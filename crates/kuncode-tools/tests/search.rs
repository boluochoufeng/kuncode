mod support;

use std::{process::Command, time::Duration};

use kuncode_core::ToolCapability;
use kuncode_events::EventKind;
use kuncode_tools::{SearchTool, ToolError, ToolInput, ToolLimits};
use serde_json::json;
use tokio::{fs, time::Instant};
use tokio_util::sync::CancellationToken;

use support::{ToolFixture, tool_kinds};

#[tokio::test]
async fn search_happy_path_uses_rust_fallback_when_rg_disabled() {
    let f = ToolFixture::new().await;
    fs::create_dir(f.root().join("src")).await.expect("src");
    fs::write(f.root().join("src/lib.rs"), "alpha\nneedle here\n").await.expect("write");

    let result = f
        .run_tool(
            SearchTool::with_rg_enabled(false),
            ToolInput::new("search", json!({ "query": "needle", "path": "src" })),
            &[ToolCapability::Explore],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect("search");

    assert!(result.inline_content.as_deref().expect("inline").contains("src/lib.rs:2:needle here"));
    assert_eq!(result.metadata["backend"], "rust");
    assert_eq!(result.metadata["matches"], 1);
}

#[tokio::test]
async fn search_max_results_marks_truncated() {
    let f = ToolFixture::new().await;
    fs::write(f.root().join("a.txt"), "needle 1\nneedle 2\nneedle 3\n").await.expect("write");

    let result = f
        .run_tool(
            SearchTool::with_rg_enabled(false),
            ToolInput::new("search", json!({ "query": "needle", "max_results": 2 })),
            &[ToolCapability::Explore],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect("search");

    assert_eq!(result.metadata["matches"], 2);
    assert_eq!(result.metadata["truncated"], true);
}

#[tokio::test]
async fn search_caps_long_snippets() {
    let f = ToolFixture::new().await;
    fs::write(f.root().join("a.txt"), format!("needle {}\n", "x".repeat(500))).await.expect("write");

    let result = f
        .run_tool(
            SearchTool::with_rg_enabled(false),
            ToolInput::new("search", json!({ "query": "needle" })),
            &[ToolCapability::Explore],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect("search");

    let inline = result.inline_content.as_deref().expect("inline");
    assert!(inline.len() <= 240);
    assert!(inline.ends_with("..."));
    assert_eq!(result.metadata["snippet_truncated"], true);
}

#[tokio::test]
async fn search_writes_artifact_when_inline_limit_is_exceeded() {
    let f = ToolFixture::new().await;
    fs::write(f.root().join("a.txt"), "needle one\nneedle two\n").await.expect("write");
    let limits = ToolLimits { max_inline_output_bytes: 8, ..ToolLimits::default() };

    let result = f
        .run_tool(
            SearchTool::with_rg_enabled(false),
            ToolInput::new("search", json!({ "query": "needle" })),
            &[ToolCapability::Explore],
            limits,
            CancellationToken::new(),
        )
        .await
        .expect("search");

    assert!(result.content_ref.is_some());
    assert_eq!(result.metadata["inline_truncated"], true);
    let artifact_id = result.content_ref.expect("artifact id");
    let artifact =
        fs::read_to_string(f.run_dir.artifacts_dir().join(format!("{artifact_id}.bin"))).await.expect("artifact");
    assert!(artifact.contains("a.txt:1:needle one"));
}

#[tokio::test]
async fn search_rg_backend_stops_after_inline_limit() {
    if Command::new("rg").arg("--version").output().is_err() {
        return;
    }

    let f = ToolFixture::new().await;
    fs::write(f.root().join("a.txt"), "needle one\nneedle two\nneedle three\n").await.expect("write");
    let limits = ToolLimits { max_inline_output_bytes: 8, ..ToolLimits::default() };
    let started = Instant::now();

    let result = f
        .run_tool(
            SearchTool::new(),
            ToolInput::new("search", json!({ "query": "needle" })),
            &[ToolCapability::Explore],
            limits,
            CancellationToken::new(),
        )
        .await
        .expect("search");

    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(result.metadata["backend"], "rg");
    assert_eq!(result.metadata["inline_truncated"], true);
}

#[tokio::test]
async fn search_capability_denied_before_lifecycle_events() {
    let f = ToolFixture::new().await;

    let err = f
        .run_tool(
            SearchTool::new(),
            ToolInput::new("search", json!({ "query": "needle" })),
            &[ToolCapability::Verify],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("denied");

    assert!(matches!(err, ToolError::CapabilityDenied { .. }));
    assert!(tool_kinds(&f.drain().await).is_empty());
}

#[tokio::test]
async fn search_invalid_regex_is_invalid_input() {
    let f = ToolFixture::new().await;

    let err = f
        .run_tool(
            SearchTool::with_rg_enabled(false),
            ToolInput::new("search", json!({ "query": "(" })),
            &[ToolCapability::Explore],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("invalid regex");

    assert!(matches!(err, ToolError::InvalidInput { .. }));
    let events = f.drain().await;
    assert_eq!(tool_kinds(&events), vec![EventKind::ToolStarted, EventKind::ToolFailed]);
}
