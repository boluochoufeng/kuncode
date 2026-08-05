//! The `edit_file` tool: replace one unique occurrence of text in a file.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use kuncode_core::non_empty_vec::NonEmptyVec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use tokio::{fs::OpenOptions, io::AsyncReadExt};

use super::helpers::{
    file_stamp, io_error, non_empty_path, open_error, open_no_follow, revalidate_path,
    workspace_error, write_no_follow,
};
use crate::{
    permission::{
        CanonicalPath, CanonicalToolInput, ChangePreview, PermissionCheckSpec, PermissionTarget,
        ToolDisplay,
    },
    tool::{
        PreparationContext, PreparedInvocationState, ToolContext, ToolError, ToolOutput,
        TypedPreparation, TypedTool, definition_for,
    },
    workspace::Workspace,
};

/// Arguments accepted by the [`EditFile`] tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditFileArgs {
    /// Workspace-relative or absolute path to an existing UTF-8 file.
    path: String,
    /// Text to replace, which must appear in the file exactly once. Include
    /// enough surrounding lines to make it unique — a snippet that occurs
    /// twice is rejected rather than guessed at.
    old_text: String,
    /// Replacement text, written in place of `old_text` verbatim.
    new_text: String,
}

/// Result of editing a workspace file.
#[derive(Debug, Serialize)]
pub struct EditFileOutput {
    /// Path shown relative to the workspace when possible.
    pub path: String,
    /// Number of replacements applied.
    pub replacements: usize,
    /// Number of UTF-8 bytes written after the edit.
    pub bytes: usize,
}

/// Canonical edit target paired with the exact replacement retained for execution.
#[derive(Debug)]
pub struct PreparedEditFile {
    args: EditFileArgs,
    path: PathBuf,
}

/// Replaces text in UTF-8 files inside the workspace.
#[derive(Clone, Debug)]
pub struct EditFile {
    definition: ToolDefinition,
    workspace: Workspace,
}

impl EditFile {
    /// Creates a file editor bound to a workspace.
    pub fn new(workspace: Workspace) -> Self {
        Self {
            definition: definition_for::<EditFileArgs>(
                "edit_file",
                "Replace one exact occurrence of `old_text` in a UTF-8 \
                 workspace file, leaving the rest untouched. Preferred over \
                 write_file for changing a file that already exists, since it \
                 names only what changes.",
            ),
            workspace,
        }
    }
}

