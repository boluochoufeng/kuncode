//! The `read_file` tool: read a UTF-8 workspace file with line pagination.

use std::{io, path::PathBuf};

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use kuncode_core::non_empty_vec::NonEmptyVec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{
    fs::OpenOptions,
    io::{AsyncBufRead, AsyncBufReadExt, BufReader},
};

use super::helpers::{
    io_error, non_empty_path, open_error, open_no_follow, revalidate_path, workspace_error,
};
use crate::{
    permission::{
        CanonicalPath, CanonicalToolInput, PermissionCheckSpec, PermissionTarget, ToolDisplay,
    },
    tool::{
        FileStamp, PreparationContext, PreparedInvocationState, ToolContext, ToolError, ToolOutput,
        TypedPreparation, TypedTool, definition_for,
    },
    workspace::Workspace,
};

const READ_LIMIT_BYTES: usize = 50_000;
const MAX_LINE_BYTES: usize = 2_000;

/// Arguments accepted by the [`ReadFile`] tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// Workspace-relative or absolute path to an existing UTF-8 file.
    path: String,
    /// One-based line number to start reading from. Defaults to `1` (the first
    /// line). Feed back the `next_line` from a previous result to paginate.
    #[serde(default)]
    start_line: Option<usize>,
    /// Maximum number of lines to return.
    #[serde(default)]
    limit: Option<usize>,
}

/// Text content read from a workspace file.
#[derive(Debug, Serialize)]
pub struct ReadFileOutput {
    /// Path shown relative to the workspace when possible.
    pub path: String,
    /// File content, sliced by line range and bounded by byte/line caps.
    pub content: String,
    /// One-based line number of the first returned line; `0` when nothing was
    /// returned (e.g. `start_line` is past the end of the file).
    pub start_line: usize,
    /// Number of lines returned in [`Self::content`].
    pub returned_lines: usize,
    /// `true` when more *lines* follow the returned range. This is the vertical
    /// pagination axis only: it never refers to a partial line, and re-reading
    /// at [`Self::next_line`] resumes at the next whole line (see
    /// [`Self::truncated_lines`] for tails dropped *within* a line). Exact total
    /// line count is intentionally not reported, since it would require reading
    /// the whole file even for a small slice.
    pub has_more: bool,
    /// One-based line number to pass back as `start_line` to continue reading
    /// where this call left off. Present only when [`Self::has_more`] is `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_line: Option<usize>,
    /// One-based *file* line numbers (the same numbering as [`Self::start_line`])
    /// whose tail was dropped to fit `MAX_LINE_BYTES`. These lines are
    /// INCOMPLETE: the elided tail is not in `content` and — unlike
    /// [`Self::has_more`] — is *not* reachable via [`Self::next_line`], which
    /// only advances by whole lines. Recover it another way (e.g. `grep`).
    /// Omitted when every returned line is intact.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub truncated_lines: Vec<usize>,
}

/// Canonical read target paired with validated pagination arguments.
#[derive(Debug)]
pub struct PreparedReadFile {
    args: ReadFileArgs,
    path: PathBuf,
}

/// Reads UTF-8 files from the workspace.
#[derive(Clone, Debug)]
pub struct ReadFile {
    definition: ToolDefinition,
    workspace: Workspace,
}

impl ReadFile {
    /// Creates a file reader bound to a workspace.
    pub fn new(workspace: Workspace) -> Self {
        Self {
            definition: definition_for::<ReadFileArgs>(
                "read_file",
                "Read a UTF-8 workspace file as numbered lines. A file too long \
                 for one reply is paginated rather than silently cut, so a \
                 result reports how to read on. Use grep to find which file to \
                 read, and prefer this over cat, head, or tail through bash.",
            ),
            workspace,
        }
    }
}

