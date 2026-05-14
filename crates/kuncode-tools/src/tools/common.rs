//! Shared helpers for the built-in tool implementations.

use std::{
    io::Write as _,
    path::Path,
    process::{ExitStatus, Stdio},
    time::{Duration, Instant},
};

use kuncode_core::ArtifactId;
use kuncode_workspace::{ExecutionLane, WorkspaceError, WorkspacePath};
use serde_json::Value;
use tempfile::NamedTempFile;
use tokio::{
    fs::OpenOptions,
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
};
use tokio_util::sync::CancellationToken;

use crate::{SUMMARY_MAX_CHARS, ToolContext, ToolError, ToolResult, ToolResultError};

/// Borrow a required string field from a validated JSON payload.
pub(crate) fn required_str<'a>(payload: &'a Value, tool: &str, field: &str) -> Result<&'a str, ToolError> {
    payload.get(field).and_then(Value::as_str).ok_or_else(|| ToolError::InvalidInput {
        tool: tool.to_owned(),
        message: format!("missing string field `{field}`"),
    })
}

/// Borrow an optional string field from a validated JSON payload.
pub(crate) fn optional_str<'a>(payload: &'a Value, field: &str) -> Option<&'a str> {
    payload.get(field).and_then(Value::as_str)
}

/// Read an optional unsigned integer field from a validated JSON payload.
pub(crate) fn optional_u64(payload: &Value, field: &str) -> Option<u64> {
    payload.get(field).and_then(Value::as_u64)
}

/// Convert a workspace error into the tool-level workspace classification.
pub(crate) fn workspace_error(err: WorkspaceError) -> ToolError {
    let message = err.to_string();
    let path = match err {
        WorkspaceError::PathEscape { path, root: _ }
        | WorkspaceError::SymlinkEscape { path, target: _, root: _ }
        | WorkspaceError::TooLarge { path, size: _, max: _ }
        | WorkspaceError::Binary { path }
        | WorkspaceError::NotFound { path }
        | WorkspaceError::IoError { path, source: _ } => path,
    };
    ToolError::Workspace { path, message }
}

/// Build a `ToolResult` while mapping invariant failures to `ToolError`.
pub(crate) fn tool_result(
    tool: &str,
    summary: String,
    inline_content: Option<String>,
    content_ref: Option<ArtifactId>,
    metadata: Value,
) -> Result<ToolResult, ToolError> {
    ToolResult::try_new(summary, inline_content, content_ref, metadata).map_err(|err| match err {
        ToolResultError::SummaryTooLong { len } => ToolError::ResultTooLarge {
            tool: tool.to_owned(),
            message: format!("summary exceeds {SUMMARY_MAX_CHARS} characters (got {len})"),
        },
        ToolResultError::MetadataNotObject => ToolError::Internal {
            tool: tool.to_owned(),
            message: "tool metadata must be a JSON object or null".to_owned(),
        },
    })
}

/// Truncate a UTF-8 string to at most `max_bytes` without splitting a codepoint.
pub(crate) fn truncate_utf8(s: &str, max_bytes: usize) -> (String, bool) {
    if s.len() <= max_bytes {
        return (s.to_owned(), false);
    }
    if max_bytes == 0 {
        return (String::new(), true);
    }

    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_owned(), true)
}

