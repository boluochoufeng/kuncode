mod support;

use kuncode_core::ToolCapability;
use kuncode_events::EventKind;
use kuncode_tools::{GitDiffTool, GitStatusTool, ToolError, ToolInput, ToolLimits};
use serde_json::json;
use tokio::fs;
use tokio_util::sync::CancellationToken;

use support::{ToolFixture, git, tool_kinds};

#[tokio::test]
async fn git_status_happy_path() {
    let f = ToolFixture::new().await;
    git(f.root(), &["init"]);
    fs::write(f.root().join("new.txt"), "new\n").await.expect("write");

    let result = f
        .run_tool(
            GitStatusTool::new(),
            ToolInput::new("git_status", json!({})),
            &[ToolCapability::Explore],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect("status");

    assert!(result.inline_content.as_deref().expect("inline").contains("?? new.txt"));
    assert_eq!(result.metadata["changed_files"], 1);
}

#[tokio::test]
async fn git_status_counts_changed_files_from_full_output_when_inline_is_truncated() {
    let f = ToolFixture::new().await;
    git(f.root(), &["init"]);
    for idx in 0..5 {
        fs::write(f.root().join(format!("new-{idx}.txt")), "new\n").await.expect("write");
    }
    let limits = ToolLimits { max_inline_output_bytes: 8, ..ToolLimits::default() };

    let result = f
        .run_tool(
            GitStatusTool::new(),
            ToolInput::new("git_status", json!({})),
            &[ToolCapability::Explore],
            limits,
            CancellationToken::new(),
        )
        .await
        .expect("status");

    assert_eq!(result.metadata["changed_files"], 5);
    assert_eq!(result.metadata["truncated"], true);
    assert!(result.inline_content.as_deref().expect("inline").len() <= limits.max_inline_output_bytes);
}

#[tokio::test]
async fn git_diff_happy_path() {
    let f = ToolFixture::new().await;
    init_repo_with_file(&f).await;
    fs::write(f.root().join("tracked.txt"), "new\n").await.expect("modify");

    let result = f
        .run_tool(
            GitDiffTool::new(),
            ToolInput::new("git_diff", json!({ "path": "tracked.txt" })),
            &[ToolCapability::Verify],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect("diff");

    let inline = result.inline_content.as_deref().expect("inline");
    assert!(inline.contains("-old"));
    assert!(inline.contains("+new"));
}

#[tokio::test]
async fn git_diff_long_output_writes_artifact() {
    let f = ToolFixture::new().await;
    init_repo_with_file(&f).await;
    fs::write(f.root().join("tracked.txt"), "new line with many bytes\n").await.expect("modify");
    let limits = ToolLimits { max_inline_output_bytes: 8, ..ToolLimits::default() };

    let result = f
        .run_tool(
            GitDiffTool::new(),
            ToolInput::new("git_diff", json!({ "path": "tracked.txt" })),
            &[ToolCapability::Verify],
            limits,
            CancellationToken::new(),
        )
        .await
        .expect("diff");

    assert!(result.content_ref.is_some());
    assert_eq!(f.artifact_records().await.len(), 1);
}

#[tokio::test]
async fn git_status_capability_denied_before_lifecycle_events() {
    let f = ToolFixture::new().await;

    let err = f
        .run_tool(
            GitStatusTool::new(),
            ToolInput::new("git_status", json!({})),
            &[ToolCapability::Edit],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("denied");

    assert!(matches!(err, ToolError::CapabilityDenied { .. }));
    assert!(tool_kinds(&f.drain().await).is_empty());
}

#[tokio::test]
async fn git_status_non_repo_is_process_error() {
    let f = ToolFixture::new().await;

    let err = f
        .run_tool(
            GitStatusTool::new(),
            ToolInput::new("git_status", json!({})),
            &[ToolCapability::Explore],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("not repo");

    assert!(matches!(err, ToolError::Process { .. }));
    let events = f.drain().await;
    assert_eq!(tool_kinds(&events), vec![EventKind::ToolStarted, EventKind::ToolFailed]);
}

#[tokio::test]
async fn git_diff_path_escape_is_workspace_error() {
    let f = ToolFixture::new().await;
    git(f.root(), &["init"]);
    let outside = f.root().parent().expect("parent").join("outside");
    fs::create_dir(&outside).await.expect("outside");
    fs::write(outside.join("secret.txt"), "secret\n").await.expect("outside file");

    let err = f
        .run_tool(
            GitDiffTool::new(),
            ToolInput::new("git_diff", json!({ "path": "../outside/secret.txt" })),
            &[ToolCapability::Verify],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("escape");

    assert!(matches!(err, ToolError::Workspace { .. }));
}

async fn init_repo_with_file(f: &ToolFixture) {
    git(f.root(), &["init"]);
    fs::write(f.root().join("tracked.txt"), "old\n").await.expect("write tracked");
    git(f.root(), &["add", "tracked.txt"]);
    git(f.root(), &["commit", "-m", "initial"]);
}
