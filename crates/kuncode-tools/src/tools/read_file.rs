//! Built-in `read_file` tool.

use std::fmt::Write as _;

use async_trait::async_trait;
use kuncode_core::{ToolCapability, ToolEffect};
use serde_json::json;
use tokio::fs;

use crate::{
    Tool, ToolContext, ToolDescriptor, ToolError, ToolInput, ToolResult,
    tools::common::{
        cap_summary, optional_u64, relative_string, required_str, tool_result, truncate_utf8, workspace_error,
    },
};

const DEFAULT_RANGE_LIMIT_LINES: u64 = 200;
const MAX_RANGE_LIMIT_LINES: u64 = 1_000;

/// Reads a UTF-8 text file from inside the workspace.
///
/// The tool always resolves paths through `Workspace::resolve_read_file` before
/// touching the filesystem, so path escapes, symlink escapes, binary files and
/// oversized files are rejected by the workspace layer.
pub struct ReadFileTool {
    descriptor: ToolDescriptor,
}

impl ReadFileTool {
    /// Create a `read_file` tool with the Phase 2 descriptor.
    pub fn new() -> Self {
        Self { descriptor: descriptor() }
    }
}

impl Default for ReadFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn execute(&self, input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let raw_path = required_str(&input.payload, &self.descriptor.name, "path")?;
        let path = ctx.workspace.resolve_read_file(raw_path).await.map_err(workspace_error)?;
        let content = fs::read_to_string(path.as_path())
            .await
            .map_err(|source| ToolError::Io { path: path.as_path().to_path_buf(), source })?;
        let bytes = content.len();
        let requested_range = requested_range(&input.payload, &self.descriptor.name)?;
        let total_lines = count_lines(&content);
        let selected = if let Some(range) = requested_range {
            select_numbered_lines(&content, range)?
        } else {
            SelectedContent {
                content: content.clone(),
                selected_bytes: bytes,
                start_line: u64::from(total_lines != 0),
                end_line: Some(u64::try_from(total_lines).map_err(|_| ToolError::InvalidInput {
                    tool: self.descriptor.name.clone(),
                    message: "file has too many lines for metadata".to_owned(),
                })?),
                returned_lines: u64::try_from(total_lines).map_err(|_| ToolError::InvalidInput {
                    tool: self.descriptor.name.clone(),
                    message: "file has too many lines for metadata".to_owned(),
                })?,
                total_lines: u64::try_from(total_lines).map_err(|_| ToolError::InvalidInput {
                    tool: self.descriptor.name.clone(),
                    message: "file has too many lines for metadata".to_owned(),
                })?,
                range_truncated: false,
                line_numbered: false,
            }
        };
        let (inline, truncated) = truncate_utf8(&selected.content, ctx.limits.max_inline_output_bytes);
        let relative = relative_string(&path);
        let summary = if requested_range.is_some() && selected.returned_lines == 0 {
            cap_summary(format!(
                "read {relative} from line {} (0 lines of {})",
                selected.start_line, selected.total_lines
            ))
        } else if requested_range.is_some() {
            cap_summary(format!(
                "read {relative} lines {}-{} of {} ({} selected bytes, {bytes} file bytes)",
                selected.start_line,
                selected.end_line.unwrap_or(selected.start_line),
                selected.total_lines,
                selected.selected_bytes,
            ))
        } else {
            cap_summary(format!("read {relative} ({bytes} bytes)"))
        };

        tool_result(
            &self.descriptor.name,
            summary,
            Some(inline),
            None,
            json!({
                "path": relative,
                "bytes": bytes,
                "selected_bytes": selected.selected_bytes,
                "truncated": truncated,
                "range_truncated": selected.range_truncated,
                "line_numbered": selected.line_numbered,
                "start_line": selected.start_line,
                "end_line": selected.end_line,
                "returned_lines": selected.returned_lines,
                "total_lines": selected.total_lines,
            }),
        )
    }
}

