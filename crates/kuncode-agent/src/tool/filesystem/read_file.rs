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
    /// Byte offset within `start_line` at which to resume reading that single
    /// line. Feed back the `resume_offset` from a `truncated_lines` entry
    /// (with `start_line` set to that entry's `line`). The call then returns
    /// up to 50 000 bytes of that one line and nothing else, reporting a
    /// further `resume_offset` while a tail remains. Offsets count bytes of
    /// line content (no terminator) and must fall on a UTF-8 char boundary.
    #[serde(default)]
    line_offset: Option<usize>,
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
    /// Byte offset within [`Self::start_line`] at which [`Self::content`]
    /// begins. Present only on a `line_offset` continuation read, whose
    /// content is a fragment of that single line rather than whole lines.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_offset: Option<usize>,
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
    /// Returned lines whose tail was elided to fit the per-call byte caps.
    /// These lines are INCOMPLETE in [`Self::content`], and — unlike
    /// [`Self::has_more`] — the elided tail is *not* reachable via
    /// [`Self::next_line`], which only advances by whole lines. Each entry
    /// instead carries the [`resume_offset`](TruncatedLine::resume_offset) to
    /// feed back as `line_offset` to keep reading that single line
    /// losslessly. Omitted when every returned line is intact.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub truncated_lines: Vec<TruncatedLine>,
}

