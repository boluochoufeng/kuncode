//! Built-in `apply_patch` tool.

use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
};

use async_trait::async_trait;
use kuncode_core::{RiskFlag, ToolCapability, ToolEffect};
use patch::{Line, Patch};
use serde_json::json;
use tokio::fs;

use crate::{
    Tool, ToolContext, ToolDescriptor, ToolError, ToolInput, ToolResult,
    tools::common::{cap_summary, relative_string, required_str, tool_result, workspace_error},
};

/// Applies a constrained text-only unified diff inside the workspace.
///
/// Phase 2 intentionally supports only modifying existing UTF-8 files and
/// creating new UTF-8 files. Deletion, rename, binary patches, path escapes
/// and context mismatches are rejected.
pub struct ApplyPatchTool {
    descriptor: ToolDescriptor,
}

impl ApplyPatchTool {
    /// Create an `apply_patch` tool with the Phase 2 descriptor.
    pub fn new() -> Self {
        Self { descriptor: descriptor() }
    }
}

impl Default for ApplyPatchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn execute(&self, input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let patch_text = required_str(&input.payload, &self.descriptor.name, "patch")?;
        let patches = Patch::from_multiple(patch_text).map_err(|err| ToolError::InvalidInput {
            tool: self.descriptor.name.clone(),
            message: format!("invalid unified diff: {err}"),
        })?;

        let mut changes = Vec::new();
        let mut seen = HashSet::new();
        for patch in patches {
            let target = patch_target(&patch, &self.descriptor.name)?;
            if !seen.insert(target.path.clone()) {
                return Err(patch_invalid(format!("duplicate patch target `{}`", target.path)));
            }

            if target.create {
                let path = ctx.workspace.resolve_write_path(&target.path).await.map_err(workspace_error)?;
                if fs::metadata(path.as_path()).await.is_ok() {
                    return Err(patch_invalid(format!("new file `{}` already exists", target.path)));
                }
                let new_content = apply_new_file(&patch)?;
                changes.push(PreparedPatch {
                    path: path.as_path().to_path_buf(),
                    relative: relative_string(&path),
                    original: None,
                    new_content,
                });
            } else {
                let path = ctx.workspace.resolve_read_file(&target.path).await.map_err(workspace_error)?;
                let current = fs::read_to_string(path.as_path())
                    .await
                    .map_err(|source| ToolError::Io { path: path.as_path().to_path_buf(), source })?;
                let new_content = apply_existing_file(&patch, &current)?;
                changes.push(PreparedPatch {
                    path: path.as_path().to_path_buf(),
                    relative: relative_string(&path),
                    original: Some(current),
                    new_content,
                });
            }
        }

        let touched = write_prepared_changes(&changes).await?;
        let touched_count = touched.len();
        let summary = cap_summary(format!("applied patch ({touched_count} files)"));
        tool_result(
            &self.descriptor.name,
            summary,
            None,
            None,
            json!({
                "touched_paths": touched,
                "touched_count": touched_count,
            }),
        )
    }
}

struct PatchTarget {
    path: String,
    create: bool,
}

struct PreparedPatch {
    path: PathBuf,
    relative: String,
    original: Option<String>,
    new_content: String,
}

async fn write_prepared_changes(changes: &[PreparedPatch]) -> Result<Vec<String>, ToolError> {
    let mut written = Vec::new();
    for change in changes {
        let write_result = fs::write(&change.path, &change.new_content).await;
        written.push(change);
        if let Err(source) = write_result {
            // Validation is already complete by this point. If actual writes
            // fail, best-effort rollback keeps the workspace from being left in
            // a partially-applied state.
            if let Err(rollback) = rollback_changes(&written).await {
                return Err(ToolError::Internal {
                    tool: "apply_patch".to_owned(),
                    message: format!(
                        "failed to write `{}`: {source}; rollback also failed: {}",
                        change.path.display(),
                        rollback.summary()
                    ),
                });
            }
            return Err(ToolError::Io { path: change.path.clone(), source });
        }
    }

    Ok(changes.iter().map(|change| change.relative.clone()).collect())
}

