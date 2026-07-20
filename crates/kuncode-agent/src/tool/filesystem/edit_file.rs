//! The `edit_file` tool: replace one unique occurrence of text in a file.

use std::path::PathBuf;

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use kuncode_core::non_empty_vec::NonEmptyVec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::helpers::{io_error, non_empty_path, workspace_error};
use crate::{
    permission::{
        CanonicalPath, CanonicalToolInput, PathSelector, PermissionCheckSpec, PermissionTarget,
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
    /// Existing text to replace once.
    old_text: String,
    /// Replacement text.
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
                "Replace text once in a UTF-8 workspace file",
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
        let display_path = self.workspace.relative_display(&resolved);
        args.path = canonical_path.as_str().to_string();
        let canonical_input = CanonicalToolInput::new(serde_json::json!({
            "path": canonical_path.as_str(),
            "old_text": args.old_text,
            "new_text": args.new_text,
        }));
        Ok(TypedPreparation::new(
            PreparedEditFile {
                args,
                path: resolved,
            },
            canonical_input,
            NonEmptyVec::new(PermissionCheckSpec::new(PermissionTarget::Edit(
                PathSelector::exact(canonical_path),
            ))),
            ToolDisplay::new(format!("Edit file: {display_path}")),
        ))
    }

    async fn run_prepared(
        &self,
        prepared: PreparedEditFile,
        _ctx: &ToolContext,
    ) -> ToolOutput<EditFileOutput> {
        let PreparedEditFile { args, path } = prepared;
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(err) => return io_error("read", &path, err, &self.workspace),
        };

        // `old_text` must be unambiguous: zero matches gives the model nothing
        // to anchor on, and more than one would let us silently edit the wrong
        // occurrence. Force a unique match so the edit is deterministic.
        match content.matches(&args.old_text).count() {
            0 => {
                return ToolOutput::failure(
                    "text_not_found",
                    format!(
                        "`old_text` was not found in `{}`",
                        self.workspace.relative_display(&path)
                    ),
                );
            }
            1 => {}
            count => {
                return ToolOutput::failure(
                    "ambiguous_match",
                    format!(
                        "`old_text` matches {count} times in `{}`; \
                         include surrounding context so it is unique",
                        self.workspace.relative_display(&path)
                    ),
                );
            }
        }

        let edited = content.replacen(&args.old_text, &args.new_text, 1);
        if let Err(err) = tokio::fs::write(&path, edited.as_bytes()).await {
            return io_error("write", &path, err, &self.workspace);
        }

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
        Ok(
            if self
                .workspace
                .revalidate_target(&prepared.path)
                .await
                .is_ok()
            {
                PreparedInvocationState::Current
            } else {
                PreparedInvocationState::Stale
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use super::EditFile;
    use crate::test_support::TestDir;
    use crate::tool::{ToolContext, execute_for_test};

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
}