/// A returned line whose tail was elided, and where to resume reading it.
#[derive(Debug, Serialize)]
pub struct TruncatedLine {
    /// One-based *file* line number, in the same numbering as
    /// [`ReadFileOutput::start_line`].
    pub line: usize,
    /// Byte offset into the line where the elided tail begins. Pass it back as
    /// `line_offset` with `start_line` set to [`Self::line`] to read on; it
    /// always falls on a UTF-8 character boundary, so repeated continuations
    /// reassemble the line without splitting a code point.
    pub resume_offset: usize,
    /// Bytes of the line not yet returned, excluding any line terminator.
    pub remaining_bytes: usize,
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
                 result reports how to read on — including an over-long single \
                 line, whose clipped tail is fetched by passing back \
                 `line_offset`. Use grep to find which file to read, and prefer \
                 this over cat, head, or tail through bash.",
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
            "line_offset": args.line_offset,
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
            match read_bounded_line(&mut lines, 0, 0).await {
                Ok(Some(_)) => {}
                // `start_line` is past EOF: there is simply nothing to return.
                Ok(None) => break,
                Err(err) => return io_error("read", &resolved, err, &self.workspace),
            }
        }

        let mut collected = Vec::new();
        let mut used_bytes = 0usize;
        // The *horizontal* truncation axis: returned lines whose tail we
        // dropped to fit the byte caps. Unlike `has_more` / `next_line` it is
        // not recovered by paginating — each entry instead names the
        // `line_offset` that reads the same line on.
        let mut truncated_lines: Vec<TruncatedLine> = Vec::new();
        let mut has_more = false;

        if let Some(offset) = args.line_offset {
            // Continuation of a single over-long line: return the next bounded
            // window of `start_line` beginning at `offset`, instead of whole
            // lines. The line is still drained and validated end to end, so
            // memory stays capped and a following line can be peeked at.
            let raw = match read_bounded_line(&mut lines, offset, READ_LIMIT_BYTES).await {
                Ok(Some(line)) => line,
                Ok(None) => {
                    return ToolOutput::failure(
                        "invalid_arguments",
                        format!(
                            "`start_line` {start_line} is past the end of the file, \
                             so `line_offset` has no line to continue"
                        ),
                    );
                }
                // `InvalidInput` is reserved for a misaligned `line_offset`;
                // everything else is a real read failure.
                Err(err) if err.kind() == io::ErrorKind::InvalidInput => {
                    return ToolOutput::failure("invalid_arguments", err.to_string());
                }
                Err(err) => return io_error("read", &resolved, err, &self.workspace),
            };
            if offset > raw.total_bytes {
                return ToolOutput::failure(
                    "invalid_arguments",
                    format!(
                        "`line_offset` {offset} is past the end of line {start_line} \
                         ({} bytes)",
                        raw.total_bytes
                    ),
                );
            }

            let mut fragment = raw.text;
            let resume_offset = offset + fragment.len();
            let remaining_bytes = raw.total_bytes - resume_offset;
            if remaining_bytes > 0 {
                truncated_lines.push(TruncatedLine {
                    line: start_line,
                    resume_offset,
                    remaining_bytes,
                });
                fragment.push_str(&line_truncated_marker(
                    start_line,
                    resume_offset,
                    remaining_bytes,
                ));
            }
            collected.push(fragment);

            // Both axes are reported independently: an unfinished line above
            // does not hide that a next whole line exists.
            has_more = match read_bounded_line(&mut lines, 0, 0).await {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(err) => return io_error("read", &resolved, err, &self.workspace),
            };
        } else {
            loop {
                // Stop once the line budget is met, peeking one line ahead so
                // the caller learns whether more lines remain. This is the
                // *vertical* axis: lossless, the next read at `next_line`
                // resumes here.
                if args.limit.is_some_and(|limit| collected.len() >= limit) {
                    // A read error while peeking is a real failure (e.g.
                    // invalid UTF-8 on the next line), not EOF — surface it
                    // like every other read instead of reporting a false
                    // end-of-file via `has_more`.
                    has_more = match read_bounded_line(&mut lines, 0, 0).await {
                        Ok(Some(_)) => true,
                        Ok(None) => false,
                        Err(err) => return io_error("read", &resolved, err, &self.workspace),
                    };
                    break;
                }

                let raw = match read_bounded_line(&mut lines, 0, MAX_LINE_BYTES).await {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(err) => return io_error("read", &resolved, err, &self.workspace),
                };

                let raw_bytes = raw.total_bytes;
                let mut line = raw.text;
                let line_truncated = raw_bytes > line.len();

                // Honor the total byte budget, but always return at least one
                // line so a single over-long line still yields its (capped)
                // prefix. Spilling a whole line to the next page is lossless,
                // so it counts as vertical pagination (`has_more`), never as
                // truncation.
                if !collected.is_empty() && used_bytes + line.len() > READ_LIMIT_BYTES {
                    has_more = true;
                    break;
                }

                // A line cut by `MAX_LINE_BYTES` gets a visible, located marker
                // plus a structured entry naming where a continuation read
                // resumes it. The marker is metadata, not file content. The cap
                // is bytes, not chars, on purpose: it bounds token cost
                // uniformly across scripts and stays on the same axis as
                // `READ_LIMIT_BYTES`.
                if line_truncated {
                    let line_number = start_line + collected.len();
                    let resume_offset = line.len();
                    let remaining_bytes = raw_bytes - resume_offset;
                    truncated_lines.push(TruncatedLine {
                        line: line_number,
                        resume_offset,
                        remaining_bytes,
                    });
                    line.push_str(&line_truncated_marker(
                        line_number,
                        resume_offset,
                        remaining_bytes,
                    ));
                }

                used_bytes += line.len();
                collected.push(line);
            }
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
            line_offset: args.line_offset,
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

// Reads and validates one UTF-8 line while retaining only the window of it
// that starts `skip` bytes in and holds at most `retain_limit` bytes. Bytes
// outside the window are still drained and validated, so they cannot hide
// invalid UTF-8 or leave the reader in the middle of a line.
async fn read_bounded_line<R>(
    reader: &mut R,
    skip: usize,
    retain_limit: usize,
) -> io::Result<Option<BoundedLine>>
where
    R: AsyncBufRead + Unpin,
{
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
        let chunk_start = total_bytes;
        total_bytes = total_bytes.checked_add(content.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "line length exceeds usize")
        })?;
        if let Some(byte) = content.last() {
            last_byte = Some(*byte);
        }

        // Intersect this chunk with the retained window, which spans line
        // bytes `[skip, skip + retain_limit)`.
        let begin = skip.saturating_sub(chunk_start).min(content.len());
        let end = skip
            .saturating_add(retain_limit)
            .saturating_sub(chunk_start)
            .min(content.len());
        retained.extend_from_slice(&content[begin..end]);

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
    // The window covers line bytes `[skip, skip + retained.len())`, so it holds
    // the `\r` exactly when it reaches the unstripped end.
    if terminated && last_byte == Some(b'\r') {
        let unstripped_bytes = total_bytes;
        total_bytes -= 1;
        if skip + retained.len() == unstripped_bytes {
            retained.pop();
        }
    }

    // With the whole stream validated above, a window that starts on a
    // continuation byte means `skip` landed inside a code point — the caller's
    // offset is wrong, not the file. `InvalidInput` keeps that distinguishable
    // from the `InvalidData` raised for a genuinely invalid file.
    if let Some(first) = retained.first()
        && (*first & 0b1100_0000) == 0b1000_0000
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "`line_offset` does not fall on a UTF-8 character boundary",
        ));
    }

    // A bounded window may end midway through an otherwise valid code point.
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

