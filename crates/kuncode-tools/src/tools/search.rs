//! Built-in `search` tool.

use std::{io::BufRead, path::Path, process::Stdio, time::Duration};

use async_trait::async_trait;
use ignore::WalkBuilder;
use kuncode_core::{ToolCapability, ToolEffect};
use regex::Regex;
use serde_json::json;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
};
use tokio_util::sync::CancellationToken;

use crate::{
    Tool, ToolContext, ToolDescriptor, ToolError, ToolInput, ToolResult,
    tools::common::{
        cap_summary, optional_str, optional_u64, path_string, relative_string, required_str, save_artifact,
        tool_result, truncate_utf8, workspace_error,
    },
};

const DEFAULT_MAX_RESULTS: usize = 50;
const MAX_RESULTS_LIMIT: usize = 1_000;
const MAX_SNIPPET_BYTES: usize = 240;

/// Searches workspace text using `rg` when available, with a Rust fallback.
///
/// Results are returned inline as compact `path:line:snippet` entries. The
/// fallback uses the same workspace path validation and default ignored
/// directory set as the rest of the built-in tools.
pub struct SearchTool {
    descriptor: ToolDescriptor,
    prefer_rg: bool,
}

impl SearchTool {
    /// Create a `search` tool that prefers `rg` and falls back to Rust search.
    pub fn new() -> Self {
        Self { descriptor: descriptor(), prefer_rg: true }
    }

    /// Create a `search` tool with `rg` explicitly enabled or disabled.
    ///
    /// Disabling `rg` is useful for deterministic fallback tests and for
    /// restricted deployments where spawning external search is not desired.
    pub fn with_rg_enabled(prefer_rg: bool) -> Self {
        Self { descriptor: descriptor(), prefer_rg }
    }
}

impl Default for SearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SearchTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn execute(&self, input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let query = required_str(&input.payload, &self.descriptor.name, "query")?;
        let regex = Regex::new(query).map_err(|err| ToolError::InvalidInput {
            tool: self.descriptor.name.clone(),
            message: format!("invalid regex `{query}`: {err}"),
        })?;
        let max_results = optional_u64(&input.payload, "max_results")
            .map(usize::try_from)
            .transpose()
            .map_err(|_| ToolError::InvalidInput {
                tool: self.descriptor.name.clone(),
                message: "`max_results` is too large for this platform".to_owned(),
            })?
            .unwrap_or(DEFAULT_MAX_RESULTS);
        let raw_path = optional_str(&input.payload, "path").unwrap_or(".");
        let search_path = ctx.workspace.resolve_existing_path(raw_path).await.map_err(workspace_error)?;
        let display_path = relative_string(&search_path);

        let search = if self.prefer_rg {
            match rg_search(
                query,
                &search_path,
                ctx.workspace.root(),
                max_results,
                ctx.limits.max_inline_output_bytes,
                ctx.limits.default_timeout_ms,
                ctx.cancel_token.clone(),
            )
            .await?
            {
                Some(found) => found,
                None => rust_search(&regex, &search_path, ctx.workspace.root(), max_results, &ctx)?,
            }
        } else {
            rust_search(&regex, &search_path, ctx.workspace.root(), max_results, &ctx)?
        };

        let (inline, inline_truncated) = truncate_utf8(&search.selected, ctx.limits.max_inline_output_bytes);
        let inline_truncated = inline_truncated || search.inline_truncated;
        let content_ref = if inline_truncated {
            Some(save_artifact(&ctx, "search.results", search.selected.as_bytes()).await?)
        } else {
            None
        };

        let summary = cap_summary(format!(
            "search `{query}` in {display_path} ({} matches{})",
            search.matches,
            if search.truncated || inline_truncated { ", truncated" } else { "" }
        ));

        tool_result(
            &self.descriptor.name,
            summary,
            Some(inline),
            content_ref,
            json!({
                "query": query,
                "path": display_path,
                "matches": search.matches,
                "truncated": search.truncated,
                "inline_truncated": inline_truncated,
                "snippet_truncated": search.snippet_truncated,
                "content_ref": content_ref,
                "backend": search.backend,
            }),
        )
    }
}