#[async_trait]
impl TypedTool for ReadFile {
    type Args = ReadFileArgs;
    type Prepared = PreparedReadFile;
    type Output = ReadFileOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare_typed(
        &self,
        mut args: ReadFileArgs,
        _canonical_input: CanonicalToolInput,
        _ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
        let path = non_empty_path(&args.path)?;
        let resolved = self
            .workspace
            .resolve_target(path)
            .await
            .map_err(workspace_error)?;

        let start_line = args.start_line.unwrap_or(1);
        if start_line == 0 {
            return Err(ToolOutput::failure(
                "invalid_arguments",
                "`start_line` is 1-based and must be greater than zero",
            ));
        }
        if matches!(args.limit, Some(0)) {
            return Err(ToolOutput::failure(
                "invalid_arguments",
                "`limit` must be greater than zero",
            ));
        }

        let canonical_path = CanonicalPath::from_absolute(&resolved)
            .map_err(|error| ToolOutput::failure("invalid_arguments", error.to_string()))?;
        let display_path = self.workspace.relative_display(&resolved);
        args.path = canonical_path.as_str().to_string();
        args.start_line = Some(start_line);
        let canonical_input = CanonicalToolInput::new(serde_json::json!({
            "path": canonical_path.as_str(),
            "start_line": start_line,
            "limit": args.limit,
        }));
        Ok(TypedPreparation::new(
            PreparedReadFile {
                args,
                path: resolved,
            },
            canonical_input,
            NonEmptyVec::new(PermissionCheckSpec::new(PermissionTarget::Read(
                canonical_path,
            ))),
            ToolDisplay::new(format!("Read file: {display_path}")),
        ))
    }

    async fn run_prepared(
        &self,
        prepared: PreparedReadFile,
        ctx: &ToolContext,
    ) -> ToolOutput<ReadFileOutput> {
        let PreparedReadFile {
            args,
            path: resolved,
        } = prepared;
        let start_line = args.start_line.unwrap_or(1);
        let file = match open_no_follow(&resolved, OpenOptions::new().read(true)).await {
            Ok(file) => file,
            Err(err) => return open_error("read", &resolved, err, &self.workspace),
        };
        // Taken before the contents, so a write landing mid-read leaves the file
        // no longer matching what was recorded and is caught as a change later.
        let stamp = file
            .metadata()
            .await
            .as_ref()
            .map(FileStamp::from_metadata)
            .unwrap_or_default();
        let mut lines = BufReader::new(file);

        // Skip the lines before `start_line` without keeping them. Cost is
        // proportional to `start_line`, not file size; nothing past the
        // requested window is read.
        for _ in 0..(start_line - 1) {
            match read_bounded_line(&mut lines, 0).await {
                Ok(Some(_)) => {}
                // `start_line` is past EOF: there is simply nothing to return.
                Ok(None) => break,
                Err(err) => return io_error("read", &resolved, err, &self.workspace),
            }
        }

        let mut collected = Vec::new();
        let mut used_bytes = 0usize;
        // The *horizontal* truncation axis: one-based file line numbers (same
        // numbering as `start_line`) whose tail we dropped to fit
        // `MAX_LINE_BYTES`. Lossy and — unlike `has_more` / `next_line` — NOT
        // recoverable by paginating.
        let mut truncated_lines: Vec<usize> = Vec::new();
        let mut has_more = false;

        loop {
            // Stop once the line budget is met, peeking one line ahead so the
            // caller learns whether more lines remain. This is the *vertical*
            // axis: lossless, the next read at `next_line` resumes here.
            if args.limit.is_some_and(|limit| collected.len() >= limit) {
                // A read error while peeking is a real failure (e.g. invalid
                // UTF-8 on the next line), not EOF — surface it like every other
                // read instead of reporting a false end-of-file via `has_more`.
                has_more = match read_bounded_line(&mut lines, 0).await {
                    Ok(Some(_)) => true,
                    Ok(None) => false,
                    Err(err) => return io_error("read", &resolved, err, &self.workspace),
                };
                break;
            }

            let raw = match read_bounded_line(&mut lines, MAX_LINE_BYTES).await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(err) => return io_error("read", &resolved, err, &self.workspace),
            };

            let raw_bytes = raw.total_bytes;
            let mut line = raw.text;
            let line_truncated = raw_bytes > line.len();

            // Honor the total byte budget, but always return at least one line
            // so a single over-long line still yields its (capped) prefix.
            // Spilling a whole line to the next page is lossless, so it counts as
            // vertical pagination (`has_more`), never as truncation.
            if !collected.is_empty() && used_bytes + line.len() > READ_LIMIT_BYTES {
                has_more = true;
                break;
            }

            // A line cut by `MAX_LINE_BYTES` gets a visible, located marker so the
            // model can see *which* line lost its tail and that re-reading will
            // not bring it back. The marker is metadata, not file content.
            //
            // Lossy-and-unpaginable is deliberate, and industry-wide: mainstream
            // agent CLIs all drop overlong tails the same way and point the model
            // at grep. The one lossless precedent (DeepAgents' continuation
            // sub-lines `41.1`, `41.2`) recovers tails — near-always minified
            // output — at the price of line numbers that don't exist in the
            // file. Not worth it. The cap is bytes, not chars, on purpose: it
            // bounds token cost uniformly across scripts and stays on the same
            // axis as `READ_LIMIT_BYTES`.
            if line_truncated {
                truncated_lines.push(start_line + collected.len());
                line.push_str(&line_truncated_marker(raw_bytes - line.len()));
            }

            used_bytes += line.len();
            collected.push(line);
        }

