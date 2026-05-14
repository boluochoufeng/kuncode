//! Built-in `exec_argv` tool.

use async_trait::async_trait;
use kuncode_core::{RiskFlag, ToolCapability, ToolEffect};
use serde_json::{Value, json};
use tokio::fs;

use crate::{
    Tool, ToolContext, ToolDescriptor, ToolError, ToolInput, ToolResult,
    tools::common::{
        CaptureLimits, cap_summary, ensure_in_lane, optional_str, optional_u64, relative_string, run_capture,
        save_combined_output_artifact, tool_result, truncate_utf8, workspace_error,
    },
};

/// Executes a subprocess from an argv array without invoking a shell.
///
/// The working directory must resolve inside the workspace and the active
/// execution lane. Long stdout/stderr are persisted as an artifact linked to
/// the call's `tool.started` event.
pub struct ExecArgvTool {
    descriptor: ToolDescriptor,
}

impl ExecArgvTool {
    /// Create an `exec_argv` tool with the Phase 2 descriptor.
    pub fn new() -> Self {
        Self { descriptor: descriptor() }
    }
}

impl Default for ExecArgvTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ExecArgvTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    fn risk_flags(&self, input: &ToolInput) -> Vec<RiskFlag> {
        let mut flags = self.descriptor.risk_flags.clone();
        if let Ok(argv) = parse_argv(&input.payload)
            && !is_trusted_argv(&argv)
            && !flags.contains(&RiskFlag::UntrustedCommand)
        {
            flags.push(RiskFlag::UntrustedCommand);
        }
        flags
    }

    async fn execute(&self, input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let argv = parse_argv(&input.payload)
            .map_err(|message| ToolError::InvalidInput { tool: self.descriptor.name.clone(), message })?;
        let timeout_ms = optional_u64(&input.payload, "timeout_ms").unwrap_or(ctx.limits.default_timeout_ms);
        if timeout_ms > ctx.limits.max_timeout_ms {
            return Err(ToolError::InvalidInput {
                tool: self.descriptor.name.clone(),
                message: format!("timeout_ms {timeout_ms} exceeds max_timeout_ms {}", ctx.limits.max_timeout_ms),
            });
        }

        let raw_cwd = optional_str(&input.payload, "cwd").unwrap_or(".");
        let cwd = ctx.workspace.resolve_existing_path(raw_cwd).await.map_err(workspace_error)?;
        ensure_in_lane(&cwd, ctx.lane, &self.descriptor.name)?;
        let metadata = fs::metadata(cwd.as_path())
            .await
            .map_err(|source| ToolError::Io { path: cwd.as_path().to_path_buf(), source })?;
        if !metadata.is_dir() {
            return Err(ToolError::Workspace {
                path: cwd.as_path().to_path_buf(),
                message: "cwd must be a directory".to_owned(),
            });
        }

        let program = argv[0].clone();
        let command_args = argv[1..].to_vec();
        let output = run_capture(
            &self.descriptor.name,
            &program,
            &command_args,
            cwd.as_path(),
            timeout_ms,
            ctx.cancel_token.clone(),
            CaptureLimits { stdout: ctx.limits.max_stdout_bytes, stderr: ctx.limits.max_stderr_bytes },
        )
        .await?;

        let stdout_truncated = output.stdout.truncated;
        let stderr_truncated = output.stderr.truncated;
        let needs_artifact = stdout_truncated || stderr_truncated;
        let content_ref = if needs_artifact {
            Some(save_combined_output_artifact(&ctx, "exec_argv.output", &output.stdout, &output.stderr).await?)
        } else {
            None
        };

        let stdout_text = String::from_utf8_lossy(&output.stdout.inline);
        let stderr_text = String::from_utf8_lossy(&output.stderr.inline);
        let (stdout_inline, stdout_inline_truncated) = truncate_utf8(&stdout_text, ctx.limits.max_inline_output_bytes);
        let (stderr_inline, stderr_inline_truncated) = truncate_utf8(&stderr_text, ctx.limits.max_inline_output_bytes);
        let inline_candidate = format!("stdout:\n{stdout_inline}\nstderr:\n{stderr_inline}");
        let (inline, inline_truncated) = truncate_utf8(&inline_candidate, ctx.limits.max_inline_output_bytes);
        let exit_code = output.status.code();
        let status_text = exit_code.map_or_else(|| "signal".to_owned(), |code| code.to_string());
        let artifact_text = if needs_artifact { ", artifact" } else { "" };
        let summary = cap_summary(format!(
            "exec exit {status_text} (stdout {} bytes, stderr {} bytes{artifact_text})",
            output.stdout.bytes, output.stderr.bytes,
        ));

        tool_result(
            &self.descriptor.name,
            summary,
            Some(inline),
            content_ref,
            json!({
                "argv": argv,
                "cwd": relative_string(&cwd),
                "exit_code": exit_code,
                "success": output.status.success(),
                "duration_ms": output.duration_ms,
                "stdout_bytes": output.stdout.bytes,
                "stderr_bytes": output.stderr.bytes,
                "stdout_truncated": stdout_truncated || stdout_inline_truncated,
                "stderr_truncated": stderr_truncated || stderr_inline_truncated,
                "inline_truncated": inline_truncated,
                "trusted_command": is_trusted_argv_value(&input.payload),
                "artifact": needs_artifact,
            }),
        )
    }
}

fn parse_argv(payload: &Value) -> Result<Vec<String>, String> {
    let values =
        payload.get("argv").and_then(Value::as_array).ok_or_else(|| "missing array field `argv`".to_owned())?;
    let mut argv = Vec::with_capacity(values.len());
    for value in values {
        let item = value.as_str().ok_or_else(|| "`argv` must contain only strings".to_owned())?;
        if item.is_empty() {
            return Err("`argv` entries must not be empty".to_owned());
        }
        argv.push(item.to_owned());
    }
    if argv.is_empty() {
        return Err("`argv` must not be empty".to_owned());
    }
    Ok(argv)
}

fn is_trusted_argv_value(payload: &Value) -> bool {
    parse_argv(payload).is_ok_and(|argv| is_trusted_argv(&argv))
}

fn is_trusted_argv(argv: &[String]) -> bool {
    match argv.first().map(String::as_str) {
        Some("cargo" | "go" | "python" | "npm" | "pnpm" | "bun" | "rustc" | "node") => true,
        Some("git") => is_trusted_git(argv),
        _ => false,
    }
}

fn is_trusted_git(argv: &[String]) -> bool {
    match argv.get(1).map(String::as_str) {
        Some("status" | "diff" | "log" | "show" | "branch") => true,
        Some("worktree") => matches!(argv.get(2).map(String::as_str), Some("list")),
        _ => false,
    }
}

fn descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: "exec_argv".to_owned(),
        description: "Execute a subprocess from an argv array without a shell.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "argv": {
                    "type": "array",
                    "minItems": 1,
                    "items": { "type": "string", "minLength": 1 }
                },
                "cwd": { "type": "string", "minLength": 1 },
                "timeout_ms": { "type": "integer", "minimum": 1 }
            },
            "required": ["argv"],
            "additionalProperties": false
        }),
        output_schema: None,
        effects: vec![ToolEffect::ExecuteProcess],
        default_capabilities: vec![ToolCapability::Verify, ToolCapability::Edit],
        risk_flags: vec![RiskFlag::LongRunning],
    }
}