/// Truncate a summary candidate to the runtime-enforced summary limit.
pub(crate) fn cap_summary(s: String) -> String {
    if s.chars().count() <= SUMMARY_MAX_CHARS {
        return s;
    }

    let mut out: String = s.chars().take(SUMMARY_MAX_CHARS.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

/// Return a stable workspace-relative path string.
pub(crate) fn relative_string(path: &WorkspacePath) -> String {
    path.relative_path().display().to_string()
}

/// Save large tool output as an artifact linked to the current `tool.started`.
pub(crate) async fn save_artifact(ctx: &ToolContext<'_>, kind: &str, content: &[u8]) -> Result<ArtifactId, ToolError> {
    let store = ctx
        .artifact_store
        .ok_or_else(|| ToolError::Artifact { message: format!("artifact store is required for `{kind}` output") })?;
    let record = store
        .save(kind.to_owned(), ctx.source_event_id, content)
        .await
        .map_err(|err| ToolError::Artifact { message: err.to_string() })?;
    Ok(record.artifact_id)
}

/// Save a file artifact linked to the current `tool.started`.
pub(crate) async fn save_file_artifact(
    ctx: &ToolContext<'_>,
    kind: &str,
    path: &Path,
) -> Result<ArtifactId, ToolError> {
    let store = ctx
        .artifact_store
        .ok_or_else(|| ToolError::Artifact { message: format!("artifact store is required for `{kind}` output") })?;
    let record = store
        .save_file(kind.to_owned(), ctx.source_event_id, path)
        .await
        .map_err(|err| ToolError::Artifact { message: err.to_string() })?;
    Ok(record.artifact_id)
}

/// Ensure a resolved path also stays within the active execution lane.
pub(crate) fn ensure_in_lane(path: &WorkspacePath, lane: &ExecutionLane, tool: &str) -> Result<(), ToolError> {
    if path.as_path().starts_with(lane.root_path()) {
        Ok(())
    } else {
        Err(ToolError::Workspace {
            path: path.as_path().to_path_buf(),
            message: format!("path is outside the active execution lane for `{tool}`"),
        })
    }
}

/// Convert an arbitrary path to a display string for diagnostics.
pub(crate) fn path_string(path: &Path) -> String {
    path.display().to_string()
}

/// Captured output and timing data from a child process.
pub(crate) struct CapturedOutput {
    /// Process exit status reported by the OS.
    pub(crate) status: ExitStatus,
    /// Bounded stdout stream data.
    pub(crate) stdout: CapturedStream,
    /// Bounded stderr stream data.
    pub(crate) stderr: CapturedStream,
    /// Wall-clock duration from spawn to process exit.
    pub(crate) duration_ms: u128,
}

/// Per-stream memory limits for process capture.
#[derive(Clone, Copy)]
pub(crate) struct CaptureLimits {
    pub(crate) stdout: usize,
    pub(crate) stderr: usize,
}

/// One captured process stream. `inline` is bounded by the configured stream
/// limit; `spill` contains the complete stream when the limit was exceeded.
pub(crate) struct CapturedStream {
    pub(crate) inline: Vec<u8>,
    pub(crate) bytes: u64,
    pub(crate) truncated: bool,
    spill: Option<NamedTempFile>,
}

impl CapturedStream {
    pub(crate) fn full_path(&self) -> Option<&Path> {
        self.spill.as_ref().map(NamedTempFile::path)
    }
}

/// Spawn a command without a shell and capture stdout/stderr.
///
/// The process is killed when the timeout expires or the cancellation token is
/// triggered. Spawn/read/join failures are classified as process errors.
pub(crate) async fn run_capture(
    tool: &str,
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout_ms: u64,
    cancel_token: CancellationToken,
    limits: CaptureLimits,
) -> Result<CapturedOutput, ToolError> {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    configure_process_group(&mut command);

    let mut child = command
        .spawn()
        .map_err(|source| ToolError::Process { message: format!("failed to spawn `{program}`: {source}") })?;
    let stdout = child.stdout.take().ok_or_else(|| ToolError::Internal {
        tool: tool.to_owned(),
        message: "child stdout was not captured".to_owned(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| ToolError::Internal {
        tool: tool.to_owned(),
        message: "child stderr was not captured".to_owned(),
    })?;
    let stdout_task = tokio::spawn(read_bounded(stdout, limits.stdout));
    let stderr_task = tokio::spawn(read_bounded(stderr, limits.stderr));
    let started = Instant::now();

    // The read tasks run concurrently with `wait()` so a verbose child cannot
    // block forever on a full stdout/stderr pipe while the parent is waiting.
    let status = tokio::select! {
        status = child.wait() => status.map_err(|source| ToolError::Process {
            message: format!("failed to wait for `{program}`: {source}"),
        })?,
        () = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
            kill_child_tree(&mut child).await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(ToolError::Timeout { tool: tool.to_owned(), elapsed_ms: timeout_ms });
        }
        () = cancel_token.cancelled() => {
            kill_child_tree(&mut child).await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(ToolError::Cancelled { tool: tool.to_owned() });
        }
    };

    let stdout = join_reader(tool, "stdout", stdout_task).await?;
    let stderr = join_reader(tool, "stderr", stderr_task).await?;
    Ok(CapturedOutput { status, stdout, stderr, duration_ms: started.elapsed().as_millis() })
}

async fn read_bounded<R>(mut reader: R, limit: usize) -> std::io::Result<CapturedStream>
where
    R: AsyncRead + Unpin,
{
    // Capture keeps only a prefix in memory. Once the prefix budget is full we
    // create a temp spill file and copy the already-captured prefix into it, so
    // later artifact creation can still persist the complete stream.
    let mut inline = Vec::with_capacity(limit.min(8192));
    let mut bytes = 0_u64;
    let mut spill = None::<NamedTempFile>;
    let mut spill_file = None::<tokio::fs::File>;
    let mut buffer = [0_u8; 8192];

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        bytes = bytes.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));

        if let Some(file) = spill_file.as_mut() {
            file.write_all(chunk).await?;
            continue;
        }

        let remaining = limit.saturating_sub(inline.len());
        if read <= remaining {
            inline.extend_from_slice(chunk);
            continue;
        }

        // First overflow: `inline` becomes the stable model-facing prefix;
        // `spill` becomes the full stream, starting with that same prefix.
        inline.extend_from_slice(&chunk[..remaining]);
        let named = named_tempfile()?;
        let mut file = OpenOptions::new().write(true).truncate(true).open(named.path()).await?;
        file.write_all(&inline).await?;
        file.write_all(&chunk[remaining..]).await?;
        spill = Some(named);
        spill_file = Some(file);
    }

    if let Some(file) = spill_file.as_mut() {
        file.flush().await?;
    }
    drop(spill_file);
    Ok(CapturedStream { inline, bytes, truncated: spill.is_some(), spill })
}