#[derive(Clone, Copy)]
struct ReadRange {
    offset: u64,
    limit: u64,
}

struct SelectedContent {
    content: String,
    selected_bytes: usize,
    start_line: u64,
    end_line: Option<u64>,
    returned_lines: u64,
    total_lines: u64,
    range_truncated: bool,
    line_numbered: bool,
}

fn requested_range(payload: &serde_json::Value, tool: &str) -> Result<Option<ReadRange>, ToolError> {
    let has_offset = payload.get("offset").is_some();
    let has_limit = payload.get("limit").is_some();
    if !has_offset && !has_limit {
        return Ok(None);
    }

    let offset = optional_u64(payload, "offset").unwrap_or(1);
    if offset == 0 {
        return Err(ToolError::InvalidInput {
            tool: tool.to_owned(),
            message: "`offset` must be at least 1".to_owned(),
        });
    }

    let limit = optional_u64(payload, "limit").unwrap_or(DEFAULT_RANGE_LIMIT_LINES);
    if limit == 0 || limit > MAX_RANGE_LIMIT_LINES {
        return Err(ToolError::InvalidInput {
            tool: tool.to_owned(),
            message: format!("`limit` must be between 1 and {MAX_RANGE_LIMIT_LINES}"),
        });
    }

    Ok(Some(ReadRange { offset, limit }))
}

fn select_numbered_lines(content: &str, range: ReadRange) -> Result<SelectedContent, ToolError> {
    let start_index = usize::try_from(range.offset.saturating_sub(1)).map_err(|_| ToolError::InvalidInput {
        tool: "read_file".to_owned(),
        message: "`offset` is too large for this platform".to_owned(),
    })?;
    let limit = usize::try_from(range.limit).map_err(|_| ToolError::InvalidInput {
        tool: "read_file".to_owned(),
        message: "`limit` is too large for this platform".to_owned(),
    })?;

    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let total_lines = lines.len();
    let start = start_index.min(total_lines);
    let end = start.saturating_add(limit).min(total_lines);
    let returned_lines = end.saturating_sub(start);
    let selected_bytes = lines[start..end].iter().map(|line| line.len()).sum();
    let start_line = if returned_lines == 0 { range.offset } else { usize_to_u64(start + 1)? };
    let end_line = if returned_lines == 0 { None } else { Some(usize_to_u64(end)?) };

    Ok(SelectedContent {
        content: numbered_lines(&lines[start..end], start + 1),
        selected_bytes,
        start_line,
        end_line,
        returned_lines: usize_to_u64(returned_lines)?,
        total_lines: usize_to_u64(total_lines)?,
        range_truncated: end < total_lines,
        line_numbered: true,
    })
}

fn numbered_lines(lines: &[&str], start_line: usize) -> String {
    let mut out = String::new();
    for (idx, line) in lines.iter().enumerate() {
        let _ = write!(out, "{} | {line}", start_line + idx);
        if !line.ends_with('\n') && idx + 1 < lines.len() {
            out.push('\n');
        }
    }
    out
}

fn count_lines(content: &str) -> usize {
    content.split_inclusive('\n').count()
}

fn usize_to_u64(value: usize) -> Result<u64, ToolError> {
    u64::try_from(value).map_err(|_| ToolError::InvalidInput {
        tool: "read_file".to_owned(),
        message: "line count is too large for metadata".to_owned(),
    })
}

fn descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: "read_file".to_owned(),
        description: "Read a UTF-8 text file inside the workspace.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "minLength": 1 },
                "offset": { "type": "integer", "minimum": 1 },
                "limit": { "type": "integer", "minimum": 1, "maximum": MAX_RANGE_LIMIT_LINES }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        output_schema: None,
        effects: vec![ToolEffect::ReadWorkspace],
        default_capabilities: vec![ToolCapability::Explore, ToolCapability::Edit],
        risk_flags: vec![],
    }
}
