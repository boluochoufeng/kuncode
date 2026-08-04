//! The `write_file` tool: write a UTF-8 file inside the workspace.

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use kuncode_core::non_empty_vec::NonEmptyVec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::helpers::{
    non_empty_path, open_error, revalidate_path, workspace_error, write_no_follow,
};
use crate::{
    permission::{
        CanonicalPath, CanonicalToolInput, ChangePreview, PermissionCheckSpec, PermissionTarget,
        ToolDisplay,
    },
    tool::{
        PreparationContext, PreparedInvocationState, ReadState, ToolContext, ToolError, ToolOutput,
        TypedPreparation, TypedTool, definition_for,
    },
    workspace::Workspace,
};

/// Arguments accepted by the [`WriteFile`] tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteFileArgs {
    /// Workspace-relative or absolute file path to write.
    path: String,
    /// Complete UTF-8 content of the file. This replaces the file wholesale,
    /// so pass everything it should end up containing — never an abbreviation
    /// like `// ... rest unchanged`, which would be written literally.
    content: String,
}

/// Result of writing a workspace file.
#[derive(Debug, Serialize)]
pub struct WriteFileOutput {
    /// Path shown relative to the workspace when possible.
    pub path: String,
    /// Number of UTF-8 bytes written.
    pub bytes: usize,
}

/// Canonical write target paired with the content retained for execution.
#[derive(Debug)]
pub struct PreparedWriteFile {
    args: WriteFileArgs,
    path: PathBuf,
}

/// Writes UTF-8 files inside the workspace.
#[derive(Clone, Debug)]
pub struct WriteFile {
    definition: ToolDefinition,
    workspace: Workspace,
}

impl WriteFile {
    /// Creates a file writer bound to a workspace.
    pub fn new(workspace: Workspace) -> Self {
        Self {
            definition: definition_for::<WriteFileArgs>(
                "write_file",
                "Create a UTF-8 workspace file, or replace one entirely. An \
                 existing file is truncated first, so whatever is not in \
                 `content` is gone — replacing one is therefore refused unless \
                 read_file has already returned it in full during this session. \
                 To change part of a file, use edit_file instead: it names only \
                 the text being replaced and so cannot lose the rest.",
            ),
            workspace,
        }
    }
}

#[async_trait]
impl TypedTool for WriteFile {
    type Args = WriteFileArgs;
    type Prepared = PreparedWriteFile;
    type Output = WriteFileOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare_typed(
        &self,
        mut args: WriteFileArgs,
        _canonical_input: CanonicalToolInput,
        _ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
        let path = non_empty_path(&args.path)?;
        let resolved = self
            .workspace
            .resolve_target(path)
            .await
            .map_err(workspace_error)?;

        let canonical_path = CanonicalPath::from_absolute(&resolved)
            .map_err(|error| ToolOutput::failure("invalid_arguments", error.to_string()))?;
        let display_path = self.workspace.relative_display(&resolved);
        args.path = canonical_path.as_str().to_string();
        let canonical_input = CanonicalToolInput::new(serde_json::json!({
            "path": canonical_path.as_str(),
            "content": args.content,
        }));
        // Built here, while approval is still ahead: what makes this write worth
        // confirming is the part being dropped, and that is only visible against
        // what is on disk now.
        let display = ToolDisplay::new(format!("Write file: {display_path}"))
            .with_preview(preview_against_disk(&resolved, &args.content).await);
        Ok(TypedPreparation::new(
            PreparedWriteFile {
                args,
                path: resolved,
            },
            canonical_input,
            NonEmptyVec::new(PermissionCheckSpec::new(PermissionTarget::Edit(
                canonical_path,
            ))),
            display,
        ))
    }

    async fn run_prepared(
        &self,
        prepared: PreparedWriteFile,
        ctx: &ToolContext,
    ) -> ToolOutput<WriteFileOutput> {
        let PreparedWriteFile { args, path } = prepared;
        // Only an existing file has contents to lose. Creating one discards
        // nothing, so nothing has to have been read first.
        if let Some(refusal) = refuse_blind_overwrite(ctx, &path).await {
            return refusal;
        }
        if let Err(error) = write_no_follow(&path, args.content.as_bytes()).await {
            return open_error("write", &path, error, &self.workspace);
        }
        ctx.reads.record_write(&path, modified_time(&path).await);
        ToolOutput::success(WriteFileOutput {
            path: self.workspace.relative_display(&path),
            bytes: args.content.len(),
        })
    }