#[async_trait]
impl TypedTool for EditFile {
    type Args = EditFileArgs;
    type Prepared = PreparedEditFile;
    type Output = EditFileOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare_typed(
        &self,
        mut args: EditFileArgs,
        _canonical_input: CanonicalToolInput,
        _ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
        let path = non_empty_path(&args.path)?;
        if args.old_text.is_empty() {
            return Err(ToolOutput::failure(
                "invalid_arguments",
                "`old_text` must not be empty",
            ));
        }
        let resolved = self
            .workspace
            .resolve_target(path)
            .await
            .map_err(workspace_error)?;

        let canonical_path = CanonicalPath::from_absolute(&resolved)
            .map_err(|error| ToolOutput::failure("invalid_arguments", error.to_string()))?;
        // After the path itself is known to be valid, so a bad path is reported
        // as one rather than as text that could not be found in it.
        //
        // Asked here at all so a call that cannot run never costs the user an
        // approval decision: being prompted about an edit that then fails
        // anyway spends a decision on nothing and teaches that the prompt is
        // not worth reading. Execution asks again, since the file can change
        // while the prompt is open.
        let content = read_unique_match(&self.workspace, &resolved, &args.old_text).await?;
        let display_path = self.workspace.relative_display(&resolved);
        args.path = canonical_path.as_str().to_string();
        let canonical_input = CanonicalToolInput::new(serde_json::json!({
            "path": canonical_path.as_str(),
            "old_text": args.old_text,
            "new_text": args.new_text,
        }));
        // Applied here only to show it. The edit is redone against the file as
        // it stands at execution, so what runs is never this copy — a preview
        // that went stale in between misleads about the change, but cannot
        // become the change.
        let display = ToolDisplay::new(format!("Edit file: {display_path}")).with_preview(
            ChangePreview::between(
                &content,
                &content.replacen(&args.old_text, &args.new_text, 1),
            ),
        );
        Ok(TypedPreparation::new(
            PreparedEditFile {
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
        prepared: PreparedEditFile,
        ctx: &ToolContext,
    ) -> ToolOutput<EditFileOutput> {
        let PreparedEditFile { args, path } = prepared;
        // Preparation already refused the calls it could see coming. This is
        // the second half of that check, against the file as it stands now: the
        // approval prompt was open for as long as the user took to answer it,
        // and an edit anchored on text that moved in that window would land
        // somewhere nobody agreed to.
        let content = match read_unique_match(&self.workspace, &path, &args.old_text).await {
            Ok(content) => content,
            Err(refusal) => return refusal,
        };

        let edited = content.replacen(&args.old_text, &args.new_text, 1);
        if let Err(err) = write_no_follow(&path, edited.as_bytes()).await {
            return open_error("write", &path, err, &self.workspace);
        }
        // The change is the session's own, so it must not come back to
        // `write_file` as somebody else's. Only an existing baseline moves —
        // editing a file nobody read leaves it unread, since replacing one
        // known snippet says nothing about the lines around it.
        ctx.reads.touch(&path, file_stamp(&path).await);

        ToolOutput::success(EditFileOutput {
            path: self.workspace.relative_display(&path),
            replacements: 1,
            bytes: edited.len(),
        })
    }

    async fn revalidate_prepared(
        &self,
        prepared: &mut PreparedEditFile,
        _ctx: &ToolContext,
    ) -> Result<PreparedInvocationState, ToolError> {
        revalidate_path(&self.workspace, &prepared.path).await
    }
}

/// Reads `path`, confirming `old_text` names exactly one place in it.
///
/// `old_text` must be unambiguous: zero matches gives the model nothing to
/// anchor on, and more than one would let us silently edit the wrong
/// occurrence. Forcing a unique match is what makes the edit deterministic.
///
/// Both stages ask this — preparation so a doomed call never reaches the user,
/// execution so a file that moved while the prompt was open is caught. Sharing
/// one function is what keeps the two answers from drifting apart, and returning
/// the contents means the caller that asked has what it needs to act on the
/// answer without reading the file a second time.
async fn read_unique_match<D>(
    workspace: &Workspace,
    path: &Path,
    old_text: &str,
) -> Result<String, ToolOutput<D>> {
    let mut file = open_no_follow(path, OpenOptions::new().read(true))
        .await
        .map_err(|err| open_error("read", path, err, workspace))?;
    let mut content = String::new();
    file.read_to_string(&mut content)
        .await
        .map_err(|err| io_error("read", path, err, workspace))?;

    match content.matches(old_text).count() {
        1 => Ok(content),
        0 => Err(ToolOutput::failure(
            "text_not_found",
            format!(
                "`old_text` was not found in `{}`",
                workspace.relative_display(path)
            ),
        )),
        count => Err(ToolOutput::failure(
            "ambiguous_match",
            format!(
                "`old_text` matches {count} times in `{}`; \
                 include surrounding context so it is unique",
                workspace.relative_display(path)
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use super::EditFile;
    use crate::test_support::TestDir;
    use crate::tool::{Tool, ToolContext, execute_for_test};

    #[tokio::test]
    async fn edit_file_replaces_once() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("notes.txt"), "target rest").expect("file should be written");
        let tool = EditFile::new(tmp.workspace().await);

        let output = execute_for_test(
            Arc::new(tool),
            serde_json::json!({
                "path": "notes.txt",
                "old_text": "target",
                "new_text": "done"
            }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness-level error");

        assert!(output.ok);
        assert_eq!(
            fs::read_to_string(tmp.path().join("notes.txt")).unwrap(),
            "done rest"
        );
        assert_eq!(output.data.expect("data present")["replacements"], 1);
    }

    #[tokio::test]
    async fn edit_file_rejects_ambiguous_match() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("notes.txt"), "same same").expect("file should be written");
        let tool = EditFile::new(tmp.workspace().await);

        let output = execute_for_test(
            Arc::new(tool),
            serde_json::json!({
                "path": "notes.txt",
                "old_text": "same",
                "new_text": "done"
            }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness-level error");

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "ambiguous_match"
        );
        // The file is left untouched when the match is ambiguous.
        assert_eq!(
            fs::read_to_string(tmp.path().join("notes.txt")).unwrap(),
            "same same"
        );
    }

    #[tokio::test]
    async fn edit_file_reports_missing_text() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("notes.txt"), "hello").expect("file should be written");
        let tool = EditFile::new(tmp.workspace().await);

        let output = execute_for_test(
            Arc::new(tool),
            serde_json::json!({
                "path": "notes.txt",
                "old_text": "missing",
                "new_text": "done"
            }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness-level error");

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "text_not_found"
        );
    }

    /// Preparation is the stage before the permission prompt. Both of these
    /// refusals are decidable there, from the file alone, so spending the
    /// user's attention on them first is spending it on nothing.
    #[tokio::test]
    async fn an_edit_that_cannot_land_is_refused_before_anyone_is_asked_to_approve_it() {
        for (contents, old_text, expected) in [
            ("hello", "missing", "text_not_found"),
            ("same same", "same", "ambiguous_match"),
        ] {
            let tmp = TestDir::new();
            fs::write(tmp.path().join("notes.txt"), contents).expect("file should be written");
            let ctx = ToolContext::new();

            let prepared = Arc::new(EditFile::new(tmp.workspace().await))
                .prepare(
                    serde_json::json!({
                        "path": "notes.txt",
                        "old_text": old_text,
                        "new_text": "done"
                    }),
                    &ctx.preparation(),
                )
                .await;

            let output = prepared.err().expect("preparation should refuse");
            assert_eq!(output.error.expect("error present").kind.as_str(), expected);
        }
    }

    #[tokio::test]
    async fn an_approved_edit_still_checks_the_file_it_was_approved_against() {
        let tmp = TestDir::new();
        let path = tmp.path().join("notes.txt");
        fs::write(&path, "target rest").expect("file should be written");
        let ctx = ToolContext::new();

        let prepared = Arc::new(EditFile::new(tmp.workspace().await))
            .prepare(
                serde_json::json!({
                    "path": "notes.txt",
                    "old_text": "target",
                    "new_text": "done"
                }),
                &ctx.preparation(),
            )
            .await
            .expect("preparation should accept an edit that matches once");

        // Someone else moves the anchor while the approval prompt is open. The
        // preparation-stage check is spent by now, so only the execution-stage
        // one stands between this and an edit landing somewhere nobody saw.
        fs::write(&path, "target target").expect("file should be rewritten");

        let (_, invocation, _, _) = prepared.into_parts();
        let output = invocation
            .execute(&ctx)
            .await
            .expect("no harness-level error")
            .into_parts()
            .0;

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "ambiguous_match"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("file should be readable"),
            "target target"
        );
    }

    #[tokio::test]
    async fn an_edit_is_previewed_before_it_is_approved() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("notes.txt"), "target rest").expect("file should be written");
        let ctx = ToolContext::new();

        let prepared = Arc::new(EditFile::new(tmp.workspace().await))
            .prepare(
                serde_json::json!({
                    "path": "notes.txt",
                    "old_text": "target",
                    "new_text": "done"
                }),
                &ctx.preparation(),
            )
            .await
            .expect("preparation should accept an edit that matches once");

        let (_, _, _, display) = prepared.into_parts();
        assert!(
            display.preview().is_some(),
            "an approval prompt for an edit has to show what the edit does"
        );
    }
}
