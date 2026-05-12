//! Built-in `write_file` tool.

use async_trait::async_trait;
use kuncode_core::{RiskFlag, ToolCapability, ToolEffect};
use serde_json::json;
use tokio::fs;

use crate::{
    Tool, ToolContext, ToolDescriptor, ToolError, ToolInput, ToolResult,
    tools::common::{cap_summary, relative_string, required_str, tool_result, workspace_error},
};

/// Writes UTF-8 text to a file inside the workspace.
///
/// Parent directories are not created by this tool. The target path is
/// resolved through `Workspace::resolve_write_path`, which verifies existing
/// files and new-file parents remain inside the workspace root.
pub struct WriteFileTool {
    descriptor: ToolDescriptor,
}

impl WriteFileTool {
    /// Create a `write_file` tool with the Phase 2 descriptor.
    pub fn new() -> Self {
        Self { descriptor: descriptor() }
    }
}

impl Default for WriteFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn execute(&self, input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let raw_path = required_str(&input.payload, &self.descriptor.name, "path")?;
        let content = required_str(&input.payload, &self.descriptor.name, "content")?;
        let path = ctx.workspace.resolve_write_path(raw_path).await.map_err(workspace_error)?;

        fs::write(path.as_path(), content)
            .await
            .map_err(|source| ToolError::Io { path: path.as_path().to_path_buf(), source })?;

        let bytes = content.len();
        let relative = relative_string(&path);
        let summary = cap_summary(format!("wrote {relative} ({bytes} bytes)"));

        tool_result(
            &self.descriptor.name,
            summary,
            None,
            None,
            json!({
                "path": relative,
                "bytes": bytes,
            }),
        )
    }
}

fn descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: "write_file".to_owned(),
        description: "Write UTF-8 text to an existing workspace directory.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "minLength": 1 },
                "content": { "type": "string" }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        }),
        output_schema: None,
        effects: vec![ToolEffect::WriteWorkspace],
        default_capabilities: vec![ToolCapability::Edit],
        risk_flags: vec![RiskFlag::MutatesWorkspace],
    }
}
