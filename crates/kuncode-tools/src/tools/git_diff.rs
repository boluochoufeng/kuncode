//! Built-in `git_diff` tool.

use async_trait::async_trait;
use kuncode_core::{ToolCapability, ToolEffect};
use serde_json::json;

use crate::{
    Tool, ToolContext, ToolDescriptor, ToolError, ToolInput, ToolResult,
    tools::common::{
        CaptureLimits, cap_summary, optional_str, relative_string, run_capture, save_captured_stream_artifact,
        tool_result, truncate_utf8, workspace_error,
    },
};

/// Runs `git diff` in the workspace root, optionally restricted to one path.
///
/// Any provided path is resolved through the workspace boundary before being
/// passed to git as a pathspec. Large diffs are persisted as artifacts.
pub struct GitDiffTool {
    descriptor: ToolDescriptor,
}

impl GitDiffTool {
    /// Create a `git_diff` tool with the Phase 2 descriptor.
    pub fn new() -> Self {
        Self { descriptor: descriptor() }
    }
}

impl Default for GitDiffTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for GitDiffTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn execute(&self, input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let pathspec = if let Some(raw_path) = optional_str(&input.payload, "path") {
            let resolved = ctx.workspace.resolve_existing_path(raw_path).await.map_err(workspace_error)?;
            Some(relative_string(&resolved))
        } else {
            None
        };

        let mut args = vec!["diff".to_owned()];
        if let Some(path) = &pathspec {
            args.push("--".to_owned());
            args.push(path.clone());
        }

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
                message: format!("git diff failed: {}", String::from_utf8_lossy(&output.stderr.inline).trim()),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout.inline);
        let bytes = output.stdout.bytes;
        let needs_artifact = output.stdout.truncated;
        let content_ref = if needs_artifact {
            Some(save_captured_stream_artifact(&ctx, "git_diff", &output.stdout).await?)
        } else {
            None
        };
        let (inline, truncated) = truncate_utf8(&stdout, ctx.limits.max_inline_output_bytes);
        let summary = cap_summary(if needs_artifact {
            format!("git diff ({bytes} bytes, artifact)")
        } else {
            format!("git diff ({bytes} bytes)")
        });

        tool_result(
            &self.descriptor.name,
            summary,
            Some(inline),
            content_ref,
            json!({
                "path": pathspec,
                "bytes": bytes,
                "truncated": truncated,
                "artifact": needs_artifact,
                "duration_ms": output.duration_ms,
            }),
        )
    }
}

fn descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: "git_diff".to_owned(),
        description: "Run `git diff` in the workspace root, optionally for one safe path.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "minLength": 1 }
            },
            "additionalProperties": false
        }),
        output_schema: None,
        effects: vec![ToolEffect::ReadWorkspace],
        default_capabilities: vec![ToolCapability::Explore, ToolCapability::Verify],
        risk_flags: vec![],
    }
}
