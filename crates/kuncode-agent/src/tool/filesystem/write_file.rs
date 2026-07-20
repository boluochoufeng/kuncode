//! The `write_file` tool: write a UTF-8 file inside the workspace.

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
                "Write a UTF-8 workspace file",
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
        Ok(TypedPreparation::new(
            PreparedWriteFile {
                args,
                path: resolved,
            },
            canonical_input,
            NonEmptyVec::new(PermissionCheckSpec::new(PermissionTarget::Edit(
                PathSelector::exact(canonical_path),
            ))),
            ToolDisplay::new(format!("Write file: {display_path}")),
        ))
    }

    async fn run_prepared(
        &self,
        prepared: PreparedWriteFile,
        _ctx: &ToolContext,
    ) -> ToolOutput<WriteFileOutput> {
        let PreparedWriteFile { args, path } = prepared;
        if let Err(error) = tokio::fs::write(&path, args.content.as_bytes()).await {
            return io_error("write", &path, error, &self.workspace);
        }
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

    use super::WriteFile;
    use crate::test_support::TestDir;
    use crate::tool::{ToolContext, execute_for_test};

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
