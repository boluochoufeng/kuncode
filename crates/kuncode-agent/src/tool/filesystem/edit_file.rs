//! The `edit_file` tool: replace one unique occurrence of text in a file.

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::helpers::{io_error, is_file, non_empty_path, rule_path, workspace_error};
use crate::{
    permission::{PermissionAction, PermissionRequest},
    tool::{ToolContext, ToolOutput, TypedTool, definition_for},
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
    type Output = EditFileOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn permission(&self, args: &EditFileArgs, _ctx: &ToolContext) -> PermissionRequest {
        let path = rule_path(&self.workspace, &args.path);
        PermissionRequest::new(
            "edit_file",
            PermissionAction::Write,
            Some(path.clone()),
            format!("Edit file: {path}"),
        )
    }

    async fn run(&self, args: EditFileArgs, _ctx: &ToolContext) -> ToolOutput<EditFileOutput> {
        let path = match non_empty_path(&args.path) {
            Ok(path) => path,
            Err(output) => return output,
        };

        if args.old_text.is_empty() {
            return ToolOutput::failure("invalid_arguments", "`old_text` must not be empty");
        }

        let resolved = match self.workspace.resolve_existing(path).await {
            Ok(path) => path,
            Err(err) => return workspace_error(err),
        };

        if !is_file(&resolved).await {
            return ToolOutput::failure(
                "invalid_path",
                format!(
                    "`{}` is not a file",
                    self.workspace.relative_display(&resolved)
                ),
            );
        }

        let content = match tokio::fs::read_to_string(&resolved).await {
            Ok(content) => content,
            Err(err) => return io_error("read", &resolved, err, &self.workspace),
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
                        self.workspace.relative_display(&resolved)
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
                        self.workspace.relative_display(&resolved)
                    ),
                );
            }
        }

        let edited = content.replacen(&args.old_text, &args.new_text, 1);
        if let Err(err) = tokio::fs::write(&resolved, edited.as_bytes()).await {
            return io_error("write", &resolved, err, &self.workspace);
        }

        ToolOutput::success(EditFileOutput {
            path: self.workspace.relative_display(&resolved),
            replacements: 1,
            bytes: edited.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::EditFile;
    use crate::test_support::TestDir;
    use crate::tool::{Tool, ToolContext};

    #[tokio::test]
    async fn edit_file_replaces_once() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("notes.txt"), "target rest").expect("file should be written");
        let tool = EditFile::new(tmp.workspace().await);

        let output = tool
            .call(
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

        let output = tool
            .call(
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

        let output = tool
            .call(
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
