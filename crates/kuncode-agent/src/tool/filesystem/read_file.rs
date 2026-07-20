//! The `read_file` tool: read a UTF-8 workspace file with line pagination.

use std::path::PathBuf;

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use kuncode_core::non_empty_vec::NonEmptyVec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};

use super::helpers::{io_error, non_empty_path, workspace_error};
use crate::{
    permission::{
        CanonicalPath, CanonicalToolInput, PathSelector, PermissionCheckSpec, PermissionTarget,
        ToolDisplay,
    },
    tool::{
        PreparationContext, PreparedInvocationState, ToolContext, ToolError, ToolOutput,
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
            definition: definition_for::<ReadFileArgs>("read_file", "Read a UTF-8 workspace file"),
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
                PathSelector::exact(canonical_path),
            ))),
            ToolDisplay::new(format!("Read file: {display_path}")),
        ))
    }

    async fn run_prepared(
        &self,
        prepared: PreparedReadFile,
        _ctx: &ToolContext,
    ) -> ToolOutput<ReadFileOutput> {
        let PreparedReadFile {
            args,
            path: resolved,
        } = prepared;
        let start_line = args.start_line.unwrap_or(1);
        let file = match tokio::fs::File::open(&resolved).await {
            Ok(file) => file,
            Err(err) => return io_error("read", &resolved, err, &self.workspace),
        };
        let mut lines = BufReader::new(file).lines();

        // Skip the lines before `start_line` without keeping them. Cost is
        // proportional to `start_line`, not file size; nothing past the
        // requested window is read.
        for _ in 0..(start_line - 1) {
            match lines.next_line().await {
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
                has_more = match lines.next_line().await {
                    Ok(Some(_)) => true,
                    Ok(None) => false,
                    Err(err) => return io_error("read", &resolved, err, &self.workspace),
                };
                break;
            }

            let raw = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(err) => return io_error("read", &resolved, err, &self.workspace),
            };

            let raw_bytes = raw.len();
            let (mut line, line_truncated) = truncate_utf8(&raw, MAX_LINE_BYTES);

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
            // TODO(read_file): horizontal truncation is lossy and unpaginable.
            // Revisit whether a better scheme — e.g. Roo-style continuation
            // sub-lines (`41.1`, `41.2`) that fold the tail back onto the
            // vertical pagination axis — is worth the line-numbering complexity.
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
        Ok(
            if self
                .workspace
                .revalidate_target(&prepared.path)
                .await
                .is_ok()
            {
                PreparedInvocationState::Current
            } else {
                // Re-preparation produces the model-safe path diagnostic against
                // current metadata without executing the stale payload.
                PreparedInvocationState::Stale
            },
        )
    }
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

/// Truncates `input` to at most `max_bytes` bytes, backing off to the nearest
/// UTF-8 code-point boundary so the result is always valid UTF-8.
///
/// This guards the *code-point* boundary (one Rust `char`), not the *grapheme
/// cluster* (what a user sees as one character). A grapheme can span several
/// code points — `e` + combining accent, a ZWJ emoji sequence, a flag — and the
/// cut may land between them, leaving a lone combining mark or a half emoji. The
/// bytes stay valid UTF-8; only the rendered glyph may look odd. This is
/// deliberate: grapheme segmentation needs an extra crate (`unicode-segmentation`)
/// and only matters in the degenerate over-long-line case (minified JS, base64),
/// so it is not worth the dependency.
fn truncate_utf8(input: &str, max_bytes: usize) -> (String, bool) {
    if input.len() <= max_bytes {
        return (input.to_string(), false);
    }

    let mut end = max_bytes;
    while !input.is_char_boundary(end) {
        end -= 1;
    }

    (input[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use super::{MAX_LINE_BYTES, ReadFile};
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
        let long_line = "x".repeat(MAX_LINE_BYTES + 1_000);
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
