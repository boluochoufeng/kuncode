use std::{fs, path::Path};

use kuncode_workspace::{ExecutionLane, LaneKind, Workspace, WorkspaceConfig, WorkspaceError};
use tempfile::tempdir;

async fn workspace_at(path: &Path) -> Workspace {
    Workspace::open(path, WorkspaceConfig::default()).await.expect("open workspace")
}

#[tokio::test]
async fn root_is_canonicalized() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("root");
    fs::create_dir(&root).expect("create root");

    let workspace = workspace_at(&root).await;

    assert_eq!(workspace.root(), root.canonicalize().expect("canonical root"));
}

#[tokio::test]
async fn parent_components_cannot_escape_root() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("root");
    let outside = temp.path().join("outside");
    fs::create_dir(&root).expect("create root");
    fs::create_dir(&outside).expect("create outside");
    fs::write(outside.join("secret.txt"), "secret").expect("write outside");

    let workspace = workspace_at(&root).await;
    let err = workspace.resolve_read_file("../outside/secret.txt").await.expect_err("escape rejected");

    assert!(matches!(err, WorkspaceError::PathEscape { .. }));
}

#[tokio::test]
async fn absolute_paths_outside_root_are_rejected() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("root");
    let outside = temp.path().join("outside");
    fs::create_dir(&root).expect("create root");
    fs::create_dir(&outside).expect("create outside");
    let outside_file = outside.join("secret.txt");
    fs::write(&outside_file, "secret").expect("write outside");

    let workspace = workspace_at(&root).await;
    let err = workspace.resolve_read_file(&outside_file).await.expect_err("absolute escape rejected");

    assert!(matches!(err, WorkspaceError::PathEscape { .. }));
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_targets_outside_root_are_rejected() {
    use std::os::unix::fs::symlink;

    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("root");
    let outside = temp.path().join("outside");
    fs::create_dir(&root).expect("create root");
    fs::create_dir(&outside).expect("create outside");
    let outside_file = outside.join("secret.txt");
    fs::write(&outside_file, "secret").expect("write outside");
    symlink(&outside_file, root.join("link.txt")).expect("create symlink");

    let workspace = workspace_at(&root).await;
    let err = workspace.resolve_read_file("link.txt").await.expect_err("symlink escape rejected");

    assert!(matches!(err, WorkspaceError::SymlinkEscape { .. }));
}

#[tokio::test]
async fn valid_workspace_file_resolves_to_workspace_path() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("root");
    fs::create_dir(&root).expect("create root");
    fs::write(root.join("main.rs"), "fn main() {}\n").expect("write file");

    let workspace = workspace_at(&root).await;
    let path = workspace.resolve_read_file("main.rs").await.expect("resolve file");

    assert_eq!(path.as_path(), root.canonicalize().expect("canonical root").join("main.rs"));
    assert_eq!(path.relative_path(), Path::new("main.rs"));
}

#[tokio::test]
async fn existing_directory_resolves_to_workspace_path() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("root");
    fs::create_dir(&root).expect("create root");
    fs::create_dir(root.join("src")).expect("create src");

    let workspace = workspace_at(&root).await;
    let path = workspace.resolve_existing_path("src").await.expect("resolve dir");

    assert_eq!(path.relative_path(), Path::new("src"));
}

#[tokio::test]
async fn write_path_allows_new_file_with_safe_parent() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("root");
    fs::create_dir(&root).expect("create root");
    fs::create_dir(root.join("src")).expect("create src");

    let workspace = workspace_at(&root).await;
    let path = workspace.resolve_write_path("src/new.rs").await.expect("resolve write path");

    assert_eq!(path.relative_path(), Path::new("src/new.rs"));
}

#[tokio::test]
async fn write_path_rejects_parent_outside_root() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("root");
    let outside = temp.path().join("outside");
    fs::create_dir(&root).expect("create root");
    fs::create_dir(&outside).expect("create outside");

    let workspace = workspace_at(&root).await;
    let err = workspace.resolve_write_path("../outside/new.rs").await.expect_err("escape rejected");

    assert!(matches!(err, WorkspaceError::PathEscape { .. }));
}

#[tokio::test]
async fn oversized_files_are_rejected() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("root");
    fs::create_dir(&root).expect("create root");
    fs::write(root.join("large.txt"), "abcd").expect("write file");

    let workspace = Workspace::open(&root, WorkspaceConfig { max_file_size: 3, reject_binary: true })
        .await
        .expect("open workspace");
    let err = workspace.resolve_read_file("large.txt").await.expect_err("too large rejected");

    assert!(matches!(err, WorkspaceError::TooLarge { size: 4, max: 3, .. }));
}

#[tokio::test]
async fn nul_or_non_utf8_files_are_rejected_as_binary() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("root");
    fs::create_dir(&root).expect("create root");
    fs::write(root.join("nul.txt"), b"hello\0world").expect("write nul file");
    fs::write(root.join("latin1.txt"), [0xff, 0xfe]).expect("write non-utf8 file");

    let workspace = workspace_at(&root).await;
    let nul = workspace.resolve_read_file("nul.txt").await.expect_err("nul rejected");
    let non_utf8 = workspace.resolve_read_file("latin1.txt").await.expect_err("non-utf8 rejected");

    assert!(matches!(nul, WorkspaceError::Binary { .. }));
    assert!(matches!(non_utf8, WorkspaceError::Binary { .. }));
}

#[tokio::test]
async fn main_execution_lane_binds_workspace_root() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("root");
    fs::create_dir(&root).expect("create root");

    let workspace = workspace_at(&root).await;
    let lane = ExecutionLane::main(&workspace);

    assert_eq!(lane.kind(), LaneKind::MainWorkspace);
    assert_eq!(lane.root_path(), workspace.root());
}

#[tokio::test]
async fn default_ignored_components_are_detected() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("root");
    fs::create_dir(&root).expect("create root");

    let workspace = workspace_at(&root).await;

    assert!(workspace.is_default_ignored("target/debug"));
    assert!(workspace.is_default_ignored("node_modules/pkg"));
    assert!(!workspace.is_default_ignored("src/main.rs"));
}