/// Inline marker appended to a line whose tail was elided by a byte cap.
/// Deliberately explicit: the elided tail is neither in the returned content
/// nor reachable via `next_line` (which advances by whole lines), so the
/// marker spells out the exact continuation call that does return it.
fn line_truncated_marker(line: usize, resume_offset: usize, elided_bytes: usize) -> String {
    format!(
        "…⟨kuncode: line truncated, {elided_bytes} more bytes — pass start_line={line}, \
         line_offset={resume_offset} to read on⟩"
    )
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use tokio::io::BufReader;

    use super::{MAX_LINE_BYTES, READ_LIMIT_BYTES, ReadFile, read_bounded_line};
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
        // The cut is reported on the horizontal axis, located to line 1 and
        // carrying the offset a continuation read resumes at.
        assert_eq!(
            data["truncated_lines"],
            serde_json::json!([{
                "line": 1,
                "resume_offset": MAX_LINE_BYTES,
                "remaining_bytes": long_line.len() - MAX_LINE_BYTES,
            }])
        );
    }

    #[tokio::test]
    async fn bounded_line_reader_retains_only_the_requested_prefix() {
        let input = vec![b'x'; 4 * 1024 * 1024];
        // A deliberately small transport buffer forces the line and its UTF-8
        // validation state across many reads.
        let mut reader = BufReader::with_capacity(257, input.as_slice());

        let line = read_bounded_line(&mut reader, 0, MAX_LINE_BYTES)
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

        let line = read_bounded_line(&mut reader, 0, MAX_LINE_BYTES)
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
        // The resume offset matches the boundary-backed-off prefix, so a
        // continuation starts exactly where the returned body ends.
        assert_eq!(data["truncated_lines"][0]["line"], 1);
        assert_eq!(data["truncated_lines"][0]["resume_offset"], body.len());
        assert_eq!(
            data["truncated_lines"][0]["remaining_bytes"],
            long_line.len() - body.len()
        );
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
        // Horizontal: line 2's tail is gone, flagged with where to resume it.
        assert_eq!(
            data["truncated_lines"],
            serde_json::json!([{
                "line": 2,
                "resume_offset": MAX_LINE_BYTES,
                "remaining_bytes": 500,
            }])
        );
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
    async fn line_continuation_returns_a_bounded_fragment_and_both_axes() {
        let tmp = TestDir::new();
        let long_line = "x".repeat(200_000);
        fs::write(tmp.path().join("min.js"), format!("{long_line}\ntail\n"))
            .expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        let output = call(
            tool,
            serde_json::json!({
                "path": "min.js",
                "start_line": 1,
                "line_offset": MAX_LINE_BYTES
            }),
        )
        .await;

        assert!(output.ok);
        assert!(output.truncated);
        let data = output.data.expect("data present");
        let content = data["content"].as_str().expect("content is a string");
        // The fragment resumes where the clipped first read left off and is
        // bounded by the per-call byte budget, not the 2 KB line cap.
        let body = content.split('…').next().expect("body precedes the marker");
        assert_eq!(body, "x".repeat(READ_LIMIT_BYTES));
        assert!(content.contains("line truncated"));
        assert_eq!(data["start_line"], 1);
        assert_eq!(data["line_offset"], MAX_LINE_BYTES);
        assert_eq!(data["returned_lines"], 1);
        // Horizontal axis: the line's remaining tail, with a stable resume spot.
        assert_eq!(
            data["truncated_lines"],
            serde_json::json!([{
                "line": 1,
                "resume_offset": MAX_LINE_BYTES + READ_LIMIT_BYTES,
                "remaining_bytes": long_line.len() - MAX_LINE_BYTES - READ_LIMIT_BYTES,
            }])
        );
        // Vertical axis stays independently visible: a next whole line exists
        // even though the current line is unfinished.
        assert_eq!(data["has_more"], true);
        assert_eq!(data["next_line"], 2);
    }

    #[tokio::test]
    async fn line_continuations_reassemble_a_long_multibyte_line() {
        let tmp = TestDir::new();
        // 120 000 bytes of 3-byte code points: several continuation reads, each
        // forced to back its window edges off to char boundaries.
        let long_line = "你".repeat(40_000);
        fs::write(tmp.path().join("cjk.jsonl"), format!("{long_line}\nnext\n"))
            .expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        let first = call(
            tool.clone(),
            serde_json::json!({ "path": "cjk.jsonl", "limit": 1 }),
        )
        .await;
        assert!(first.ok);
        let data = first.data.expect("data present");
        let content = data["content"].as_str().expect("content is a string");
        let mut assembled = content
            .split('…')
            .next()
            .expect("body precedes the marker")
            .to_string();
        let mut entry = data["truncated_lines"][0].clone();

        loop {
            assert_eq!(entry["line"], 1);
            let offset = entry["resume_offset"].as_u64().expect("resume offset") as usize;
            // Each resume offset continues exactly where the previous fragment
            // ended: concatenating fragments loses and duplicates nothing.
            assert_eq!(offset, assembled.len());

            let output = call(
                tool.clone(),
                serde_json::json!({
                    "path": "cjk.jsonl",
                    "start_line": 1,
                    "line_offset": offset
                }),
            )
            .await;
            assert!(output.ok);
            let data = output.data.expect("data present");
            let content = data["content"].as_str().expect("content is a string");
            let body = content.split('…').next().expect("body precedes the marker");
            // No fragment splits a code point.
            assert!(body.chars().all(|character| character == '你'));
            assembled.push_str(body);

            if data["truncated_lines"].is_null() {
                // The final fragment completes the line; the vertical axis then
                // reports the next whole line as usual.
                assert!(!output.truncated);
                assert_eq!(data["has_more"], true);
                assert_eq!(data["next_line"], 2);
                break;
            }
            assert!(output.truncated);
            entry = data["truncated_lines"][0].clone();
        }

        assert_eq!(assembled, long_line);
    }

    #[tokio::test]
    async fn line_continuation_strips_crlf_and_reports_the_following_line() {
        let tmp = TestDir::new();
        let long_line = "x".repeat(MAX_LINE_BYTES + 100);
        fs::write(
            tmp.path().join("log.txt"),
            format!("{long_line}\r\nnext\r\n"),
        )
        .expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        let output = call(
            tool,
            serde_json::json!({
                "path": "log.txt",
                "start_line": 1,
                "line_offset": MAX_LINE_BYTES
            }),
        )
        .await;

        assert!(output.ok);
        assert!(!output.truncated);
        let data = output.data.expect("data present");
        // The fragment ends with the line's content: the `\r\n` terminator is
        // stripped exactly as it is for whole-line reads.
        assert_eq!(data["content"], "x".repeat(100));
        assert_eq!(data["returned_lines"], 1);
        assert!(data["truncated_lines"].is_null());
        assert_eq!(data["has_more"], true);
        assert_eq!(data["next_line"], 2);
    }

    #[tokio::test]
    async fn line_continuation_rejects_an_offset_inside_a_code_point() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("cjk.txt"), "你好\n").expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        // Offset 1 lands inside the 3-byte `你`.
        let output = call(
            tool,
            serde_json::json!({ "path": "cjk.txt", "start_line": 1, "line_offset": 1 }),
        )
        .await;

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "invalid_arguments"
        );
    }

    #[tokio::test]
    async fn line_continuation_rejects_an_offset_past_the_line_end() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("notes.txt"), "short\nlonger line\n")
            .expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        let output = call(
            tool,
            serde_json::json!({ "path": "notes.txt", "start_line": 1, "line_offset": 100 }),
        )
        .await;

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "invalid_arguments"
        );
    }

    #[tokio::test]
    async fn line_continuation_rejects_a_start_line_past_eof() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("notes.txt"), "a\nb").expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        let output = call(
            tool,
            serde_json::json!({ "path": "notes.txt", "start_line": 6, "line_offset": 0 }),
        )
        .await;

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "invalid_arguments"
        );
    }

    #[tokio::test]
    async fn bounded_line_reader_retains_a_mid_line_window() {
        let input = "你".repeat(2_000);
        // Tiny transport buffers force the window's edges to land mid-chunk.
        let mut reader = BufReader::with_capacity(5, input.as_bytes());

        // 300 is a char boundary (300 % 3 == 0); so is the window end.
        let line = read_bounded_line(&mut reader, 300, 30)
            .await
            .expect("line should be read")
            .expect("line should be present");

        assert_eq!(line.total_bytes, input.len());
        assert_eq!(line.text.len(), 30);
        assert!(line.text.chars().all(|character| character == '你'));
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
