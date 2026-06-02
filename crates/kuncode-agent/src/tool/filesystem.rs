//! Workspace-scoped filesystem tools.

use std::{
    ffi::OsStr,
    io,
    path::{Component, Path},
};

use async_trait::async_trait;
use ignore::WalkBuilder;
use kuncode_core::completion::ToolDefinition;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::{
    tool::{ToolOutput, TypedTool, definition_for},
    workspace::{Workspace, WorkspaceError},
};

const READ_LIMIT_BYTES: usize = 50_000;
const MAX_LINE_BYTES: usize = 2_000;
const DEFAULT_GLOB_LIMIT: usize = 200;
const MAX_GLOB_LIMIT: usize = 1_000;

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

/// Reads UTF-8 files from the workspace.
/// TODO: Add line numbers to the content read.
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
    type Output = ReadFileOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn run(&self, args: ReadFileArgs) -> ToolOutput<ReadFileOutput> {
        let path = match non_empty_path(&args.path) {
            Ok(path) => path,
            Err(output) => return output,
        };

        let resolved = match self.workspace.resolve_existing(path).await {
            Ok(path) => path,
            Err(err) => return workspace_error(err),
        };

        if !is_file(&resolved).await {
            return ToolOutput::failure(
                "invalid_path",
                format!(
                    "`{}` is not a file",
                    self.workspace.relative_display(&resolved)
                ),
            );
        }

        let start_line = args.start_line.unwrap_or(1);
        if start_line == 0 {
            return ToolOutput::failure(
                "invalid_arguments",
                "`start_line` is 1-based and must be greater than zero",
            );
        }
        if matches!(args.limit, Some(0)) {
            return ToolOutput::failure("invalid_arguments", "`limit` must be greater than zero");
        }

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
}

/// Arguments accepted by the [`WriteFile`] tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteFileArgs {
    /// Workspace-relative or absolute file path to write.
    path: String,
    /// UTF-8 content to write to the file.
    content: String,
}

/// Result of writing a workspace file.
#[derive(Debug, Serialize)]
pub struct WriteFileOutput {
    /// Path shown relative to the workspace when possible.
    pub path: String,
    /// Number of UTF-8 bytes written.
    pub bytes: usize,
}

/// Writes UTF-8 files inside the workspace.
#[derive(Clone, Debug)]
pub struct WriteFile {
    definition: ToolDefinition,
    workspace: Workspace,
}

impl WriteFile {
    /// Creates a file writer bound to a workspace.
    pub fn new(workspace: Workspace) -> Self {
        Self {
            definition: definition_for::<WriteFileArgs>(
                "write_file",
                "Write a UTF-8 workspace file",
            ),
            workspace,
        }
    }
}

#[async_trait]
impl TypedTool for WriteFile {
    type Args = WriteFileArgs;
    type Output = WriteFileOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn run(&self, args: WriteFileArgs) -> ToolOutput<WriteFileOutput> {
        let path = match non_empty_path(&args.path) {
            Ok(path) => path,
            Err(output) => return output,
        };

        let resolved = match self.workspace.resolve_for_write(path).await {
            Ok(path) => path,
            Err(err) => return workspace_error(err),
        };

        if is_dir(&resolved).await {
            return ToolOutput::failure(
                "invalid_path",
                format!(
                    "`{}` is a directory",
                    self.workspace.relative_display(&resolved)
                ),
            );
        }

        if let Err(err) = tokio::fs::write(&resolved, args.content.as_bytes()).await {
            return io_error("write", &resolved, err, &self.workspace);
        }

        ToolOutput::success(WriteFileOutput {
            path: self.workspace.relative_display(&resolved),
            bytes: args.content.len(),
        })
    }
}

/// Arguments accepted by the [`EditFile`] tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditFileArgs {
    /// Workspace-relative or absolute path to an existing UTF-8 file.
    path: String,
    /// Existing text to replace once.
    old_text: String,
    /// Replacement text.
    new_text: String,
}

/// Result of editing a workspace file.
#[derive(Debug, Serialize)]
pub struct EditFileOutput {
    /// Path shown relative to the workspace when possible.
    pub path: String,
    /// Number of replacements applied.
    pub replacements: usize,
    /// Number of UTF-8 bytes written after the edit.
    pub bytes: usize,
}

/// Replaces text in UTF-8 files inside the workspace.
#[derive(Clone, Debug)]
pub struct EditFile {
    definition: ToolDefinition,
    workspace: Workspace,
}