async fn rollback_changes(changes: &[&PreparedPatch]) -> Result<(), ToolError> {
    for change in changes.iter().rev() {
        // Reverse order matters for create-then-modify style batches: undo the
        // most recent visible write first.
        if let Some(original) = &change.original {
            fs::write(&change.path, original)
                .await
                .map_err(|source| ToolError::Io { path: change.path.clone(), source })?;
        } else {
            match fs::remove_file(&change.path).await {
                Ok(()) => {}
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(ToolError::Io { path: change.path.clone(), source }),
            }
        }
    }
    Ok(())
}

fn patch_target(parsed_patch: &Patch<'_>, tool: &str) -> Result<PatchTarget, ToolError> {
    let old = normalize_patch_path(&parsed_patch.old.path, PatchSide::Old, tool)?;
    let new = normalize_patch_path(&parsed_patch.new.path, PatchSide::New, tool)?;

    match (old, new) {
        (None, Some(target_path)) => Ok(PatchTarget { path: target_path, create: true }),
        (Some(_), None) => Err(ToolError::InvalidInput {
            tool: tool.to_owned(),
            message: "deleting files is not supported by apply_patch".to_owned(),
        }),
        (Some(old), Some(new)) if old == new => Ok(PatchTarget { path: old, create: false }),
        (Some(old), Some(new)) => Err(ToolError::InvalidInput {
            tool: tool.to_owned(),
            message: format!("renaming files is not supported (`{old}` -> `{new}`)"),
        }),
        (None, None) => {
            Err(ToolError::InvalidInput { tool: tool.to_owned(), message: "patch must have a target file".to_owned() })
        }
    }
}

#[derive(Clone, Copy)]
enum PatchSide {
    Old,
    New,
}

fn normalize_patch_path(path: &str, side: PatchSide, tool: &str) -> Result<Option<String>, ToolError> {
    if path == "/dev/null" {
        return Ok(None);
    }

    let stripped = match side {
        PatchSide::Old => path.strip_prefix("a/").unwrap_or(path),
        PatchSide::New => path.strip_prefix("b/").unwrap_or(path),
    };
    let as_path = Path::new(stripped);
    if as_path.is_absolute()
        || as_path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
    {
        return Err(ToolError::InvalidInput { tool: tool.to_owned(), message: format!("unsafe patch path `{path}`") });
    }

    Ok(Some(stripped.to_owned()))
}

fn apply_existing_file(patch: &Patch<'_>, current: &str) -> Result<String, ToolError> {
    let old_lines = split_preserving_line_endings(current);
    let default_line_ending = default_line_ending(&old_lines);
    let mut out = Vec::new();
    let mut old_idx = 0_usize;

    for hunk in &patch.hunks {
        let hunk_start =
            usize::try_from(hunk.old_range.start).map_err(|_| patch_invalid("patch hunk start is too large"))?;
        let target_idx = hunk_start.saturating_sub(1);
        if target_idx < old_idx {
            return Err(patch_invalid("patch hunks must be sorted by old range and must not overlap"));
        }
        // Copy untouched source lines up to the hunk. Existing source lines
        // carry their original line endings so patches do not reformat CRLF or
        // mixed-ending files as a side effect.
        while old_idx < target_idx {
            let line = old_lines.get(old_idx).ok_or_else(|| patch_invalid("patch hunk starts past end of file"))?;
            out.push((*line).into());
            old_idx += 1;
        }

        for line in &hunk.lines {
            match line {
                Line::Context(expected) => {
                    let line = ensure_old_line(&old_lines, old_idx, expected)?;
                    out.push(line.into());
                    old_idx += 1;
                }
                Line::Remove(expected) => {
                    ensure_old_line(&old_lines, old_idx, expected)?;
                    old_idx += 1;
                }
                Line::Add(added) => {
                    out.push(PatchedLine { text: (*added).to_owned(), line_ending: default_line_ending });
                }
            }
        }
    }

    while old_idx < old_lines.len() {
        out.push(old_lines[old_idx].into());
        old_idx += 1;
    }

    Ok(render_patched_lines(out, patch.end_newline, default_line_ending))
}