async fn join_reader(
    tool: &str,
    stream_name: &str,
    task: tokio::task::JoinHandle<std::io::Result<CapturedStream>>,
) -> Result<CapturedStream, ToolError> {
    task.await
        .map_err(|err| ToolError::Process { message: format!("failed to join {stream_name} reader: {err}") })?
        .map_err(|source| ToolError::Process {
            message: format!("failed to read {stream_name} for `{tool}`: {source}"),
        })
}

pub(crate) async fn save_captured_stream_artifact(
    ctx: &ToolContext<'_>,
    kind: &str,
    stream: &CapturedStream,
) -> Result<ArtifactId, ToolError> {
    if let Some(path) = stream.full_path() {
        save_file_artifact(ctx, kind, path).await
    } else {
        save_artifact(ctx, kind, &stream.inline).await
    }
}

pub(crate) async fn count_non_empty_lines(stream: &CapturedStream, tool: &str) -> Result<usize, ToolError> {
    if let Some(path) = stream.full_path() {
        // Do not derive counts from the truncated inline prefix. Tools like
        // git_status need metadata based on the complete captured stream.
        let file = tokio::fs::File::open(path).await.map_err(|err| ToolError::Process {
            message: format!("failed to open captured output for `{tool}`: {err}"),
        })?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        let mut count = 0_usize;
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line).await.map_err(|err| ToolError::Process {
                message: format!("failed to read captured output for `{tool}`: {err}"),
            })?;
            if read == 0 {
                break;
            }
            count += usize::from(non_empty_line_bytes(&line));
        }
        Ok(count)
    } else {
        Ok(stream.inline.split(|byte| *byte == b'\n').filter(|line| non_empty_line_bytes(line)).count())
    }
}

pub(crate) async fn save_combined_output_artifact(
    ctx: &ToolContext<'_>,
    kind: &str,
    stdout: &CapturedStream,
    stderr: &CapturedStream,
) -> Result<ArtifactId, ToolError> {
    // exec_argv exposes one content_ref for the whole process transcript. The
    // streams may individually live in memory or spill files; this helper
    // normalizes both cases into the stable stdout/stderr artifact format.
    let combined = named_tempfile().map_err(|err| ToolError::Artifact { message: err.to_string() })?;
    {
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(combined.path())
            .await
            .map_err(|err| ToolError::Artifact { message: err.to_string() })?;
        file.write_all(b"stdout:\n").await.map_err(|err| ToolError::Artifact { message: err.to_string() })?;
        write_stream_to_file(stdout, &mut file).await?;
        file.write_all(b"\nstderr:\n").await.map_err(|err| ToolError::Artifact { message: err.to_string() })?;
        write_stream_to_file(stderr, &mut file).await?;
        file.flush().await.map_err(|err| ToolError::Artifact { message: err.to_string() })?;
    }
    save_file_artifact(ctx, kind, combined.path()).await
}

async fn write_stream_to_file(stream: &CapturedStream, file: &mut tokio::fs::File) -> Result<(), ToolError> {
    if let Some(path) = stream.full_path() {
        let mut source =
            tokio::fs::File::open(path).await.map_err(|err| ToolError::Artifact { message: err.to_string() })?;
        tokio::io::copy(&mut source, file).await.map_err(|err| ToolError::Artifact { message: err.to_string() })?;
    } else {
        file.write_all(&stream.inline).await.map_err(|err| ToolError::Artifact { message: err.to_string() })?;
    }
    Ok(())
}

fn non_empty_line_bytes(line: &[u8]) -> bool {
    line.iter().any(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
}

fn named_tempfile() -> std::io::Result<NamedTempFile> {
    let mut file = NamedTempFile::new()?;
    file.flush()?;
    Ok(file)
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    // Create a fresh process group before exec. Timeout/cancel then target the
    // group instead of only the direct child, which covers shell-spawned
    // grandchildren on Unix.
    unsafe {
        command.pre_exec(|| {
            // SAFETY: `setpgid(0, 0)` runs in the child process immediately
            // before exec and only places that child into a new process group.
            if setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

async fn kill_child_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id().and_then(|id| i32::try_from(id).ok()) {
            // SAFETY: negative pid targets the process group created for this
            // child by `configure_process_group`; SIGKILL is a fixed signal.
            unsafe {
                let _ = kill(-pid, SIGKILL);
            }
        } else {
            let _ = child.start_kill();
        }
    }

    #[cfg(not(unix))]
    {
        // Windows process-tree management needs Job Objects; Phase 2 keeps the
        // documented direct-child fallback there.
        let _ = child.start_kill();
    }

    let _ = child.wait().await;
}

#[cfg(unix)]
const SIGKILL: i32 = 9;

#[cfg(unix)]
unsafe extern "C" {
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
}