impl EditFile {
    /// Creates a file editor bound to a workspace.
    pub fn new(workspace: Workspace) -> Self {
        Self {
            definition: definition_for::<EditFileArgs>(
                "edit_file",
                "Replace text once in a UTF-8 workspace file",
            ),
            workspace,
        }
    }
}

#[async_trait]
impl TypedTool for EditFile {
    type Args = EditFileArgs;
    type Output = EditFileOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn run(&self, args: EditFileArgs) -> ToolOutput<EditFileOutput> {
        let path = match non_empty_path(&args.path) {
            Ok(path) => path,
            Err(output) => return output,
        };

        if args.old_text.is_empty() {
            return ToolOutput::failure("invalid_arguments", "`old_text` must not be empty");
        }

        let resolved = match self.workspace.resolve_existing(path).await {
            Ok(path) => path,
            Err(err) => return workspace_error(err),
        };

        if !is_file(&resolved).await {
            return ToolOutput::failure(
                "invalid_path",
                format!(
                    "`{}` is not a file",
                    self.workspace.relative_display(&resolved)
                ),
            );
        }

        let content = match tokio::fs::read_to_string(&resolved).await {
            Ok(content) => content,
            Err(err) => return io_error("read", &resolved, err, &self.workspace),
        };

        // `old_text` must be unambiguous: zero matches gives the model nothing
        // to anchor on, and more than one would let us silently edit the wrong
        // occurrence. Force a unique match so the edit is deterministic.
        match content.matches(&args.old_text).count() {
            0 => {
                return ToolOutput::failure(
                    "text_not_found",
                    format!(
                        "`old_text` was not found in `{}`",
                        self.workspace.relative_display(&resolved)
                    ),
                );
            }
            1 => {}
            count => {
                return ToolOutput::failure(
                    "ambiguous_match",
                    format!(
                        "`old_text` matches {count} times in `{}`; \
                         include surrounding context so it is unique",
                        self.workspace.relative_display(&resolved)
                    ),
                );
            }
        }

        let edited = content.replacen(&args.old_text, &args.new_text, 1);
        if let Err(err) = tokio::fs::write(&resolved, edited.as_bytes()).await {
            return io_error("write", &resolved, err, &self.workspace);
        }

        ToolOutput::success(EditFileOutput {
            path: self.workspace.relative_display(&resolved),
            replacements: 1,
            bytes: edited.len(),
        })
    }
}

/// Arguments accepted by the [`Glob`] tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GlobArgs {
    /// Workspace-relative glob pattern. Supports `*`, `?`, and `**`.
    pattern: String,
    /// Maximum number of matches to return.
    #[serde(default)]
    limit: Option<usize>,
    /// Also search files hidden or excluded by `.gitignore`. The VCS store
    /// (`.git`) is always skipped. Defaults to `false`.
    #[serde(default)]
    include_ignored: bool,
}

/// Filesystem entries matched by a glob pattern.
#[derive(Debug, Serialize)]
pub struct GlobOutput {
    /// Pattern used for matching.
    pub pattern: String,
    /// Workspace-relative matching paths.
    pub matches: Vec<String>,
    /// Total matches found before output limiting.
    pub total_matches: usize,
}

/// Finds workspace paths using a small glob matcher.
#[derive(Clone, Debug)]
pub struct Glob {
    definition: ToolDefinition,
    workspace: Workspace,
}

impl Glob {
    /// Creates a glob search tool bound to a workspace.
    pub fn new(workspace: Workspace) -> Self {
        Self {
            definition: definition_for::<GlobArgs>(
                "glob",
                "Find workspace paths matching a glob pattern",
            ),
            workspace,
        }
    }
}

