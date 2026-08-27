//! The `write_file` tool: write a UTF-8 file inside the workspace.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use kuncode_core::non_empty_vec::NonEmptyVec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::helpers::{
    file_stamp, non_empty_path, open_error, revalidate_path, workspace_error, write_no_follow,
};
use crate::{
    permission::{
        CanonicalPath, CanonicalToolInput, ChangePreview, PermissionCheckSpec, PermissionTarget,
        ToolDisplay,
    },
    tool::{
        FileStamp, PreparationContext, PreparedInvocationState, ReadLedger, ReadState, ToolContext,
        ToolError, ToolOutput, TypedPreparation, TypedTool, definition_for,
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
                 read_file has already returned it during this session. To \
                 change part of a file, use edit_file instead: it names only \
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
        ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
        let path = non_empty_path(&args.path)?;
        let resolved = self
            .workspace
            .resolve_target(path)
            .await
            .map_err(workspace_error)?;
        let canonical_path = CanonicalPath::from_absolute(&resolved)
            .map_err(|error| ToolOutput::failure("invalid_arguments", error.to_string()))?;
        // After the path itself is known to be valid, so a bad path is reported
        // as one rather than as an unread file — the shallower diagnosis would
        // send the caller off to read something it cannot name.
        //
        // Asked here at all so a call that cannot run never costs the user an
        // approval decision: being prompted about a change that then fails
        // anyway teaches that the prompt is not worth reading. Execution asks
        // again, since the file can change while the prompt is open.
        if let Some(refusal) = refuse_blind_overwrite(&ctx.reads, &resolved).await {
            return Err(refusal);
        }
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
        // Preparation already refused the calls it could see coming. This is
        // the second half of that check, against the file as it stands now: the
        // approval prompt was open for as long as the user took to answer it,
        // and the guard is worth nothing if that window is a way through.
        if let Some(refusal) = refuse_blind_overwrite(&ctx.reads, &path).await {
            return refusal;
        }
        if let Err(error) = write_no_follow(&path, args.content.as_bytes()).await {
            return open_error("write", &path, error, &self.workspace);
        }
        // Supplying the contents whole is knowing them whole, so this counts as
        // a reading — without it, writing the same file twice in one session
        // would be refused the second time.
        ctx.reads.record(&path, file_stamp(&path).await);
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

/// Refuses to truncate a file whose current contents the session has not seen.
///
/// `write_file` replaces a file wholesale, so anything the caller omits is gone
/// with no way back. That is the intended behaviour when the caller has read
/// the file and is choosing what to keep, and data loss when it is writing from
/// memory or assumption — and the two are indistinguishable from the arguments
/// alone, which is why the session's reading history decides it.
///
/// Run once while preparing and once while executing, which is why it takes the
/// ledger rather than either context: the first refusal spares the user a
/// pointless approval, and the second covers what changed while they answered.
async fn refuse_blind_overwrite<D>(reads: &ReadLedger, path: &Path) -> Option<ToolOutput<D>> {
    // No metadata means no file yet, and creating one loses nothing. Symlinks
    // are read without following, both to avoid stat-ing a target outside the
    // workspace and because `write_no_follow` refuses them anyway — with a far
    // more accurate message than "you have not read this file".
    let metadata = tokio::fs::symlink_metadata(path).await.ok()?;
    if !metadata.is_file() {
        return None;
    }
    match reads.state(path, FileStamp::from_metadata(&metadata)) {
        ReadState::Current => None,
        ReadState::Never => Some(ToolOutput::failure(
            "unread_file",
            "this file already exists and has not been read in this session; \
             read_file it first, or use edit_file to change part of it without \
             replacing the rest",
        )),
        ReadState::Stale => Some(ToolOutput::failure(
            "stale_read",
            "this file changed on disk after it was read, so writing it now \
             would discard those changes; read it again first",
        )),
        ReadState::Evicted => Some(ToolOutput::failure(
            "evicted_read",
            "this file was read, but that reading was since compacted out of \
             the conversation, so its contents are no longer in view; \
             read_file it again before replacing the whole file, or use \
             edit_file to change part of it without replacing the rest",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::Arc,
        time::{Duration, SystemTime},
    };

    use super::WriteFile;
    use crate::test_support::TestDir;
    use crate::tool::filesystem::{EditFile, ReadFile};
    use crate::tool::{Tool, ToolContext, execute_for_test};

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

    async fn edit(
        workspace: &crate::workspace::Workspace,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> crate::tool::ToolOutput {
        execute_for_test(Arc::new(EditFile::new(workspace.clone())), args, ctx)
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
    async fn an_evicted_read_no_longer_licenses_the_overwrite() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("a.txt"), "original\n").expect("file should be written");
        let workspace = tmp.workspace().await;
        let ledger = crate::tool::ReadLedger::default();

        // Read through a witnessed handle, the way the runner binds one.
        let read_ctx = ToolContext::new().with_reads(ledger.witnessed_by("read-1"));
        assert!(
            read(
                &workspace,
                &read_ctx,
                serde_json::json!({ "path": "a.txt" })
            )
            .await
            .ok
        );
        // What installing a compacted context does once the read's tool result
        // is no longer among the active messages.
        ledger.evict_unwitnessed(&std::collections::HashSet::new());

        let ctx = ToolContext::new().with_reads(ledger.clone());
        let refused = write(
            &workspace,
            &ctx,
            serde_json::json!({ "path": "a.txt", "content": "replacement" }),
        )
        .await;

        assert!(!refused.ok);
        assert_eq!(
            refused.error.expect("error present").kind.as_str(),
            "evicted_read"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("a.txt")).expect("file should be readable"),
            "original\n"
        );

        // Re-reading is the move the refusal asks for, and it satisfies it.
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
    }

    #[tokio::test]
    async fn reading_one_page_licenses_replacing_the_whole_file() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("a.txt"), "one\ntwo\nthree\n").expect("file should be written");
        let workspace = tmp.workspace().await;
        let ctx = ToolContext::new();

        // Lines 2 and 3 were never returned, and this write does drop them. The
        // guard asks whether the caller looked at the file at all, not whether
        // it looked at every line — a per-line bar is unsatisfiable for the
        // files below, and what a write discards is shown in the approval diff.
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

        assert!(output.ok, "error: {:?}", output.error);
    }

    #[tokio::test]
    async fn a_file_with_an_over_long_line_can_still_be_rewritten() {
        let tmp = TestDir::new();
        // Past `read_file`'s per-line cap, so the line comes back clipped and
        // every re-read clips it identically. Demanding an unclipped reading
        // would leave this file permanently unwritable, with no call the caller
        // could make to change that.
        fs::write(tmp.path().join("a.txt"), "x".repeat(3_000)).expect("file should be written");
        let workspace = tmp.workspace().await;
        let ctx = ToolContext::new();

        let seen = read(&workspace, &ctx, serde_json::json!({ "path": "a.txt" })).await;
        assert!(seen.ok);
        // The loss is still reported to the caller; it just is not treated as
        // grounds to refuse every later write.
        assert!(
            !seen.data.expect("data present")["truncated_lines"]
                .as_array()
                .expect("truncated lines present")
                .is_empty()
        );

        let output = write(
            &workspace,
            &ctx,
            serde_json::json!({ "path": "a.txt", "content": "replacement" }),
        )
        .await;

        assert!(output.ok, "error: {:?}", output.error);
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
    async fn a_modification_time_moved_backwards_is_refused_too() {
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
        // Stands in for `rsync -t`, `cp -p`, or an archive unpacked with times
        // preserved: the file is replaced by one carrying an *older* stamp.
        // Deliberately the same length as what it replaced, so the size
        // dimension stays silent and only the clock can catch this — comparing
        // for a strictly newer time would wave it through.
        fs::write(&path, "replaced\n").expect("file should be written");
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("file should open")
            .set_times(
                fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1)),
            )
            .expect("modification time should be settable");

        let output = write(
            &workspace,
            &ctx,
            serde_json::json!({ "path": "a.txt", "content": "whatever" }),
        )
        .await;

        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "stale_read"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("file should be readable"),
            "replaced\n"
        );
    }

    #[tokio::test]
    async fn a_blind_overwrite_is_refused_before_anyone_is_asked_to_approve_it() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("a.txt"), "original").expect("file should be written");
        let workspace = tmp.workspace().await;
        let ctx = ToolContext::new();

        // Preparation only — the stage that runs before the permission prompt.
        // A call refused here never reaches the user, which is the point:
        // approving a write that then fails anyway spends a decision on
        // nothing and teaches that the prompt is not worth reading.
        let prepared = Arc::new(WriteFile::new(workspace))
            .prepare(
                serde_json::json!({ "path": "a.txt", "content": "replacement" }),
                &ctx.preparation(),
            )
            .await;

        let output = prepared.err().expect("preparation should refuse");
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "unread_file"
        );
    }

    #[tokio::test]
    async fn preparation_sees_what_the_session_has_read() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("a.txt"), "original\n").expect("file should be written");
        let workspace = tmp.workspace().await;
        let ctx = ToolContext::new();

        assert!(
            read(&workspace, &ctx, serde_json::json!({ "path": "a.txt" }))
                .await
                .ok
        );
        // The reading history has to survive the trip from the session's
        // execution context into the preparation one, or the guard would refuse
        // every overwrite regardless of what was read.
        let prepared = Arc::new(WriteFile::new(workspace))
            .prepare(
                serde_json::json!({ "path": "a.txt", "content": "replacement" }),
                &ctx.preparation(),
            )
            .await;

        assert!(
            prepared.is_ok(),
            "preparation refused a file this session read"
        );
    }

    #[tokio::test]
    async fn an_edit_does_not_make_a_read_file_look_stale() {
        let tmp = TestDir::new();
        let path = tmp.path().join("a.txt");
        fs::write(&path, "one\ntwo\n").expect("file should be written");
        let workspace = tmp.workspace().await;
        let ctx = ToolContext::new();

        assert!(
            read(&workspace, &ctx, serde_json::json!({ "path": "a.txt" }))
                .await
                .ok
        );
        // Long enough for the edit to land on a later timestamp than the read.
        // Unrecorded, that gap would report the session's own change back to it
        // as an outside one — the read-edit-write sequence is routine, and it
        // has to work.
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(
            edit(
                &workspace,
                &ctx,
                serde_json::json!({ "path": "a.txt", "old_text": "one", "new_text": "1" })
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

        assert!(output.ok, "error: {:?}", output.error);
    }

    #[tokio::test]
    async fn editing_an_unread_file_does_not_license_replacing_it() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("a.txt"), "one\ntwo\n").expect("file should be written");
        let workspace = tmp.workspace().await;
        let ctx = ToolContext::new();

        // `edit_file` needs no prior read, since it names the text it replaces.
        // That must not become a way around this guard: knowing one snippet
        // says nothing about the lines around it.
        assert!(
            edit(
                &workspace,
                &ctx,
                serde_json::json!({ "path": "a.txt", "old_text": "one", "new_text": "1" })
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
        assert_eq!(
            fs::read_to_string(tmp.path().join("a.txt")).expect("file should be readable"),
            "1\ntwo\n"
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
