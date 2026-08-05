//! The `edit_file` tool: replace occurrences of text in a file.

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
    /// Text to replace, which must appear in the file exactly once unless
    /// `replace_all` is set. Include enough surrounding lines to make it
    /// unique — a snippet that occurs twice is rejected rather than guessed
    /// at.
    old_text: String,
    /// Replacement text, written in place of `old_text` verbatim.
    new_text: String,
    /// Replace every occurrence rather than requiring exactly one. Set it when
    /// changing all of something — renaming a symbol, say — where the
    /// repetition is the point and no surrounding context could tell the
    /// occurrences apart.
    #[serde(default)]
    replace_all: bool,
}

impl EditFileArgs {
    /// Applies this edit to `content`, without regard for whether it may be.
    ///
    /// Shared by the preview and the write so that what is approved and what
    /// lands cannot be produced by two subtly different expressions.
    fn applied_to(&self, content: &str) -> String {
        if self.replace_all {
            content.replace(&self.old_text, &self.new_text)
        } else {
            content.replacen(&self.old_text, &self.new_text, 1)
        }
    }
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
    /// How many occurrences the preview was built from. Execution refuses a
    /// file that no longer has that many: with `replace_all`, "every
    /// occurrence" is only a well-defined change against a known file, and one
    /// that grew an occurrence while the prompt was open would quietly widen
    /// what was approved.
    occurrences: usize,
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
                "Replace exact occurrences of `old_text` in a UTF-8 workspace \
                 file, leaving the rest untouched. `old_text` must match once \
                 and only once, unless `replace_all` asks for every occurrence \
                 — matching twice is refused rather than guessed at. Preferred \
                 over write_file for changing a file that already exists, since \
                 it names only what changes.",
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
        let (content, occurrences) = read_matches(&self.workspace, &resolved, &args).await?;
        let display_path = self.workspace.relative_display(&resolved);
        args.path = canonical_path.as_str().to_string();
        // `replace_all` belongs in here with the rest: it is the difference
        // between changing one line and changing every line that looks like it,
        // and two calls that differ only by it are not the same call to reason
        // about.
        let canonical_input = CanonicalToolInput::new(serde_json::json!({
            "path": canonical_path.as_str(),
            "old_text": args.old_text,
            "new_text": args.new_text,
            "replace_all": args.replace_all,
        }));
        // Applied here only to show it. The edit is redone against the file as
        // it stands at execution, so what runs is never this copy — a preview
        // that went stale in between misleads about the change, but cannot
        // become the change.
        let display = ToolDisplay::new(format!("Edit file: {display_path}"))
            .with_preview(ChangePreview::between(&content, &args.applied_to(&content)));
        Ok(TypedPreparation::new(
            PreparedEditFile {
                args,
                path: resolved,
                occurrences,
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
        let PreparedEditFile {
            args,
            path,
            occurrences,
        } = prepared;
        // Preparation already refused the calls it could see coming. This is
        // the second half of that check, against the file as it stands now: the
        // approval prompt was open for as long as the user took to answer it,
        // and an edit anchored on text that moved in that window would land
        // somewhere nobody agreed to.
        let (content, found) = match read_matches(&self.workspace, &path, &args).await {
            Ok(found) => found,
            Err(refusal) => return refusal,
        };
        if found != occurrences {
            return ToolOutput::failure(
                "stale_match",
                format!(
                    "`old_text` matched {occurrences} times in `{}` when this edit was \
                     prepared and {found} times now; the file changed in between. \
                     Read it again and reissue the edit against what it says now",
                    self.workspace.relative_display(&path)
                ),
            );
        }

        let edited = args.applied_to(&content);
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
            replacements: found,
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

/// Reads `path` and counts the occurrences this edit is allowed to act on.
///
/// The tool takes text, not a position, so a snippet appearing twice leaves
/// nothing to decide which one was meant. Guessing — taking the first — is
/// wrong silently: the caller is told the edit succeeded and carries on, with
/// the mistake surfacing much later somewhere else. Refusing is wrong loudly,
/// and one retry with more context fixes it. `replace_all` is how the caller
/// says the repetition itself is the target, which makes many occurrences an
/// answer rather than an ambiguity. Nothing found is a refusal either way:
/// there is no anchor at all.
///
/// Both stages ask this — preparation so a doomed call never reaches the user,
/// execution so a file that moved while the prompt was open is caught. Sharing
/// one function is what keeps the two answers from drifting apart, and returning
/// the contents means the caller that asked has what it needs to act on the
/// answer without reading the file a second time.
async fn read_matches<D>(
    workspace: &Workspace,
    path: &Path,
    args: &EditFileArgs,
) -> Result<(String, usize), ToolOutput<D>> {
    let mut file = open_no_follow(path, OpenOptions::new().read(true))
        .await
        .map_err(|err| open_error("read", path, err, workspace))?;
    let mut content = String::new();
    file.read_to_string(&mut content)
        .await
        .map_err(|err| io_error("read", path, err, workspace))?;

    match (content.matches(&args.old_text).count(), args.replace_all) {
        (0, _) => Err(ToolOutput::failure(
            "text_not_found",
            format!(
                "`old_text` was not found in `{}`",
                workspace.relative_display(path)
            ),
        )),
        (count, true) => Ok((content, count)),
        (1, false) => Ok((content, 1)),
        (count, false) => Err(ToolOutput::failure(
            "ambiguous_match",
            format!(
                "`old_text` matches {count} times in `{}`; include surrounding \
                 context so it is unique, or set `replace_all` if every \
                 occurrence should change",
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

    #[tokio::test]
    async fn replace_all_changes_every_occurrence() {
        let tmp = TestDir::new();
        fs::write(
            tmp.path().join("notes.txt"),
            "old one\nold two\nkeep\nold three\n",
        )
        .expect("file should be written");
        let tool = EditFile::new(tmp.workspace().await);

        let output = execute_for_test(
            Arc::new(tool),
            serde_json::json!({
                "path": "notes.txt",
                "old_text": "old",
                "new_text": "new",
                "replace_all": true
            }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness-level error");

        assert!(output.ok);
        assert_eq!(
            fs::read_to_string(tmp.path().join("notes.txt")).unwrap(),
            "new one\nnew two\nkeep\nnew three\n"
        );
        assert_eq!(output.data.expect("data present")["replacements"], 3);
    }

    #[tokio::test]
    async fn replace_all_still_needs_something_to_match() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("notes.txt"), "hello").expect("file should be written");
        let tool = EditFile::new(tmp.workspace().await);

        let output = execute_for_test(
            Arc::new(tool),
            serde_json::json!({
                "path": "notes.txt",
                "old_text": "missing",
                "new_text": "done",
                "replace_all": true
            }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness-level error");

        // "Every occurrence" of nothing is not an edit worth reporting as one:
        // the model asked to change something that is not there, and silently
        // rewriting the file unchanged would hide that.
        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "text_not_found"
        );
    }

    /// Without `replace_all`, a second occurrence is ambiguity — and the
    /// refusal has to name the way out, or the model retries by padding
    /// context that cannot be made unique.
    #[tokio::test]
    async fn an_ambiguous_match_points_at_both_ways_out() {
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

        let message = output.error.expect("error present").message;
        assert!(message.contains("unique"), "{message}");
        assert!(message.contains("replace_all"), "{message}");
    }

    #[tokio::test]
    async fn replace_all_refuses_to_widen_past_what_was_approved() {
        let tmp = TestDir::new();
        let path = tmp.path().join("notes.txt");
        fs::write(&path, "old one\nold two\n").expect("file should be written");
        let ctx = ToolContext::new();

        let prepared = Arc::new(EditFile::new(tmp.workspace().await))
            .prepare(
                serde_json::json!({
                    "path": "notes.txt",
                    "old_text": "old",
                    "new_text": "new",
                    "replace_all": true
                }),
                &ctx.preparation(),
            )
            .await
            .expect("preparation should accept an edit that matches");

        // The preview showed two changes and that is what was approved. A third
        // occurrence appearing while the prompt was open would be swept up
        // silently, since `replace_all` names no text that would notice it.
        fs::write(&path, "old one\nold two\nold three\n").expect("file should be rewritten");

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
            "stale_match"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("file should be readable"),
            "old one\nold two\nold three\n"
        );
    }

    #[tokio::test]
    async fn replace_all_is_part_of_what_a_rule_decides_on() {
        let tmp = TestDir::new();
        // One occurrence, so both spellings of the call prepare successfully
        // and the canonical inputs differ only by the flag under test.
        fs::write(tmp.path().join("notes.txt"), "old one\ntwo\n").expect("file should be written");
        let workspace = tmp.workspace().await;
        let ctx = ToolContext::new();

        let canonical = |replace_all: bool| {
            let tool = Arc::new(EditFile::new(workspace.clone()));
            let preparation = ctx.preparation();
            async move {
                tool.prepare(
                    serde_json::json!({
                        "path": "notes.txt",
                        "old_text": "old",
                        "new_text": "new",
                        "replace_all": replace_all
                    }),
                    &preparation,
                )
                .await
                .expect("preparation should accept an edit that matches")
                .into_parts()
                .0
            }
        };

        // Editing one line and editing every line that looks like it are not
        // the same request. If the canonical input could not tell them apart,
        // approving the narrow one would carry over to the broad one.
        assert_ne!(
            canonical(false).await.as_value(),
            canonical(true).await.as_value()
        );
    }
}