#[async_trait]
impl TypedTool for Glob {
    type Args = GlobArgs;
    type Output = GlobOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn run(&self, args: GlobArgs) -> ToolOutput<GlobOutput> {
        let pattern = args.pattern.trim();
        if pattern.is_empty() {
            return ToolOutput::failure("invalid_arguments", "`pattern` must not be empty");
        }

        if let Err(message) = validate_glob_pattern(pattern) {
            return ToolOutput::failure("invalid_arguments", message);
        }

        let limit = args.limit.unwrap_or(DEFAULT_GLOB_LIMIT).min(MAX_GLOB_LIMIT);
        if limit == 0 {
            return ToolOutput::failure("invalid_arguments", "`limit` must be greater than zero");
        }

        // The `ignore` walker is synchronous and thread-based, so the whole
        // tree walk runs on the blocking pool to keep the async runtime free.
        let workspace = self.workspace.clone();
        let include_ignored = args.include_ignored;
        let entries =
            match tokio::task::spawn_blocking(move || walk_workspace(&workspace, include_ignored))
                .await
            {
                Ok(entries) => entries,
                Err(err) => {
                    return ToolOutput::failure(
                        "internal",
                        format!("workspace walk did not complete: {err}"),
                    );
                }
            };

        let normalized_pattern = normalize_pattern(pattern);
        let mut matches = entries
            .into_iter()
            .filter(|entry| glob_match(&normalized_pattern, entry))
            .collect::<Vec<_>>();
        matches.sort();

        let total_matches = matches.len();
        let truncated = total_matches > limit;
        matches.truncate(limit);

        let output = ToolOutput::success(GlobOutput {
            pattern: pattern.to_string(),
            matches,
            total_matches,
        });

        if truncated {
            output.truncated()
        } else {
            output
        }
    }
}

fn non_empty_path<D>(path: &str) -> Result<&Path, ToolOutput<D>> {
    if path.trim().is_empty() {
        Err(ToolOutput::failure(
            "invalid_arguments",
            "`path` must not be empty",
        ))
    } else {
        Ok(Path::new(path))
    }
}

fn workspace_error<D>(err: WorkspaceError) -> ToolOutput<D> {
    ToolOutput::failure("workspace_path", err.to_string())
}

/// Non-blocking `stat` predicate. A missing path or stat error resolves to
/// `false`, matching the semantics of [`Path::is_file`].
async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .map(|meta| meta.is_file())
        .unwrap_or(false)
}

/// Non-blocking `stat` predicate. A missing path or stat error resolves to
/// `false`, matching the semantics of [`Path::is_dir`].
async fn is_dir(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .map(|meta| meta.is_dir())
        .unwrap_or(false)
}

fn io_error<D>(kind: &str, path: &Path, err: io::Error, workspace: &Workspace) -> ToolOutput<D> {
    ToolOutput::failure(
        kind,
        format!(
            "failed to {kind} `{}`: {err}",
            workspace.relative_display(path)
        ),
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

fn validate_glob_pattern(pattern: &str) -> Result<(), String> {
    let path = Path::new(pattern);
    if path.is_absolute() {
        return Err("`pattern` must be relative to the workspace".to_string());
    }

    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err("`pattern` must not escape the workspace".to_string());
    }

    Ok(())
}

/// Walks the workspace with ripgrep's `ignore` crate, returning every entry as
/// a workspace-relative, slash-separated path.
///
/// Which paths are "noise" is delegated to the project itself rather than a
/// hardcoded name list: by default `.gitignore` / `.ignore` / `.git/info/exclude`
/// inside the workspace are honored and hidden dotfiles are skipped. The VCS
/// store (`.git`) is never traversed. Ignore files *above* the workspace and
/// the user's global gitignore are deliberately not consulted, so behavior is
/// reproducible and scoped to the workspace.
///
/// Synchronous and thread-based; callers run it on the blocking pool.
fn walk_workspace(workspace: &Workspace, include_ignored: bool) -> Vec<String> {
    let root = workspace.root();
    let enabled = !include_ignored;

    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(enabled)
        .git_ignore(enabled)
        .git_exclude(enabled)
        .ignore(enabled)
        .git_global(false)
        .parents(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != OsStr::new(".git"));

    let mut entries = Vec::new();
    for result in builder.build() {
        // Skip unreadable entries (permissions, races) rather than aborting the
        // whole search, matching ripgrep's resilience.
        let Ok(entry) = result else { continue };
        let path = entry.path();
        if path == root {
            continue;
        }

        // Symlinks are listed but not followed. Only advertise a link whose
        // target stays inside the workspace, so glob's visible set matches what
        // `read_file`/`write_file` will actually act on; escaping and dangling
        // links are dropped. The `canonicalize` cost lands only on links, which
        // are rare, and we are already on the blocking pool.
        if entry
            .file_type()
            .is_some_and(|file_type| file_type.is_symlink())
            && !std::fs::canonicalize(path).is_ok_and(|target| target.starts_with(root))
        {
            continue;
        }

        entries.push(relative_slash(workspace, path));
    }

    // Traversal order is irrelevant: the caller sorts matches before returning.
    entries
}

fn relative_slash(workspace: &Workspace, path: &Path) -> String {
    workspace.relative_display(path).replace('\\', "/")
}

fn normalize_pattern(pattern: &str) -> String {
    pattern.replace('\\', "/")
}

fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern_parts = pattern.split('/').collect::<Vec<_>>();
    let path_parts = path.split('/').collect::<Vec<_>>();
    glob_parts_match(&pattern_parts, &path_parts)
}

