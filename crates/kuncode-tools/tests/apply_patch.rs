mod support;

use kuncode_core::ToolCapability;
use kuncode_events::EventKind;
use kuncode_tools::{ApplyPatchTool, ToolError, ToolInput, ToolLimits};
use serde_json::json;
use tokio::fs;
use tokio_util::sync::CancellationToken;

use support::{ToolFixture, tool_kinds};

#[tokio::test]
async fn apply_patch_modifies_existing_file() {
    let f = ToolFixture::new().await;
    fs::create_dir(f.root().join("src")).await.expect("src");
    fs::write(f.root().join("src/lib.rs"), "old\nsame\n").await.expect("write");
    let patch = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,2 @@
-old
+new
 same
";

    let result = f
        .run_tool(
            ApplyPatchTool::new(),
            ToolInput::new("apply_patch", json!({ "patch": patch })),
            &[ToolCapability::Edit],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect("apply");

    assert_eq!(fs::read_to_string(f.root().join("src/lib.rs")).await.expect("read"), "new\nsame\n");
    assert_eq!(result.metadata["touched_count"], 1);
}

#[tokio::test]
async fn apply_patch_creates_new_file() {
    let f = ToolFixture::new().await;
    fs::create_dir(f.root().join("src")).await.expect("src");
    let patch = "\
--- /dev/null
+++ b/src/new.txt
@@ -0,0 +1,2 @@
+hello
+world
";

    f.run_tool(
        ApplyPatchTool::new(),
        ToolInput::new("apply_patch", json!({ "patch": patch })),
        &[ToolCapability::Edit],
        ToolLimits::default(),
        CancellationToken::new(),
    )
    .await
    .expect("apply");

    assert_eq!(fs::read_to_string(f.root().join("src/new.txt")).await.expect("read"), "hello\nworld\n");
}

#[tokio::test]
async fn apply_patch_capability_denied_before_lifecycle_events() {
    let f = ToolFixture::new().await;

    let err = f
        .run_tool(
            ApplyPatchTool::new(),
            ToolInput::new("apply_patch", json!({ "patch": "--- a\n+++ b\n@@ -1 +1 @@\n-a\n+b\n" })),
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
async fn apply_patch_rejects_delete_patch() {
    let f = ToolFixture::new().await;
    fs::write(f.root().join("a.txt"), "old\n").await.expect("write");
    let patch = "\
--- a/a.txt
+++ /dev/null
@@ -1 +0,0 @@
-old
";

    let err = f
        .run_tool(
            ApplyPatchTool::new(),
            ToolInput::new("apply_patch", json!({ "patch": patch })),
            &[ToolCapability::Edit],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("delete rejected");

    assert!(matches!(err, ToolError::InvalidInput { .. }));
}

#[tokio::test]
async fn apply_patch_rejects_context_mismatch() {
    let f = ToolFixture::new().await;
    fs::write(f.root().join("a.txt"), "actual\n").await.expect("write");
    let patch = "\
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-expected
+new
";

    let err = f
        .run_tool(
            ApplyPatchTool::new(),
            ToolInput::new("apply_patch", json!({ "patch": patch })),
            &[ToolCapability::Edit],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("mismatch");

    assert!(matches!(err, ToolError::Process { .. }));
    let events = f.drain().await;
    assert_eq!(tool_kinds(&events), vec![EventKind::ToolStarted, EventKind::ToolFailed]);
}

#[tokio::test]
async fn apply_patch_does_not_partially_write_when_later_file_fails_validation() {
    let f = ToolFixture::new().await;
    fs::write(f.root().join("a.txt"), "old a\n").await.expect("write a");
    fs::write(f.root().join("b.txt"), "actual b\n").await.expect("write b");
    let patch = "\
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-old a
+new a
--- a/b.txt
+++ b/b.txt
@@ -1 +1 @@
-expected b
+new b
";

    let err = f
        .run_tool(
            ApplyPatchTool::new(),
            ToolInput::new("apply_patch", json!({ "patch": patch })),
            &[ToolCapability::Edit],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("mismatch");

    assert!(matches!(err, ToolError::Process { .. }));
    assert_eq!(fs::read_to_string(f.root().join("a.txt")).await.expect("read a"), "old a\n");
    assert_eq!(fs::read_to_string(f.root().join("b.txt")).await.expect("read b"), "actual b\n");
}

#[tokio::test]
async fn apply_patch_duplicate_target_does_not_write_first_change() {
    let f = ToolFixture::new().await;
    fs::write(f.root().join("a.txt"), "one\ntwo\n").await.expect("write");
    let patch = "\
--- a/a.txt
+++ b/a.txt
@@ -1,2 +1,2 @@
-one
+ONE
 two
--- a/a.txt
+++ b/a.txt
@@ -1,2 +1,2 @@
 one
-two
+TWO
";

    let err = f
        .run_tool(
            ApplyPatchTool::new(),
            ToolInput::new("apply_patch", json!({ "patch": patch })),
            &[ToolCapability::Edit],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("duplicate");

    assert!(matches!(err, ToolError::Process { .. }));
    assert_eq!(fs::read_to_string(f.root().join("a.txt")).await.expect("read"), "one\ntwo\n");
}

#[tokio::test]
async fn apply_patch_new_file_is_not_left_when_later_patch_fails() {
    let f = ToolFixture::new().await;
    fs::write(f.root().join("existing.txt"), "actual\n").await.expect("write existing");
    let patch = "\
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+created
--- a/existing.txt
+++ b/existing.txt
@@ -1 +1 @@
-expected
+new
";

    let err = f
        .run_tool(
            ApplyPatchTool::new(),
            ToolInput::new("apply_patch", json!({ "patch": patch })),
            &[ToolCapability::Edit],
            ToolLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect_err("mismatch");

    assert!(matches!(err, ToolError::Process { .. }));
    assert!(!f.root().join("new.txt").exists());
    assert_eq!(fs::read_to_string(f.root().join("existing.txt")).await.expect("read existing"), "actual\n");
}