struct SearchOutput {
    selected: String,
    matches: usize,
    truncated: bool,
    inline_truncated: bool,
    snippet_truncated: bool,
    backend: &'static str,
}

struct SearchCollector {
    lines: Vec<String>,
    selected_bytes: usize,
    max_results: usize,
    max_inline_bytes: usize,
    truncated: bool,
    inline_truncated: bool,
    snippet_truncated: bool,
}

impl SearchCollector {
    fn new(max_results: usize, max_inline_bytes: usize) -> Self {
        Self {
            lines: Vec::new(),
            selected_bytes: 0,
            max_results,
            max_inline_bytes,
            truncated: false,
            inline_truncated: false,
            snippet_truncated: false,
        }
    }

    fn push(&mut self, line: &str) -> bool {
        if self.lines.len() >= self.max_results {
            self.truncated = true;
            return false;
        }

        let (line, snippet_truncated) = cap_search_line(line);
        self.snippet_truncated |= snippet_truncated;
        let additional = line.len() + usize::from(!self.lines.is_empty());
        self.selected_bytes = self.selected_bytes.saturating_add(additional);
        self.lines.push(line);

        if self.selected_bytes > self.max_inline_bytes {
            self.truncated = true;
            self.inline_truncated = true;
            return false;
        }

        true
    }

    fn finish(self, backend: &'static str) -> SearchOutput {
        SearchOutput {
            selected: self.lines.join("\n"),
            matches: self.lines.len(),
            truncated: self.truncated,
            inline_truncated: self.inline_truncated,
            snippet_truncated: self.snippet_truncated,
            backend,
        }
    }
}

async fn rg_search(
    query: &str,
    search_path: &kuncode_workspace::WorkspacePath,
    workspace_root: &Path,
    max_results: usize,
    max_inline_bytes: usize,
    timeout_ms: u64,
    cancel_token: CancellationToken,
) -> Result<Option<SearchOutput>, ToolError> {
    let path_arg =
        if search_path.relative_path().as_os_str().is_empty() { ".".to_owned() } else { relative_string(search_path) };
    let mut command = Command::new("rg");
    command
        .current_dir(workspace_root)
        .arg("--line-number")
        .arg("--color")
        .arg("never")
        .arg("--no-heading")
        .arg("--glob")
        .arg("!.git/**")
        .arg("--glob")
        .arg("!target/**")
        .arg("--glob")
        .arg("!node_modules/**")
        .arg("--glob")
        .arg("!.venv/**")
        .arg("--max-columns")
        .arg(MAX_SNIPPET_BYTES.to_string())
        .arg("--max-columns-preview")
        .arg("--")
        .arg(query)
        .arg(path_arg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(ToolError::Process { message: format!("failed to run rg: {source}") }),
    };
    let stdout = child.stdout.take().ok_or_else(|| ToolError::Internal {
        tool: "search".to_owned(),
        message: "rg stdout was not captured".to_owned(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| ToolError::Internal {
        tool: "search".to_owned(),
        message: "rg stderr was not captured".to_owned(),
    })?;
    let stderr_task = tokio::spawn(read_limited_stderr(stderr));
    let mut reader = BufReader::new(stdout);
    let mut collector = SearchCollector::new(max_results, max_inline_bytes);
    let timeout = tokio::time::sleep(Duration::from_millis(timeout_ms));
    tokio::pin!(timeout);

    let stopped_early = loop {
        let mut line = String::new();
        let read = tokio::select! {
            read = reader.read_line(&mut line) => read.map_err(|source| ToolError::Process {
                message: format!("failed to read rg output: {source}"),
            })?,
            () = &mut timeout => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = stderr_task.await;
                return Err(ToolError::Timeout { tool: "search".to_owned(), elapsed_ms: timeout_ms });
            }
            () = cancel_token.cancelled() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = stderr_task.await;
                return Err(ToolError::Cancelled { tool: "search".to_owned() });
            }
        };

        if read == 0 {
            break false;
        }
        if !line.trim().is_empty() && !collector.push(line.trim_end_matches(['\r', '\n'])) {
            let _ = child.start_kill();
            break true;
        }
    };

    let status = child
        .wait()
        .await
        .map_err(|source| ToolError::Process { message: format!("failed to wait for rg: {source}") })?;
    let stderr = stderr_task
        .await
        .map_err(|err| ToolError::Process { message: format!("failed to join rg stderr reader: {err}") })?
        .map_err(|source| ToolError::Process { message: format!("failed to read rg stderr: {source}") })?;

    if stopped_early || matches!(status.code(), Some(0 | 1)) {
        Ok(Some(collector.finish("rg")))
    } else {
        Err(ToolError::Process { message: format!("rg failed: {}", String::from_utf8_lossy(&stderr).trim()) })
    }
}

