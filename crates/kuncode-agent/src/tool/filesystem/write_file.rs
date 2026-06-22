//! The `write_file` tool: write a UTF-8 file inside the workspace.

use std::path::Path;

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::helpers::{io_error, non_empty_path, rule_path, workspace_error};
use crate::{
    permission::{PermissionAction, PermissionRequest},
    tool::{ToolContext, ToolOutput, TypedTool, definition_for},
    workspace::Workspace,
};

/// Arguments accepted by the [`WriteFile`] tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteFileArgs {
    /// Workspace-relative or absolute file path to write.
    path: String,
    /// UTF-8 content to write to the file.
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
                "Write a UTF-8 workspace file",
            ),
            workspace,
        }
    }
}

#[async_trait]
impl TypedTool for WriteFile {
    type Args = WriteFileArgs;
    type Output = WriteFileOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn permission(&self, args: &WriteFileArgs, _ctx: &ToolContext) -> PermissionRequest {
        let path = rule_path(&self.workspace, &args.path);
        PermissionRequest::new(
            "write_file",
            PermissionAction::Write,
            Some(path.clone()),
            format!("Write file: {path}"),
        )
    }

    async fn run(&self, args: WriteFileArgs, _ctx: &ToolContext) -> ToolOutput<WriteFileOutput> {
        let path = match non_empty_path(&args.path) {
            Ok(path) => path,
            Err(output) => return output,
        };

        let resolved = match self.workspace.resolve_for_write(path).await {
            Ok(path) => path,
            Err(err) => return workspace_error(err),
        };

        if is_dir(&resolved).await {
            return ToolOutput::failure(
                "invalid_path",
                format!(
                    "`{}` is a directory",
                    self.workspace.relative_display(&resolved)
                ),
            );
        }

        if let Err(err) = tokio::fs::write(&resolved, args.content.as_bytes()).await {
            return io_error("write", &resolved, err, &self.workspace);
        }

        ToolOutput::success(WriteFileOutput {
            path: self.workspace.relative_display(&resolved),
            bytes: args.content.len(),
        })
    }
}

/// Non-blocking `stat` predicate. A missing path or stat error resolves to
/// `false`, matching the semantics of [`Path::is_dir`].
async fn is_dir(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .map(|meta| meta.is_dir())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::WriteFile;
    use crate::tool::filesystem::test_support::TestDir;
    use crate::tool::{Tool, ToolContext};

    #[tokio::test]
    async fn write_file_rejects_missing_parent() {
        let tmp = TestDir::new();
        let tool = WriteFile::new(tmp.workspace().await);

        let output = tool
            .call(
                serde_json::json!({
                    "path": "missing/new.txt",
                    "content": "hello"
                }),
                &ToolContext::new(),
            )
            .await
            .expect("no harness-level error");

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "workspace_path"
        );
    }

    #[tokio::test]
    async fn write_file_writes_inside_workspace() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join("src")).expect("directory should be created");
        let tool = WriteFile::new(tmp.workspace().await);

        let output = tool
            .call(
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