        let returned_lines = collected.len();
        let next_line = has_more.then_some(start_line + returned_lines);
        let truncated = !truncated_lines.is_empty();

        // Any read licenses a later whole-file write, including a single page
        // of a long file — see [`ReadLedger`](crate::tool::ReadLedger) for why
        // the bar is deliberately this low.
        ctx.reads.record(&resolved, stamp);

        let output = ToolOutput::success(ReadFileOutput {
            path: self.workspace.relative_display(&resolved),
            content: collected.join("\n"),
            start_line: if returned_lines == 0 { 0 } else { start_line },
            returned_lines,
            has_more,
            next_line,
            truncated_lines,
        });

        if truncated {
            output.truncated()
        } else {
            output
        }
    }

    async fn revalidate_prepared(
        &self,
        prepared: &mut PreparedReadFile,
        _ctx: &ToolContext,
    ) -> Result<PreparedInvocationState, ToolError> {
        revalidate_path(&self.workspace, &prepared.path).await
    }
}

#[derive(Debug)]
struct BoundedLine {
    text: String,
    total_bytes: usize,
}

// Reads and validates one UTF-8 line while retaining only its bounded prefix.
// The discarded tail is still drained and validated so it cannot hide invalid
// UTF-8 or leave the reader in the middle of a line.
async fn read_bounded_line<R>(
    reader: &mut R,
    retain_limit: usize,
) -> io::Result<Option<BoundedLine>>
where
    R: AsyncBufRead + Unpin,
{
    let retain_limit = retain_limit.min(MAX_LINE_BYTES);
    let mut retained = Vec::with_capacity(retain_limit);
    let mut validator = Utf8Validator::default();
    let mut total_bytes = 0usize;
    let mut last_byte = None;
    let mut terminated = false;

    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            break;
        }

        let newline = available.iter().position(|byte| *byte == b'\n');
        let content_end = newline.unwrap_or(available.len());
        let content = &available[..content_end];

        validator.push(content)?;
        total_bytes = total_bytes.checked_add(content.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "line length exceeds usize")
        })?;
        if let Some(byte) = content.last() {
            last_byte = Some(*byte);
        }

        let remaining = retain_limit.saturating_sub(retained.len());
        retained.extend_from_slice(&content[..content.len().min(remaining)]);

        let consumed = content_end + usize::from(newline.is_some());
        terminated = newline.is_some();
        reader.consume(consumed);
        if terminated {
            break;
        }
    }

    if total_bytes == 0 && !terminated {
        return Ok(None);
    }
    validator.finish()?;

    // Match `AsyncBufReadExt::lines`: strip a carriage return only when it is
    // immediately before a newline, after validating it as part of the input.
    if terminated && last_byte == Some(b'\r') {
        let unstripped_bytes = total_bytes;
        total_bytes -= 1;
        if retained.len() == unstripped_bytes {
            retained.pop();
        }
    }

    // A bounded prefix may end midway through an otherwise valid code point.
    // Back up to the last complete boundary, matching `truncate_utf8` semantics.
    let valid_prefix_len = match std::str::from_utf8(&retained) {
        Ok(_) => retained.len(),
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => return Err(invalid_utf8_error()),
    };
    retained.truncate(valid_prefix_len);
    let text = String::from_utf8(retained).map_err(|_| invalid_utf8_error())?;

    Ok(Some(BoundedLine { text, total_bytes }))
}

#[derive(Debug, Default)]
struct Utf8Validator {
    pending: Vec<u8>,
}