    async fn revalidate_prepared(
        &self,
        prepared: &mut PreparedWriteFile,
        _ctx: &ToolContext,
    ) -> Result<PreparedInvocationState, ToolError> {
        revalidate_path(&self.workspace, &prepared.path).await
    }
}

/// Diffs what is on disk against what is about to replace it.
///
/// A missing or unreadable file previews as nothing rather than as an error:
/// this runs before authorization, so it must not be able to turn a call the
/// policy would have allowed into a failure. Non-UTF-8 contents fall in the same
/// bucket — there is no line diff to show, and the write itself still reports
/// what happened.
async fn preview_against_disk(path: &Path, content: &str) -> Option<ChangePreview> {
    let existing = tokio::fs::read(path).await.ok()?;
    let existing = String::from_utf8(existing).ok()?;
    ChangePreview::between(&existing, content)
}

/// The file's modification time, or `None` if it has none to report.
async fn modified_time(path: &Path) -> Option<SystemTime> {
    tokio::fs::metadata(path)
        .await
        .ok()
        .and_then(|metadata| metadata.modified().ok())
}

/// Refuses to truncate a file whose current contents the session has not seen.
///
/// `write_file` replaces a file wholesale, so anything the caller omits is gone
/// with no way back. That is the intended behaviour when the caller has read
/// the file and is choosing what to keep, and data loss when it is writing from
/// memory or assumption — and the two are indistinguishable from the arguments
/// alone, which is why the session's reading history decides it.
async fn refuse_blind_overwrite<D>(ctx: &ToolContext, path: &Path) -> Option<ToolOutput<D>> {
    // No metadata means no file yet, and creating one loses nothing. Symlinks
    // are read without following, both to avoid stat-ing a target outside the
    // workspace and because `write_no_follow` refuses them anyway — with a far
    // more accurate message than "you have not read this file".
    let metadata = tokio::fs::symlink_metadata(path).await.ok()?;
    if !metadata.is_file() {
        return None;
    }
    match ctx.reads.state(path, metadata.modified().ok()) {
        ReadState::Current => None,
        ReadState::Never => Some(ToolOutput::failure(
            "unread_file",
            "this file already exists and has not been read in this session; \
             read_file it first, or use edit_file to change part of it without \
             replacing the rest",
        )),
        ReadState::Partial => Some(ToolOutput::failure(
            "unread_file",
            "only part of this file has been read, so writing it whole would \
             discard the rest; read it in full first, or use edit_file",
        )),
        ReadState::Stale => Some(ToolOutput::failure(
            "stale_read",
            "this file changed on disk after it was read, so writing it now \
             would discard those changes; read it again first",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use super::WriteFile;
    use crate::test_support::TestDir;
    use crate::tool::filesystem::ReadFile;
    use crate::tool::{ToolContext, execute_for_test};

    /// Runs `write_file` against `ctx`, so a test can share one session's
    /// reading history across several calls.
    async fn write(
        workspace: &crate::workspace::Workspace,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> crate::tool::ToolOutput {
        execute_for_test(Arc::new(WriteFile::new(workspace.clone())), args, ctx)
            .await
            .expect("no harness-level error")
    }

    async fn read(
        workspace: &crate::workspace::Workspace,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> crate::tool::ToolOutput {
        execute_for_test(Arc::new(ReadFile::new(workspace.clone())), args, ctx)
            .await
            .expect("no harness-level error")
    }

    #[tokio::test]
    async fn overwriting_a_file_nobody_read_is_refused() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("a.txt"), "original").expect("file should be written");
        let workspace = tmp.workspace().await;

        let output = write(
            &workspace,
            &ToolContext::new(),
            serde_json::json!({ "path": "a.txt", "content": "replacement" }),
        )
        .await;

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "unread_file"
        );
        // The refusal is only worth anything if the file survived it.
        assert_eq!(
            fs::read_to_string(tmp.path().join("a.txt")).expect("file should be readable"),
            "original"
        );
    }

    #[tokio::test]
    async fn a_full_read_licenses_the_overwrite() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("a.txt"), "original\n").expect("file should be written");
        let workspace = tmp.workspace().await;
        let ctx = ToolContext::new();

        assert!(
            read(&workspace, &ctx, serde_json::json!({ "path": "a.txt" }))
                .await
                .ok
        );
        let output = write(
            &workspace,
            &ctx,
            serde_json::json!({ "path": "a.txt", "content": "replacement" }),
        )
        .await;

        assert!(output.ok, "error: {:?}", output.error);
        assert_eq!(
            fs::read_to_string(tmp.path().join("a.txt")).expect("file should be readable"),
            "replacement"
        );
    }

    #[tokio::test]
    async fn reading_one_page_does_not_license_replacing_the_whole_file() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("a.txt"), "one\ntwo\nthree\n").expect("file should be written");
        let workspace = tmp.workspace().await;
        let ctx = ToolContext::new();

        // Everything past line 1 is unseen, and a whole-file write drops it.
        assert!(
            read(
                &workspace,
                &ctx,
                serde_json::json!({ "path": "a.txt", "limit": 1 })
            )
            .await
            .ok
        );
        let output = write(
            &workspace,
            &ctx,
            serde_json::json!({ "path": "a.txt", "content": "replacement" }),
        )
        .await;

        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "unread_file"
        );
    }

    #[tokio::test]
    async fn a_change_after_the_read_is_refused() {
        let tmp = TestDir::new();
        let path = tmp.path().join("a.txt");
        fs::write(&path, "original\n").expect("file should be written");
        let workspace = tmp.workspace().await;
        let ctx = ToolContext::new();

        assert!(
            read(&workspace, &ctx, serde_json::json!({ "path": "a.txt" }))
                .await
                .ok
        );
        // Stands in for the user or a formatter editing the file while the
        // model was deciding what to write.
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&path, "edited by someone else\n").expect("file should be written");

        let output = write(
            &workspace,
            &ctx,
            serde_json::json!({ "path": "a.txt", "content": "replacement" }),
        )
        .await;

        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "stale_read"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("file should be readable"),
            "edited by someone else\n"
        );
    }

    #[tokio::test]
    async fn writing_the_same_file_twice_is_allowed() {
        let tmp = TestDir::new();
        let workspace = tmp.workspace().await;
        let ctx = ToolContext::new();

        // The first write creates the file; the second overwrites what this
        // session itself put there, which it knows as well as anything it read.
        assert!(
            write(
                &workspace,
                &ctx,
                serde_json::json!({ "path": "a.txt", "content": "first" })
            )
            .await
            .ok
        );
        let output = write(
            &workspace,
            &ctx,
            serde_json::json!({ "path": "a.txt", "content": "second" }),
        )
        .await;

        assert!(output.ok, "error: {:?}", output.error);
        assert_eq!(
            fs::read_to_string(tmp.path().join("a.txt")).expect("file should be readable"),
            "second"
        );
    }

    #[tokio::test]
    async fn write_file_rejects_missing_parent() {
        let tmp = TestDir::new();
        let tool = WriteFile::new(tmp.workspace().await);

        let output = execute_for_test(
            Arc::new(tool),
            serde_json::json!({
                "path": "missing/new.txt",
                "content": "hello"
            }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness-level error");

        assert!(!output.ok);
        assert_eq!(output.error.expect("error present").kind.as_str(), "write");
    }

    #[tokio::test]
    async fn write_file_writes_inside_workspace() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join("src")).expect("directory should be created");
        let tool = WriteFile::new(tmp.workspace().await);

        let output = execute_for_test(
            Arc::new(tool),
            serde_json::json!({
                "path": "src/new.txt",
                "content": "hello"
            }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness-level error");

        assert!(output.ok);
        assert_eq!(
            fs::read_to_string(tmp.path().join("src/new.txt")).unwrap(),
            "hello"
        );
        assert_eq!(output.data.expect("data present")["bytes"], 5);
    }
}