fn glob_parts_match(pattern: &[&str], path: &[&str]) -> bool {
    match (pattern.split_first(), path.split_first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some((&"**", rest)), None) => glob_parts_match(rest, path),
        (Some((&"**", rest)), Some((_, path_rest))) => {
            glob_parts_match(rest, path) || glob_parts_match(pattern, path_rest)
        }
        (Some((segment_pattern, pattern_rest)), Some((segment, path_rest))) => {
            segment_match(segment_pattern, segment) && glob_parts_match(pattern_rest, path_rest)
        }
        (Some(_), None) => false,
    }
}

fn segment_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let text = text.chars().collect::<Vec<_>>();
    let mut dp = vec![vec![false; text.len() + 1]; pattern.len() + 1];
    dp[0][0] = true;

    for index in 0..pattern.len() {
        match pattern[index] {
            '*' => {
                for text_index in 0..=text.len() {
                    if dp[index][text_index] {
                        dp[index + 1][text_index] = true;
                    }
                    if text_index > 0 && dp[index + 1][text_index - 1] {
                        dp[index + 1][text_index] = true;
                    }
                }
            }
            '?' => {
                for text_index in 0..text.len() {
                    if dp[index][text_index] {
                        dp[index + 1][text_index + 1] = true;
                    }
                }
            }
            literal => {
                for text_index in 0..text.len() {
                    if dp[index][text_index] && text[text_index] == literal {
                        dp[index + 1][text_index + 1] = true;
                    }
                }
            }
        }
    }

    dp[pattern.len()][text.len()]
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{EditFile, Glob, MAX_LINE_BYTES, ReadFile, WriteFile, glob_match};
    use crate::{
        tool::Tool,
        workspace::{Workspace, WorkspaceError},
    };

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "kuncode-filesystem-tool-test-{stamp}-{}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        async fn workspace(&self) -> Workspace {
            Workspace::new(&self.path)
                .await
                .expect("test workspace should be valid")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[tokio::test]
    async fn read_file_returns_a_line_window_with_pagination() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("notes.txt"), "one\ntwo\nthree\n")
            .expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        let output = tool
            .call(serde_json::json!({
                "path": "notes.txt",
                "start_line": 2,
                "limit": 1
            }))
            .await
            .expect("no harness-level error");

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

        let output = tool
            .call(serde_json::json!({ "path": "notes.txt" }))
            .await
            .expect("no harness-level error");

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

        let output = tool
            .call(serde_json::json!({ "path": "notes.txt", "start_line": 6 }))
            .await
            .expect("no harness-level error");

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

        let output = tool
            .call(serde_json::json!({ "path": "min.js" }))
            .await
            .expect("no harness-level error");

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

        let output = tool
            .call(serde_json::json!({ "path": "cjk.txt" }))
            .await
            .expect("no harness-level error");

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

        let output = tool
            .call(serde_json::json!({ "path": "big.txt" }))
            .await
            .expect("no harness-level error");

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

        let output = tool
            .call(serde_json::json!({ "path": "mixed.txt", "limit": 2 }))
            .await
            .expect("no harness-level error");

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

        let output = tool
            .call(serde_json::json!({ "path": "mixed.bin", "limit": 1 }))
            .await
            .expect("no harness-level error");

        assert!(!output.ok);
        assert_eq!(output.error.expect("error present").kind, "read");
    }

    #[tokio::test]
    async fn read_file_rejects_zero_start_line() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("notes.txt"), "a\nb").expect("file should be written");
        let tool = ReadFile::new(tmp.workspace().await);

        // `start_line` is 1-based; `0` is a contract violation, not "line 0".
        let output = tool
            .call(serde_json::json!({ "path": "notes.txt", "start_line": 0 }))
            .await
            .expect("no harness-level error");

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind,
            "invalid_arguments"
        );
    }

    #[tokio::test]
    async fn write_file_rejects_missing_parent() {
        let tmp = TestDir::new();
        let tool = WriteFile::new(tmp.workspace().await);

        let output = tool
            .call(serde_json::json!({
                "path": "missing/new.txt",
                "content": "hello"
            }))
            .await
            .expect("no harness-level error");

        assert!(!output.ok);
        assert_eq!(output.error.expect("error present").kind, "workspace_path");
    }

    #[tokio::test]
    async fn write_file_writes_inside_workspace() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join("src")).expect("directory should be created");
        let tool = WriteFile::new(tmp.workspace().await);

        let output = tool
            .call(serde_json::json!({
                "path": "src/new.txt",
                "content": "hello"
            }))
            .await
            .expect("no harness-level error");

        assert!(output.ok);
        assert_eq!(
            fs::read_to_string(tmp.path().join("src/new.txt")).unwrap(),
            "hello"
        );
        assert_eq!(output.data.expect("data present")["bytes"], 5);
    }

    #[tokio::test]
    async fn edit_file_replaces_once() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("notes.txt"), "target rest").expect("file should be written");
        let tool = EditFile::new(tmp.workspace().await);

        let output = tool
            .call(serde_json::json!({
                "path": "notes.txt",
                "old_text": "target",
                "new_text": "done"
            }))
            .await
            .expect("no harness-level error");

        assert!(output.ok);
        assert_eq!(
            fs::read_to_string(tmp.path().join("notes.txt")).unwrap(),
            "done rest"
        );
        assert_eq!(output.data.expect("data present")["replacements"], 1);
    }

    #[tokio::test]
    async fn edit_file_rejects_ambiguous_match() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("notes.txt"), "same same").expect("file should be written");
        let tool = EditFile::new(tmp.workspace().await);

        let output = tool
            .call(serde_json::json!({
                "path": "notes.txt",
                "old_text": "same",
                "new_text": "done"
            }))
            .await
            .expect("no harness-level error");

        assert!(!output.ok);
        assert_eq!(output.error.expect("error present").kind, "ambiguous_match");
        // The file is left untouched when the match is ambiguous.
        assert_eq!(
            fs::read_to_string(tmp.path().join("notes.txt")).unwrap(),
            "same same"
        );
    }

    #[tokio::test]
    async fn edit_file_reports_missing_text() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("notes.txt"), "hello").expect("file should be written");
        let tool = EditFile::new(tmp.workspace().await);

        let output = tool
            .call(serde_json::json!({
                "path": "notes.txt",
                "old_text": "missing",
                "new_text": "done"
            }))
            .await
            .expect("no harness-level error");

        assert!(!output.ok);
        assert_eq!(output.error.expect("error present").kind, "text_not_found");
    }

    #[tokio::test]
    async fn glob_returns_sorted_workspace_relative_matches() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join("src/bin")).expect("directory should be created");
        fs::write(tmp.path().join("src/lib.rs"), "").expect("file should be written");
        fs::write(tmp.path().join("src/bin/main.rs"), "").expect("file should be written");
        fs::write(tmp.path().join("README.md"), "").expect("file should be written");
        let tool = Glob::new(tmp.workspace().await);

        let output = tool
            .call(serde_json::json!({ "pattern": "**/*.rs" }))
            .await
            .expect("no harness-level error");

        assert!(output.ok);
        let data = output.data.expect("data present");
        assert_eq!(
            data["matches"],
            serde_json::json!(["src/bin/main.rs", "src/lib.rs"])
        );
        assert_eq!(data["total_matches"], 2);
    }

    #[tokio::test]
    async fn glob_respects_gitignore() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join(".gitignore"), "target/\nnode_modules/\n")
            .expect("gitignore should be written");
        fs::create_dir_all(tmp.path().join("target/debug")).expect("directory should be created");
        fs::create_dir_all(tmp.path().join("node_modules/pkg"))
            .expect("directory should be created");
        fs::write(tmp.path().join("target/debug/built.rs"), "").expect("file should be written");
        fs::write(tmp.path().join("node_modules/pkg/index.rs"), "")
            .expect("file should be written");
        fs::write(tmp.path().join("src.rs"), "").expect("file should be written");
        let tool = Glob::new(tmp.workspace().await);

        let output = tool
            .call(serde_json::json!({ "pattern": "**/*.rs" }))
            .await
            .expect("no harness-level error");

        assert!(output.ok);
        let data = output.data.expect("data present");
        // The project's own `.gitignore` prunes `target/` and `node_modules/`.
        assert_eq!(data["matches"], serde_json::json!(["src.rs"]));
        assert_eq!(data["total_matches"], 1);
    }

    #[tokio::test]
    async fn glob_always_skips_git_directory() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join(".git")).expect("directory should be created");
        fs::write(tmp.path().join(".git/packed.rs"), "").expect("file should be written");
        fs::write(tmp.path().join("keep.rs"), "").expect("file should be written");
        let tool = Glob::new(tmp.workspace().await);

        // Even with `include_ignored`, the VCS store must never be traversed.
        let output = tool
            .call(serde_json::json!({ "pattern": "**/*.rs", "include_ignored": true }))
            .await
            .expect("no harness-level error");

        assert!(output.ok);
        let data = output.data.expect("data present");
        assert_eq!(data["matches"], serde_json::json!(["keep.rs"]));
    }

    #[tokio::test]
    async fn glob_include_ignored_surfaces_gitignored_files() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join(".gitignore"), "build/\n").expect("gitignore should be written");
        fs::create_dir_all(tmp.path().join("build")).expect("directory should be created");
        fs::write(tmp.path().join("build/out.rs"), "").expect("file should be written");
        fs::write(tmp.path().join("keep.rs"), "").expect("file should be written");
        let tool = Glob::new(tmp.workspace().await);

        let output = tool
            .call(serde_json::json!({ "pattern": "**/*.rs", "include_ignored": true }))
            .await
            .expect("no harness-level error");

        assert!(output.ok);
        let data = output.data.expect("data present");
        // The escape hatch reaches files the project ignores by default.
        assert_eq!(
            data["matches"],
            serde_json::json!(["build/out.rs", "keep.rs"])
        );
        assert_eq!(data["total_matches"], 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn glob_drops_escaping_symlinks_but_keeps_internal_ones() {
        use std::os::unix::fs::symlink;

        let tmp = TestDir::new();
        let outside = tmp
            .path()
            .parent()
            .expect("temp root has parent")
            .join(format!("kuncode-glob-outside-{}.rs", std::process::id()));
        fs::write(&outside, "").expect("outside file should be written");
        fs::write(tmp.path().join("keep.rs"), "").expect("file should be written");
        symlink(&outside, tmp.path().join("escape_link.rs")).expect("symlink should be created");
        symlink(
            tmp.path().join("keep.rs"),
            tmp.path().join("inside_link.rs"),
        )
        .expect("symlink should be created");
        let tool = Glob::new(tmp.workspace().await);

        let output = tool
            .call(serde_json::json!({ "pattern": "**/*.rs" }))
            .await
            .expect("no harness-level error");

        let _ = fs::remove_file(outside);
        assert!(output.ok);
        let data = output.data.expect("data present");
        // The escaping link is dropped; the internal one stays, matching the
        // set `read_file` would actually allow.
        assert_eq!(
            data["matches"],
            serde_json::json!(["inside_link.rs", "keep.rs"])
        );
    }

    #[tokio::test]
    async fn glob_rejects_patterns_that_escape_workspace() {
        let tmp = TestDir::new();
        let tool = Glob::new(tmp.workspace().await);

        let output = tool
            .call(serde_json::json!({ "pattern": "../*.rs" }))
            .await
            .expect("no harness-level error");

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind,
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

        let output = tool
            .call(serde_json::json!({ "path": "link" }))
            .await
            .expect("no harness-level error");

        let _ = fs::remove_file(outside);
        assert!(!output.ok);
        assert_eq!(output.error.expect("error present").kind, "workspace_path");
    }

    #[test]
    fn glob_match_supports_segment_and_recursive_wildcards() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "src/main.rs"));
        assert!(glob_match("**/*.rs", "src/main.rs"));
        assert!(glob_match("src/**/main.??", "src/bin/main.rs"));
        assert!(!glob_match("src/**/main.??", "src/bin/main.txt"));
    }

    #[test]
    fn workspace_errors_are_model_recoverable() {
        let output: crate::tool::ToolOutput<()> =
            super::workspace_error(WorkspaceError::MissingFileName {
                path: PathBuf::from("."),
            });

        assert!(!output.ok);
        assert_eq!(output.error.expect("error present").kind, "workspace_path");
    }
}