impl Utf8Validator {
    fn push(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        if !self.pending.is_empty() {
            let sequence_len = utf8_sequence_len(self.pending[0]).ok_or_else(invalid_utf8_error)?;
            if self.pending.len() >= sequence_len {
                return Err(invalid_utf8_error());
            }
            let take = (sequence_len - self.pending.len()).min(bytes.len());
            self.pending.extend_from_slice(&bytes[..take]);
            if self.pending.len() < sequence_len {
                return Ok(());
            }
            std::str::from_utf8(&self.pending).map_err(|_| invalid_utf8_error())?;
            self.pending.clear();
            bytes = &bytes[take..];
        }

        if let Err(error) = std::str::from_utf8(bytes) {
            if error.error_len().is_some() {
                return Err(invalid_utf8_error());
            }
            self.pending
                .extend_from_slice(&bytes[error.valid_up_to()..]);
        }
        Ok(())
    }

    fn finish(self) -> io::Result<()> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            Err(invalid_utf8_error())
        }
    }
}

fn utf8_sequence_len(first_byte: u8) -> Option<usize> {
    match first_byte {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
}

fn invalid_utf8_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "stream did not contain valid UTF-8",
    )
}

/// Inline marker appended to a line whose tail was dropped to fit
/// `MAX_LINE_BYTES`. Deliberately explicit: the elided tail is neither in the
/// returned content nor reachable via `next_line` (which advances by whole
/// lines), so the model is told to recover it another way rather than re-read.
fn line_truncated_marker(elided_bytes: usize) -> String {
    format!(
        "…⟨kuncode: line truncated, {elided_bytes} more bytes — re-reading won't return them; use grep⟩"
    )
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use tokio::io::BufReader;

    use super::{MAX_LINE_BYTES, ReadFile, read_bounded_line};
    use crate::test_support::TestDir;
    use crate::tool::{ToolContext, ToolOutput, execute_for_test};

    async fn call(tool: ReadFile, args: serde_json::Value) -> ToolOutput {
        execute_for_test(Arc::new(tool), args, &ToolContext::new())
            .await
            .expect("no harness-level error")
    }

    #[tokio::test]
    async fn read_file_returns_a_line_window_with_pagination() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("notes.txt"), "one\ntwo\nthree\n")
            .expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        let output = call(
            tool,
            serde_json::json!({
                "path": "notes.txt",
                "start_line": 2,
                "limit": 1
            }),
        )
        .await;

        assert!(output.ok);
        // The returned line is complete, so the content itself is not truncated.
        assert!(!output.truncated);
        let data = output.data.expect("data present");
        assert_eq!(data["path"], "notes.txt");
        assert_eq!(data["content"], "two");
        assert_eq!(data["start_line"], 2);
        assert_eq!(data["returned_lines"], 1);
        // A line still follows the window, so the model can paginate: the next
        // read resumes at line 3 (`three`).
        assert_eq!(data["has_more"], true);
        assert_eq!(data["next_line"], 3);
    }

    #[tokio::test]
    async fn read_file_reads_a_whole_small_file_without_more() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("notes.txt"), "a\nb").expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "path": "notes.txt" })).await;

        assert!(output.ok);
        assert!(!output.truncated);
        let data = output.data.expect("data present");
        assert_eq!(data["content"], "a\nb");
        assert_eq!(data["start_line"], 1);
        assert_eq!(data["returned_lines"], 2);
        assert_eq!(data["has_more"], false);
        // `next_line` is omitted once the whole file has been read.
        assert!(data["next_line"].is_null());
    }

    #[tokio::test]
    async fn read_file_preserves_carriage_return_at_unterminated_eof() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("notes.txt"), "tail\r").expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "path": "notes.txt" })).await;

        assert!(output.ok);
        assert_eq!(output.data.expect("data present")["content"], "tail\r");
    }

    #[tokio::test]
    async fn read_file_start_past_end_returns_empty() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("notes.txt"), "a\nb").expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        let output = call(
            tool,
            serde_json::json!({ "path": "notes.txt", "start_line": 6 }),
        )
        .await;

        assert!(output.ok);
        let data = output.data.expect("data present");
        assert_eq!(data["content"], "");
        assert_eq!(data["start_line"], 0);
        assert_eq!(data["returned_lines"], 0);
        assert_eq!(data["has_more"], false);
    }

    #[tokio::test]
    async fn read_file_truncates_an_overlong_line() {
        let tmp = TestDir::new();
        let long_line = "x".repeat(4 * 1024 * 1024);
        fs::write(tmp.path().join("min.js"), &long_line).expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "path": "min.js" })).await;

        assert!(output.ok);
        // The single line is capped: content is truncated horizontally, but
        // there is no further line, so `has_more` stays false.
        assert!(output.truncated);
        let data = output.data.expect("data present");
        let content = data["content"].as_str().expect("content is a string");
        // The capped prefix is preserved and a visible marker is appended.
        assert!(content.starts_with(&"x".repeat(MAX_LINE_BYTES)));
        assert!(content.contains("line truncated"));
        assert_eq!(data["returned_lines"], 1);
        assert_eq!(data["has_more"], false);
        // The cut is reported on the horizontal axis, located to line 1.
        assert_eq!(data["truncated_lines"], serde_json::json!([1]));
    }

    #[tokio::test]
    async fn bounded_line_reader_retains_only_the_requested_prefix() {
        let input = vec![b'x'; 4 * 1024 * 1024];
        // A deliberately small transport buffer forces the line and its UTF-8
        // validation state across many reads.
        let mut reader = BufReader::with_capacity(257, input.as_slice());

        let line = read_bounded_line(&mut reader, MAX_LINE_BYTES)
            .await
            .expect("line should be read")
            .expect("line should be present");

        assert_eq!(line.total_bytes, input.len());
        assert_eq!(line.text.len(), MAX_LINE_BYTES);
    }

    #[tokio::test]
    async fn bounded_line_reader_validates_code_points_across_buffers() {
        let input = "你".repeat(MAX_LINE_BYTES);
        // Five-byte buffers split successive three-byte code points at
        // different offsets instead of accidentally preserving alignment.
        let mut reader = BufReader::with_capacity(5, input.as_bytes());

        let line = read_bounded_line(&mut reader, MAX_LINE_BYTES)
            .await
            .expect("line should be read")
            .expect("line should be present");

        assert_eq!(line.total_bytes, input.len());
        assert!(line.text.len() <= MAX_LINE_BYTES);
        assert!(line.text.chars().all(|character| character == '你'));
    }

    #[tokio::test]
    async fn read_file_skips_an_overlong_line_without_retaining_it() {
        let tmp = TestDir::new();
        let long_line = "x".repeat(4 * 1024 * 1024);
        fs::write(
            tmp.path().join("generated.txt"),
            format!("{long_line}\r\nselected\r\ntrailing"),
        )
        .expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        let output = call(
            tool,
            serde_json::json!({
                "path": "generated.txt",
                "start_line": 2,
                "limit": 1
            }),
        )
        .await;

        assert!(output.ok);
        let data = output.data.expect("data present");
        assert_eq!(data["content"], "selected");
        assert_eq!(data["start_line"], 2);
        assert_eq!(data["returned_lines"], 1);
        assert_eq!(data["has_more"], true);
        assert_eq!(data["next_line"], 3);
    }

    #[tokio::test]
    async fn read_file_rejects_invalid_utf8_in_a_discarded_line_tail() {
        let tmp = TestDir::new();
        let mut body = vec![b'x'; 4 * 1024 * 1024];
        body.extend_from_slice(&[0xff, b'\n']);
        body.extend_from_slice(b"selected\n");
        fs::write(tmp.path().join("mixed.bin"), body).expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        let output = call(
            tool,
            serde_json::json!({ "path": "mixed.bin", "start_line": 2 }),
        )
        .await;

        assert!(!output.ok);
        assert_eq!(output.error.expect("error present").kind.as_str(), "read");
    }

    #[tokio::test]
    async fn read_file_truncates_a_multibyte_line_on_a_char_boundary() {
        let tmp = TestDir::new();
        // Each `你` is 3 bytes and `MAX_LINE_BYTES` is not a multiple of 3, so
        // the byte cap necessarily lands *inside* a code point — exercising the
        // `is_char_boundary` back-off that ASCII-only tests never reach.
        let long_line = "你".repeat(MAX_LINE_BYTES);
        fs::write(tmp.path().join("cjk.txt"), &long_line).expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "path": "cjk.txt" })).await;

        assert!(output.ok);
        assert!(output.truncated);
        let data = output.data.expect("data present");
        // Round-tripping through JSON as a string already proves valid UTF-8.
        let content = data["content"].as_str().expect("content is valid UTF-8");
        // The capped prefix sits before the inline marker.
        let body = content.split('…').next().expect("body precedes the marker");
        // No code point was split: every char survived whole and the cut landed
        // on a char boundary at or below the byte cap.
        assert!(body.chars().all(|c| c == '你'));
        assert!(body.len() <= MAX_LINE_BYTES);
        assert_eq!(body.len() % '你'.len_utf8(), 0);
        assert_eq!(data["truncated_lines"], serde_json::json!([1]));
    }

    #[tokio::test]
    async fn read_file_stops_at_the_byte_budget_and_paginates() {
        let tmp = TestDir::new();
        // 60 lines of 1000 bytes each (60 KB) overflow the 50 KB byte budget.
        let body = (0..60)
            .map(|_| "y".repeat(1_000))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(tmp.path().join("big.txt"), body).expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "path": "big.txt" })).await;

        assert!(output.ok);
        // Spilling whole lines to the next page is lossless vertical pagination,
        // not truncation.
        assert!(!output.truncated);
        let data = output.data.expect("data present");
        // 50 lines * 1000 bytes fills the budget; the rest spills to a next read.
        assert_eq!(data["returned_lines"], 50);
        assert_eq!(data["has_more"], true);
        // 50 lines read starting at line 1, so the next read resumes at line 51.
        assert_eq!(data["next_line"], 51);
        // No line lost its tail, so the horizontal axis is empty/omitted.
        assert!(data["truncated_lines"].is_null());
    }

    #[tokio::test]
    async fn read_file_keeps_line_truncation_off_the_pagination_axis() {
        let tmp = TestDir::new();
        let long_line = "x".repeat(MAX_LINE_BYTES + 500);
        // A truncated line sits in the *middle* of the window, with a line after
        // it — the case where a single `truncated` boolean would mislead the
        // model into thinking `next_line` recovers the lost tail.
        fs::write(
            tmp.path().join("mixed.txt"),
            format!("head\n{long_line}\ntail\n"),
        )
        .expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "path": "mixed.txt", "limit": 2 })).await;

        assert!(output.ok);
        assert!(output.truncated);
        let data = output.data.expect("data present");
        assert_eq!(data["returned_lines"], 2);
        // Horizontal: line 2's tail is gone and flagged as such.
        assert_eq!(data["truncated_lines"], serde_json::json!([2]));
        assert!(
            data["content"]
                .as_str()
                .expect("content is a string")
                .contains("line truncated")
        );
        // Vertical: `next_line` points at the *next line* 3 (`tail`), never at
        // the truncated line's missing tail.
        assert_eq!(data["has_more"], true);
        assert_eq!(data["next_line"], 3);
    }

    #[tokio::test]
    async fn read_file_surfaces_a_read_error_while_peeking_for_more() {
        let tmp = TestDir::new();
        // The first line is valid UTF-8; the next line is not. Reading with
        // `limit: 1` collects line 1, then peeks line 2 to set `has_more` — the
        // peek must surface the decode error, not report a clean end-of-file.
        fs::write(
            tmp.path().join("mixed.bin"),
            [b'o', b'k', b'\n', 0xff, 0xfe],
        )
        .expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "path": "mixed.bin", "limit": 1 })).await;

        assert!(!output.ok);
        assert_eq!(output.error.expect("error present").kind.as_str(), "read");
    }

    #[tokio::test]
    async fn read_file_rejects_zero_start_line() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("notes.txt"), "a\nb").expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        // `start_line` is 1-based; `0` is a contract violation, not "line 0".
        let output = call(
            tool,
            serde_json::json!({ "path": "notes.txt", "start_line": 0 }),
        )
        .await;

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "invalid_arguments"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let tmp = TestDir::new();
        let outside = tmp
            .path()
            .parent()
            .expect("temp root has parent")
            .join(format!("kuncode-outside-{}", std::process::id()));
        fs::write(&outside, "outside").expect("outside file should be written");
        symlink(&outside, tmp.path().join("link")).expect("symlink should be created");
        let tool = ReadFile::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "path": "link" })).await;

        let _ = fs::remove_file(outside);
        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "workspace_path"
        );
    }
}