async fn read_limited_stderr<R>(mut reader: R) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut out = Vec::new();
    let mut buf = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        if out.len() < 8192 {
            let remaining = 8192 - out.len();
            out.extend_from_slice(&buf[..read.min(remaining)]);
        }
    }
    Ok(out)
}

fn rust_search(
    regex: &Regex,
    search_path: &kuncode_workspace::WorkspacePath,
    workspace_root: &Path,
    max_results: usize,
    ctx: &ToolContext<'_>,
) -> Result<SearchOutput, ToolError> {
    let mut collector = SearchCollector::new(max_results, ctx.limits.max_inline_output_bytes);
    let walker = WalkBuilder::new(search_path.as_path()).build();

    for entry in walker {
        if ctx.cancel_token.is_cancelled() {
            return Err(ToolError::Cancelled { tool: "search".to_owned() });
        }

        let entry = entry.map_err(|err| ToolError::Process { message: format!("search walk failed: {err}") })?;
        let path = entry.path();
        let relative = path.strip_prefix(workspace_root).unwrap_or(path);
        if ctx.workspace.is_default_ignored(relative) {
            continue;
        }
        if !entry.file_type().is_some_and(|file_type| file_type.is_file()) {
            continue;
        }

        let Ok(file) = std::fs::File::open(path) else {
            continue;
        };
        let reader = std::io::BufReader::new(file);
        for (idx, line) in reader.lines().enumerate() {
            let Ok(line) = line else {
                continue;
            };
            if regex.is_match(&line) {
                let display = format!("{}:{}:{}", path_string(relative), idx + 1, line.trim());
                if !collector.push(&display) {
                    break;
                }
            }
        }
        if collector.truncated {
            break;
        }
    }

    Ok(collector.finish("rust"))
}

fn cap_search_line(line: &str) -> (String, bool) {
    if line.len() <= MAX_SNIPPET_BYTES {
        return (line.to_owned(), false);
    }
    if MAX_SNIPPET_BYTES <= 3 {
        return (String::new(), true);
    }
    let (mut capped, _) = truncate_utf8(line, MAX_SNIPPET_BYTES - 3);
    capped.push_str("...");
    (capped, true)
}

fn descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: "search".to_owned(),
        description: "Search workspace text with ripgrep or the built-in Rust fallback.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "minLength": 1 },
                "path": { "type": "string", "minLength": 1 },
                "max_results": { "type": "integer", "minimum": 1, "maximum": MAX_RESULTS_LIMIT }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        output_schema: None,
        effects: vec![ToolEffect::ReadWorkspace],
        default_capabilities: vec![ToolCapability::Explore, ToolCapability::Edit],
        risk_flags: vec![],
    }
}
