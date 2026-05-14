//! Built-in `git_status` tool.

use async_trait::async_trait;
use kuncode_core::{ToolCapability, ToolEffect};
use serde_json::json;

use crate::{
    Tool, ToolContext, ToolDescriptor, ToolError, ToolInput, ToolResult,
    tools::common::{CaptureLimits, cap_summary, count_non_empty_lines, run_capture, tool_result, truncate_utf8},
};

/// Runs `git status --short` in the workspace root.
///
/// The tool does not accept arbitrary git arguments. It returns git's short
/// status output inline, truncated to the invocation's inline output budget.
pub struct GitStatusTool {
    descriptor: ToolDescriptor,
}

impl GitStatusTool {
    /// Create a `git_status` tool with the Phase 2 descriptor.
    pub fn new() -> Self {
        Self { descriptor: descriptor() }
    }
}

impl Default for GitStatusTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for GitStatusTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn execute(&self, _input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let args = vec!["status".to_owned(), "--short".to_owned()];
        let output = run_capture(
            &self.descriptor.name,
            "git",
            &args,
            ctx.workspace.root(),
            ctx.limits.default_timeout_ms,
            ctx.cancel_token.clone(),
            CaptureLimits { stdout: ctx.limits.max_inline_output_bytes, stderr: ctx.limits.max_stderr_bytes },
        )
        .await?;

        if !output.status.success() {
            return Err(ToolError::Process {
                message: format!("git status failed: {}", String::from_utf8_lossy(&output.stderr.inline).trim()),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout.inline);
        // `stdout.inline` may be only a prefix; count from the full captured
        // stream so metadata stays correct when inline output is truncated.
        let changed_files = count_non_empty_lines(&output.stdout, &self.descriptor.name).await?;
        let (inline, truncated) = truncate_utf8(&stdout, ctx.limits.max_inline_output_bytes);
        let summary = cap_summary(format!("git status ({changed_files} changed files)"));

        tool_result(
            &self.descriptor.name,
            summary,
            Some(inline),
            None,
            json!({
                "changed_files": changed_files,
                "stdout_bytes": output.stdout.bytes,
                "truncated": truncated || output.stdout.truncated,
                "duration_ms": output.duration_ms,
            }),
        )
    }
}

fn descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: "git_status".to_owned(),
        description: "Run `git status --short` in the workspace root.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        output_schema: None,
        effects: vec![ToolEffect::ReadWorkspace],
        default_capabilities: vec![ToolCapability::Explore, ToolCapability::Verify],
        risk_flags: vec![],
    }
}
