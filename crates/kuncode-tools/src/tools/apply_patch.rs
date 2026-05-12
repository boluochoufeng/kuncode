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
                return Err(ToolError::Process { message: format!("duplicate patch target `{}`", target.path) });
            }

            if target.create {
                let path = ctx.workspace.resolve_write_path(&target.path).await.map_err(workspace_error)?;
                if fs::metadata(path.as_path()).await.is_ok() {
                    return Err(ToolError::Process { message: format!("new file `{}` already exists", target.path) });
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
    let old_lines: Vec<&str> = current.lines().collect();
    let mut out = Vec::new();
    let mut old_idx = 0_usize;

    for hunk in &patch.hunks {
        let hunk_start = usize::try_from(hunk.old_range.start)
            .map_err(|_| ToolError::Process { message: "patch hunk start is too large".to_owned() })?;
        let target_idx = hunk_start.saturating_sub(1);
        while old_idx < target_idx {
            let line = old_lines
                .get(old_idx)
                .ok_or_else(|| ToolError::Process { message: "patch hunk starts past end of file".to_owned() })?;
            out.push((*line).to_owned());
            old_idx += 1;
        }

        for line in &hunk.lines {
            match line {
                Line::Context(expected) => {
                    ensure_old_line(&old_lines, old_idx, expected)?;
                    out.push((*expected).to_owned());
                    old_idx += 1;
                }
                Line::Remove(expected) => {
                    ensure_old_line(&old_lines, old_idx, expected)?;
                    old_idx += 1;
                }
                Line::Add(added) => out.push((*added).to_owned()),
            }
        }
    }

    while old_idx < old_lines.len() {
        out.push(old_lines[old_idx].to_owned());
        old_idx += 1;
    }

    Ok(join_patch_lines(&out, patch.end_newline))
}

fn apply_new_file(patch: &Patch<'_>) -> Result<String, ToolError> {
    let mut out = Vec::new();
    for hunk in &patch.hunks {
        for line in &hunk.lines {
            match line {
                Line::Add(added) => out.push((*added).to_owned()),
                Line::Context(_) | Line::Remove(_) => {
                    return Err(ToolError::Process {
                        message: "new-file patches may only contain added lines".to_owned(),
                    });
                }
            }
        }
    }
    Ok(join_patch_lines(&out, patch.end_newline))
}

fn ensure_old_line(old_lines: &[&str], old_idx: usize, expected: &str) -> Result<(), ToolError> {
    let actual = old_lines
        .get(old_idx)
        .ok_or_else(|| ToolError::Process { message: format!("patch expected `{expected}` past end of file") })?;
    if *actual == expected {
        Ok(())
    } else {
        Err(ToolError::Process {
            message: format!("patch context mismatch at line {}: expected `{expected}`, found `{actual}`", old_idx + 1),
        })
    }
}

fn join_patch_lines(lines: &[String], end_newline: bool) -> String {
    let mut out = lines.join("\n");
    if end_newline {
        out.push('\n');
    }
    out
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