fn apply_new_file(patch: &Patch<'_>) -> Result<String, ToolError> {
    let mut out = Vec::new();
    let mut previous_new_start = None;
    for hunk in &patch.hunks {
        if let Some(previous) = previous_new_start
            && hunk.new_range.start < previous
        {
            return Err(patch_invalid("patch hunks must be sorted by new range"));
        }
        previous_new_start = Some(hunk.new_range.start);
        for line in &hunk.lines {
            match line {
                Line::Add(added) => out.push(PatchedLine { text: (*added).to_owned(), line_ending: "\n" }),
                Line::Context(_) | Line::Remove(_) => {
                    return Err(patch_invalid("new-file patches may only contain added lines"));
                }
            }
        }
    }
    Ok(render_patched_lines(out, patch.end_newline, "\n"))
}

#[derive(Clone, Copy)]
struct SourceLine<'a> {
    text: &'a str,
    line_ending: &'static str,
}

struct PatchedLine {
    text: String,
    line_ending: &'static str,
}

impl From<SourceLine<'_>> for PatchedLine {
    fn from(line: SourceLine<'_>) -> Self {
        Self { text: line.text.to_owned(), line_ending: line.line_ending }
    }
}

fn split_preserving_line_endings(content: &str) -> Vec<SourceLine<'_>> {
    // `str::lines()` intentionally drops terminators, which would silently
    // normalize CRLF/mixed-ending files when rendering the patched file. Keep
    // line text and terminator separate so context matching ignores terminators
    // but output preserves them.
    let bytes = content.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0_usize;
    let mut idx = 0_usize;

    while idx < bytes.len() {
        match bytes[idx] {
            b'\n' => {
                let is_crlf = idx > start && bytes[idx - 1] == b'\r';
                let text_end = if is_crlf { idx - 1 } else { idx };
                lines.push(SourceLine {
                    text: &content[start..text_end],
                    line_ending: if is_crlf { "\r\n" } else { "\n" },
                });
                idx += 1;
                start = idx;
            }
            b'\r' if idx + 1 == bytes.len() || bytes[idx + 1] != b'\n' => {
                lines.push(SourceLine { text: &content[start..idx], line_ending: "\r" });
                idx += 1;
                start = idx;
            }
            _ => {
                idx += 1;
            }
        }
    }

    if start < content.len() {
        lines.push(SourceLine { text: &content[start..], line_ending: "" });
    }

    lines
}

fn default_line_ending(lines: &[SourceLine<'_>]) -> &'static str {
    lines.iter().find_map(|line| (!line.line_ending.is_empty()).then_some(line.line_ending)).unwrap_or("\n")
}

fn ensure_old_line<'a>(
    old_lines: &'a [SourceLine<'a>],
    old_idx: usize,
    expected: &str,
) -> Result<SourceLine<'a>, ToolError> {
    let actual =
        old_lines.get(old_idx).ok_or_else(|| patch_invalid(format!("patch expected `{expected}` past end of file")))?;
    if actual.text == expected {
        Ok(*actual)
    } else {
        Err(patch_invalid(format!(
            "patch context mismatch at line {}: expected `{expected}`, found `{}`",
            old_idx + 1,
            actual.text
        )))
    }
}

fn render_patched_lines(mut lines: Vec<PatchedLine>, end_newline: bool, default_line_ending: &'static str) -> String {
    // The patch crate exposes only final-file newline presence, not a per-added
    // line ending. Added lines therefore use the file's first observed line
    // ending, while untouched/context lines keep their original terminators.
    if let Some(last) = lines.last_mut() {
        if end_newline && last.line_ending.is_empty() {
            last.line_ending = default_line_ending;
        } else if !end_newline {
            last.line_ending = "";
        }
    }

    let mut out = String::new();
    for line in lines {
        out.push_str(&line.text);
        out.push_str(line.line_ending);
    }
    out
}

fn patch_invalid(message: impl Into<String>) -> ToolError {
    ToolError::InvalidInput { tool: "apply_patch".to_owned(), message: message.into() }
}

fn descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: "apply_patch".to_owned(),
        description: "Apply a constrained text unified diff inside the workspace.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "patch": { "type": "string", "minLength": 1 }
            },
            "required": ["patch"],
            "additionalProperties": false
        }),
        output_schema: None,
        effects: vec![ToolEffect::WriteWorkspace],
        default_capabilities: vec![ToolCapability::Edit],
        risk_flags: vec![RiskFlag::MutatesWorkspace],
    }
}
