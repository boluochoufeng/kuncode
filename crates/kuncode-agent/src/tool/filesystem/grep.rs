//! The `grep` tool: search workspace file *contents* with a regular expression.
//!
//! Where `glob` and `ls` expose path names, this exposes what is written inside
//! the files, which is why it leans harder on the same policy seam both of them
//! use: a `Read` deny rule here is the difference between hiding a file name and
//! leaking a credential.
//!
//! Matching runs on ripgrep's own search core (`grep-searcher` + `grep-regex`)
//! rather than a spawned `rg`, so the walk — and therefore
//! [`PathVisibility`](crate::permission::PathVisibility) filtering, the
//! `.gitignore` handling, and the `.git` skip — stays inside this process where
//! the permission layer can still reach it.

use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use kuncode_core::completion::ToolDefinition;
use kuncode_core::non_empty_vec::NonEmptyVec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::helpers::{
    Walked, io_error, is_inside_vcs_store, non_empty_path, revalidate_path, walk_entries,
    workspace_error,
};
use crate::{
    glob::{glob_match, normalize_pattern},
    permission::{
        CanonicalPath, CanonicalToolInput, PathVisibility, PermissionCheckSpec, PermissionTarget,
        ToolDisplay,
    },
    tool::{
        PreparationContext, PreparedInvocationState, ToolContext, ToolError, ToolErrorKind,
        ToolOutput, TypedPreparation, TypedTool, definition_for, output::truncate_utf8,
    },
    workspace::Workspace,
};

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1_000;
const MAX_CONTEXT_LINES: usize = 5;

/// Per-line ceiling. A matching line in a minified bundle or a base64 blob can
/// be megabytes long, and `read_file` caps lines for the same reason — lower
/// here, because one call returns hundreds of lines rather than one file.
const MAX_LINE_BYTES: usize = 500;

/// How much of a match a search reports back.
///
/// The default is deliberately the cheapest one: a content search over a common
/// identifier can match tens of thousands of lines, so a caller locates files
/// first and asks for the lines of the few that matter.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrepOutputMode {
    /// Paths of matching files only. Each file stops at its first match.
    #[default]
    FilesWithMatches,
    /// Matching lines with line numbers, plus any requested context lines.
    Content,
    /// Number of matches per file, without the lines themselves.
    Count,
}

/// Arguments accepted by the [`Grep`] tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GrepArgs {
    /// Regular expression in Rust `regex` syntax: character classes, groups,
    /// alternation, repetition, `\b`, and inline flags like `(?i)` all work.
    /// Lookaround (`(?=`, `(?<=`) and backreferences (`\1`) are NOT supported —
    /// the engine is linear-time and has no backtracking.
    pattern: String,
    /// Workspace-relative or absolute directory to search under. Defaults to
    /// the workspace root.
    #[serde(default)]
    path: Option<String>,
    /// Restrict the search to paths matching this glob, e.g. `**/*.rs`. Applies
    /// to the workspace-relative path, in the same vocabulary the `glob` tool
    /// uses.
    #[serde(default)]
    glob: Option<String>,
    /// How much of each match to return. Defaults to `files_with_matches`,
    /// which is by far the cheapest; ask for `content` once the interesting
    /// files are known.
    #[serde(default)]
    output_mode: GrepOutputMode,
    /// Match without regard to case. Defaults to `false`.
    #[serde(default)]
    case_insensitive: bool,
    /// Lines of surrounding context to include with each match, up to 5. Only
    /// meaningful with `output_mode: "content"`.
    #[serde(default)]
    context_lines: Option<usize>,
    /// Let the pattern match across line boundaries, so `.` also matches a
    /// newline. Defaults to `false`, where a pattern that would span lines
    /// simply finds nothing — set this when searching for something that wraps.
    #[serde(default)]
    multiline: bool,
    /// Maximum number of results to return: matching files for
    /// `files_with_matches` and `count`, matching lines for `content`.
    #[serde(default)]
    limit: Option<usize>,
    /// How many results to skip before returning any, in the same unit as
    /// `limit`. Defaults to `0`. Feed back the `next_offset` from a previous
    /// result to continue where it left off; results are ordered by path, so
    /// the same pattern paginates consistently.
    #[serde(default)]
    offset: Option<usize>,
    /// Also search files hidden or excluded by `.gitignore`. The VCS store
    /// (`.git`) is always skipped. Defaults to `false`.
    #[serde(default)]
    include_ignored: bool,
}

/// One line reported by a content search.
#[derive(Debug, Serialize)]
pub struct GrepLine {
    /// One-based line number, in the same numbering `read_file` uses.
    pub line_number: u64,
    /// Line text with its terminator removed, capped at `MAX_LINE_BYTES`.
    pub text: String,
    /// `true` when this line is surrounding context rather than a match.
    /// Omitted for the matching lines themselves.
    #[serde(skip_serializing_if = "is_false")]
    pub context: bool,
    /// `true` when the line was too long and its tail was dropped. The dropped
    /// tail is not reachable by paging — read the file to recover it.
    #[serde(skip_serializing_if = "is_false")]
    pub truncated: bool,
}

/// One file that contained at least one match.
#[derive(Debug, Serialize)]
pub struct GrepFile {
    /// Workspace-relative, slash-separated path, usable as-is with `read_file`.
    pub path: String,
    /// Matches in this file. Absent in `files_with_matches` mode, where the
    /// search stops at the first hit and never learns the total.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_count: Option<usize>,
    /// Matching lines, present only in `content` mode. Holds fewer than
    /// `match_count` entries when `limit` runs out partway through the file.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<GrepLine>,
}

/// Files whose contents matched a pattern.
#[derive(Debug, Serialize)]
pub struct GrepOutput {
    /// Pattern that was searched for.
    pub pattern: String,
    /// Mode the results are shaped by.
    pub mode: GrepOutputMode,
    /// Matching files sorted by path, capped by `limit`.
    pub files: Vec<GrepFile>,
    /// Matching files found before the cap was applied.
    pub total_files: usize,
    /// Matches found across all files before the cap. Absent in
    /// `files_with_matches` mode, which stops at each file's first match and so
    /// never counts the rest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_matches: Option<usize>,
    /// Offset to pass back as `offset` to continue after the last result
    /// returned here, in the same unit as `limit`. Present only when more
    /// results remain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    /// Files actually opened and searched. A surprisingly small number here
    /// usually means `glob` or `path` narrowed the search more than intended.
    pub searched_files: usize,
    /// Files skipped for containing binary data (a NUL byte). Their contents
    /// are never reported, since a byte stream is noise to a reader and can be
    /// enormous.
    #[serde(skip_serializing_if = "super::helpers::is_zero")]
    pub skipped_binary: usize,
    /// Files that could not be opened or read. Counted rather than named: a
    /// search is not a diagnosis of one path, and one unreadable file must not
    /// fail the whole call.
    #[serde(skip_serializing_if = "super::helpers::is_zero")]
    pub unreadable_files: usize,
    /// Paths that could not even be considered because their names have no
    /// workspace-relative text form.
    #[serde(skip_serializing_if = "super::helpers::is_zero")]
    pub unrepresentable_paths: usize,
    /// Paths the permission policy withheld from the search, by count rather
    /// than by name.
    #[serde(skip_serializing_if = "super::helpers::is_zero")]
    pub hidden_by_policy: usize,
}

/// Compiled matcher paired with the validated search parameters.
///
/// The regex is compiled during preparation rather than execution: a bad
/// pattern is an argument error the caller can fix, and diagnosing it before
/// authorization keeps a doomed call from ever reaching the filesystem.
pub struct PreparedGrep {
    matcher: RegexMatcher,
    pattern: String,
    root: PathBuf,
    glob: Option<String>,
    mode: GrepOutputMode,
    context_lines: usize,
    multiline: bool,
    limit: usize,
    offset: usize,
    include_ignored: bool,
}

/// Searches workspace file contents with a regular expression.
#[derive(Clone, Debug)]
pub struct Grep {
    definition: ToolDefinition,
    workspace: Workspace,
}

impl Grep {
    /// Creates a content search tool bound to a workspace.
    pub fn new(workspace: Workspace) -> Self {
        Self {
            // The output schema is never sent — `definition_for` ships only the
            // argument schema — so what comes back has to be described here.
            definition: definition_for::<GrepArgs>(
                "grep",
                "Search the contents of workspace files with a regular expression. \
                 Returns the paths of matching files by default; set \
                 output_mode to \"content\" for the matching lines themselves. \
                 Every reported path is usable as-is with read_file and edit_file. \
                 Use the `glob` argument to restrict which files are searched \
                 rather than writing file names into the pattern. A result too \
                 long for one reply reports a `next_offset` to pass back as \
                 `offset` and read on. Prefer this over running grep or rg \
                 through bash.",
            ),
            workspace,
        }
    }
}

#[async_trait]
impl TypedTool for Grep {
    type Args = GrepArgs;
    type Prepared = PreparedGrep;
    type Output = GrepOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare_typed(
        &self,
        args: GrepArgs,
        _canonical_input: CanonicalToolInput,
        _ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
        let pattern = args.pattern.trim();
        if pattern.is_empty() {
            return Err(ToolOutput::failure(
                "invalid_arguments",
                "`pattern` must not be empty",
            ));
        }
        let matcher = build_matcher(pattern, args.case_insensitive, args.multiline)?;

        let root = non_empty_path(args.path.as_deref().unwrap_or("."))?;
        let resolved = self
            .workspace
            .resolve_target(root)
            .await
            .map_err(workspace_error)?;
        // The walk filter drops `.git` entries but never the walk root itself,
        // so a search rooted inside the VCS store would read the one tree every
        // tool here promises never to traverse.
        if is_inside_vcs_store(&self.workspace, &resolved) {
            return Err(ToolOutput::failure(
                "invalid_arguments",
                "`.git` and its contents are not searchable",
            ));
        }

        let glob = match args.glob.as_deref().map(str::trim) {
            Some("") => {
                return Err(ToolOutput::failure(
                    "invalid_arguments",
                    "`glob` must not be empty; omit it to search every file",
                ));
            }
            Some(glob) => Some(normalize_pattern(glob)),
            None => None,
        };

        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        if limit == 0 {
            return Err(ToolOutput::failure(
                "invalid_arguments",
                "`limit` must be greater than zero",
            ));
        }
        let context_lines = args.context_lines.unwrap_or(0);
        if context_lines > MAX_CONTEXT_LINES {
            return Err(ToolOutput::failure(
                "invalid_arguments",
                format!("`context_lines` must be at most {MAX_CONTEXT_LINES}"),
            ));
        }

        let canonical_path = CanonicalPath::from_absolute(&resolved)
            .map_err(|error| ToolOutput::failure("invalid_arguments", error.to_string()))?;
        let canonical_input = CanonicalToolInput::new(serde_json::json!({
            "pattern": pattern,
            "path": canonical_path.as_str(),
            "glob": glob,
            "output_mode": args.output_mode,
            "case_insensitive": args.case_insensitive,
            "context_lines": context_lines,
            "multiline": args.multiline,
            "limit": limit,
            "offset": args.offset.unwrap_or(0),
            "include_ignored": args.include_ignored,
        }));
        // Same shape as `glob` and `ls`: the searched directory is the whole
        // authorization surface, and the walk's own output is filtered entry by
        // entry, so a search rooted above a denied subtree cannot read it.
        let mut checks = NonEmptyVec::new(PermissionCheckSpec::new(PermissionTarget::Read(
            canonical_path,
        )));
        if args.include_ignored {
            let target = PermissionTarget::exact_tool("grep")
                .map_err(|error| ToolOutput::failure("invalid_arguments", error.to_string()))?;
            checks.push(PermissionCheckSpec::new(target));
        }

        let summary = search_summary(
            pattern,
            &self.workspace.relative_display(&resolved),
            glob.as_deref(),
            args.include_ignored,
        );
        Ok(TypedPreparation::new(
            PreparedGrep {
                matcher,
                pattern: pattern.to_string(),
                root: resolved,
                glob,
                mode: args.output_mode,
                context_lines,
                multiline: args.multiline,
                limit,
                offset: args.offset.unwrap_or(0),
                include_ignored: args.include_ignored,
            },
            canonical_input,
            checks,
            ToolDisplay::new(summary),
        ))
    }

    async fn run_prepared(
        &self,
        prepared: PreparedGrep,
        ctx: &ToolContext,
    ) -> ToolOutput<GrepOutput> {
        // Existence and type are diagnosed here rather than during preparation,
        // so an unauthorized path cannot reveal metadata by failing early.
        match tokio::fs::symlink_metadata(&prepared.root).await {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return ToolOutput::failure(
                    "not_a_directory",
                    format!(
                        "`{}` is not a directory; grep searches a directory tree",
                        self.workspace.relative_display(&prepared.root)
                    ),
                );
            }
            Err(err) => return io_error("search", &prepared.root, err, &self.workspace),
        }

        // Both the `ignore` walker and the searcher are synchronous and do
        // blocking IO, so the whole search runs on the blocking pool.
        let workspace = self.workspace.clone();
        let visibility = ctx.visibility.clone();
        let cancel = ctx.cancel.clone();
        match tokio::task::spawn_blocking(move || {
            search(&workspace, prepared, &visibility, &cancel)
        })
        .await
        {
            Ok(result) => result,
            Err(err) => ToolOutput::failure("internal", format!("search did not complete: {err}")),
        }
    }

    async fn revalidate_prepared(
        &self,
        prepared: &mut PreparedGrep,
        _ctx: &ToolContext,
    ) -> Result<PreparedInvocationState, ToolError> {
        revalidate_path(&self.workspace, &prepared.root).await
    }
}

/// Compiles the pattern, translating engine errors into argument diagnostics.
fn build_matcher(
    pattern: &str,
    case_insensitive: bool,
    multiline: bool,
) -> Result<RegexMatcher, ToolOutput> {
    let mut builder = RegexMatcherBuilder::new();
    builder
        .case_insensitive(case_insensitive)
        // Binary detection quits at the first NUL, so a pattern containing one
        // could never match. Banning it turns that into a build error the
        // caller can read instead of an empty result it has to guess about.
        .ban_byte(Some(b'\x00'));
    if multiline {
        builder.multi_line(true).dot_matches_new_line(true);
    } else {
        // Declaring the line terminator lets the engine search whole buffers
        // rather than line by line. It also rewrites the pattern so it cannot
        // match a newline — `(?s).` becomes `[^\n]` — which is exactly
        // ripgrep's own default: without `multiline`, a cross-line pattern
        // finds nothing rather than quietly spanning lines. Only a pattern
        // whose newline cannot be rewritten away (a literal `\n`) fails to
        // build, which is what the hint below is for.
        builder.line_terminator(Some(b'\n'));
    }
    builder.build(pattern).map_err(|error| {
        ToolOutput::failure(
            ToolErrorKind::InvalidArguments,
            format!(
                "`pattern` is not a valid regular expression: {error}\n\
                 Note: Rust regex syntax — lookaround and backreferences are unsupported; \
                 set `multiline: true` for a pattern that spans lines.",
            ),
        )
    })
}

/// Builds the approval-facing summary.
///
/// The authorized target is only the directory the search cannot leave, so what
/// is being looked for inside it — and whether the project's own ignore rules
/// are being bypassed — has to come from here or not at all.
fn search_summary(
    pattern: &str,
    directory: &str,
    glob: Option<&str>,
    include_ignored: bool,
) -> String {
    let mut summary = format!("Search file contents in `{directory}` for: {pattern}");
    if let Some(glob) = glob {
        summary.push_str(&format!(" (files matching {glob})"));
    }
    if include_ignored {
        summary.push_str(" (including ignored and hidden files)");
    }
    summary
}

/// One file the walk selected as searchable.
struct Candidate {
    path: PathBuf,
    relative: String,
}

/// Runs the whole search: walk, then match file by file.
///
/// Synchronous and blocking; the caller owns the blocking pool hand-off.
fn search(
    workspace: &Workspace,
    prepared: PreparedGrep,
    visibility: &PathVisibility,
    cancel: &CancellationToken,
) -> ToolOutput<GrepOutput> {
    let walked = collect_candidates(
        workspace,
        &prepared.root,
        prepared.include_ignored,
        prepared.glob.as_deref(),
        visibility,
    );
    let Walked {
        kept: mut candidates,
        unnameable,
        hidden,
    } = walked;
    // Sorted before searching, not after: the cap cuts a prefix, so the order
    // has to be decided before it is applied for a truncated result to be
    // reproducible.
    candidates.sort_by(|left, right| left.relative.cmp(&right.relative));

    let mut searcher = build_searcher(prepared.mode, prepared.context_lines, prepared.multiline);
    let by_line = matches!(prepared.mode, GrepOutputMode::Content);
    let mut files = Vec::new();
    let mut total_files = 0usize;
    let mut total_matches = 0usize;
    let mut collected_matches = 0usize;
    let mut skipped_files = 0usize;
    // Counted down across files, because `offset` is a position in the whole
    // result sequence and one file rarely spans it exactly.
    let mut pending_skip = if by_line { prepared.offset } else { 0 };
    let mut skipped_binary = 0usize;
    let mut unreadable_files = 0usize;
    let searched_files = candidates.len();

    for candidate in candidates {
        // The runner races this call against the token, but a blocking task is
        // not aborted by that race — without this check the thread would keep
        // reading files nobody is waiting for.
        if cancel.is_cancelled() {
            return ToolOutput::failure(ToolErrorKind::Cancelled, "search was cancelled");
        }

        // Only `content` mode collects lines; the other modes need the count
        // alone, so they ask for none and let the searcher stop early.
        let collect = if by_line {
            prepared.limit.saturating_sub(collected_matches)
        } else {
            0
        };
        let outcome = search_one(&mut searcher, &prepared, &candidate, pending_skip, collect);

        let Outcome::Matched {
            matches,
            collected,
            lines,
        } = outcome
        else {
            match outcome {
                Outcome::Binary => skipped_binary += 1,
                Outcome::Unreadable => unreadable_files += 1,
                Outcome::NoMatch | Outcome::Matched { .. } => {}
            }
            continue;
        };

        total_files += 1;
        total_matches += matches;

        if by_line {
            pending_skip = pending_skip.saturating_sub(matches);
            // A file wholly consumed by the offset, or reached after the limit
            // was already spent, contributes nothing to show.
            if collected == 0 {
                continue;
            }
            collected_matches += collected;
        } else if skipped_files < prepared.offset {
            skipped_files += 1;
            continue;
        } else if files.len() >= prepared.limit {
            continue;
        }

        files.push(GrepFile {
            path: candidate.relative,
            match_count: match prepared.mode {
                // First-match-and-stop never learns the real count, and
                // reporting the `1` it stopped at would read as a file with
                // exactly one match.
                GrepOutputMode::FilesWithMatches => None,
                GrepOutputMode::Content | GrepOutputMode::Count => Some(matches),
            },
            lines,
        });
    }

    let (consumed, total) = if by_line {
        (prepared.offset + collected_matches, total_matches)
    } else {
        (prepared.offset + files.len(), total_files)
    };
    // An offset past the end would otherwise return an empty list, which reads
    // as "the pattern matches nothing" — the one conclusion that stops a caller
    // from simply asking again from a valid position.
    if files.is_empty() && prepared.offset > 0 && total > 0 {
        let unit = if by_line { "matches" } else { "matching files" };
        return ToolOutput::failure(
            "offset_past_end",
            format!(
                "`offset` {} is past the last of {total} {unit}; \
                 search again from 0 or a smaller offset",
                prepared.offset,
            ),
        );
    }

    let output = ToolOutput::success(GrepOutput {
        pattern: prepared.pattern,
        mode: prepared.mode,
        files,
        total_files,
        total_matches: match prepared.mode {
            GrepOutputMode::FilesWithMatches => None,
            GrepOutputMode::Content | GrepOutputMode::Count => Some(total_matches),
        },
        next_offset: (consumed < total).then_some(consumed),
        searched_files,
        skipped_binary,
        unreadable_files,
        unrepresentable_paths: unnameable,
        hidden_by_policy: hidden,
    });

    if consumed < total {
        output.truncated()
    } else {
        output
    }
}

/// Walks the search root, keeping the regular files a search may open.
fn collect_candidates(
    workspace: &Workspace,
    root: &Path,
    include_ignored: bool,
    glob: Option<&str>,
    visibility: &PathVisibility,
) -> Walked<Candidate> {
    walk_entries(
        workspace,
        root,
        None,
        include_ignored,
        visibility,
        |entry| {
            // Symlinks are skipped outright, matching ripgrep's default of not
            // following them. A link into the workspace adds nothing — the walk
            // reaches its target on its own, so following it would only report
            // the same matches twice — and a link out of the workspace names
            // content no rule here was ever asked about.
            if !entry.file_type.is_file() {
                return None;
            }
            if let Some(glob) = glob
                && !glob_match(glob, &entry.relative)
            {
                return None;
            }
            Some(Candidate {
                path: entry.path.to_path_buf(),
                relative: entry.relative,
            })
        },
    )
}

fn build_searcher(mode: GrepOutputMode, context_lines: usize, multiline: bool) -> Searcher {
    let mut builder = SearcherBuilder::new();
    builder
        // Without this a `.png` would be read as text and its bytes reported as
        // matching lines. `quit` stops the file at the first NUL rather than
        // lossily converting it, which is what lets the file be reported as
        // skipped instead of half-searched.
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(matches!(mode, GrepOutputMode::Content))
        .multi_line(multiline);
    if matches!(mode, GrepOutputMode::Content) {
        builder
            .before_context(context_lines)
            .after_context(context_lines);
    }
    builder.build()
}

/// What searching one file produced.
enum Outcome {
    NoMatch,
    Matched {
        /// Every match in the file, regardless of what was skipped or kept, so
        /// the reported totals stay exact under pagination.
        matches: usize,
        /// How many of them contributed lines.
        collected: usize,
        lines: Vec<GrepLine>,
    },
    Binary,
    Unreadable,
}

/// Searches one candidate, skipping the first `skip` matches and then
/// collecting at most `collect` matches' worth of lines.
fn search_one(
    searcher: &mut Searcher,
    prepared: &PreparedGrep,
    candidate: &Candidate,
    skip: usize,
    collect: usize,
) -> Outcome {
    let Ok(file) = open_no_follow(&candidate.path) else {
        return Outcome::Unreadable;
    };

    let mut collector = MatchCollector {
        lines: Vec::new(),
        matches: 0,
        collected: 0,
        skip,
        collect,
        stop_at_first: matches!(prepared.mode, GrepOutputMode::FilesWithMatches),
        binary: false,
    };
    if searcher
        .search_file(&prepared.matcher, &file, &mut collector)
        .is_err()
    {
        return Outcome::Unreadable;
    }

    if collector.binary {
        // A file that turned out to be binary is reported as skipped even when
        // text above the first NUL matched: those lines are as likely to be
        // decoded noise as real content.
        return Outcome::Binary;
    }
    if collector.matches == 0 {
        return Outcome::NoMatch;
    }
    Outcome::Matched {
        matches: collector.matches,
        collected: collector.collected,
        lines: collector.lines,
    }
}

/// Accumulates one file's matches.
///
/// Implemented by hand rather than with `grep_searcher::sinks::UTF8`, because
/// that helper discards context lines and this tool reports them.
struct MatchCollector {
    lines: Vec<GrepLine>,
    /// Matches seen so far, counted even while skipping so that the file's
    /// reported total does not depend on the page being asked for.
    matches: usize,
    collected: usize,
    /// Matches to pass over before collecting anything, carried in from the
    /// caller's remaining `offset`.
    skip: usize,
    /// How many matches past `skip` may contribute lines; `0` counts without
    /// collecting.
    collect: usize,
    stop_at_first: bool,
    binary: bool,
}

impl MatchCollector {
    /// Whether the `ordinal`-th match of this file lands on the page being
    /// built, which is the window `(skip, skip + collect]`.
    fn wants(&self, ordinal: usize) -> bool {
        ordinal > self.skip && ordinal <= self.skip.saturating_add(self.collect)
    }
}

impl Sink for MatchCollector {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        self.matches += 1;
        if self.stop_at_first {
            return Ok(false);
        }
        if self.wants(self.matches) {
            self.collected += 1;
            // In multi-line mode one match can span several lines; the reported
            // number is the first line's, so the rest count up from it.
            let first = mat.line_number().unwrap_or(0);
            for (offset, line) in mat.lines().enumerate() {
                self.lines
                    .push(grep_line(first + offset as u64, line, false));
            }
        }
        Ok(true)
    }

    fn context(&mut self, _searcher: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, io::Error> {
        // A context line is only worth keeping when the match it belongs to is
        // on this page — otherwise a skipped match would still leak its
        // surroundings into the results.
        let ordinal = match ctx.kind() {
            SinkContextKind::Before => self.matches + 1,
            SinkContextKind::After | SinkContextKind::Other => self.matches,
        };
        if self.wants(ordinal) {
            self.lines
                .push(grep_line(ctx.line_number().unwrap_or(0), ctx.bytes(), true));
        }
        Ok(true)
    }

    fn binary_data(&mut self, _searcher: &Searcher, _offset: u64) -> Result<bool, io::Error> {
        self.binary = true;
        Ok(false)
    }
}

fn grep_line(line_number: u64, bytes: &[u8], context: bool) -> GrepLine {
    // Lossy rather than strict: a file can be valid text with a stray invalid
    // byte, and refusing the whole line would hide a real match. Genuinely
    // binary files never reach here — `BinaryDetection::quit` stopped them.
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_end_matches('\n').trim_end_matches('\r');
    let (text, truncated) = truncate_utf8(text, MAX_LINE_BYTES);
    GrepLine {
        line_number,
        text,
        context,
        truncated,
    }
}

/// Opens a file for searching without following a symlink in its final
/// component; see [`super::helpers::open_no_follow`] for the reasoning.
///
/// A blocking twin of that function rather than a caller of it: the searcher is
/// synchronous and takes a [`std::fs::File`], so going through the async one
/// would mean hopping back to the runtime for every candidate.
fn open_no_follow(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    {
        // Without `O_NOFOLLOW` the check has to precede the open, leaving
        // exactly the window the flag closes elsewhere.
        if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(io::Error::other("path is a symlink"));
        }
    }
    options.open(path)
}

/// Whether a flag is unset, for `serde(skip_serializing_if)`. A per-line flag
/// that is false on almost every line is pure noise in the payload.
fn is_false(flag: &bool) -> bool {
    !*flag
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use super::Grep;
    use crate::test_support::TestDir;
    use crate::{
        permission::{
            CanonicalPath, PermissionNamespace, PermissionTarget, PolicyEffect, PolicyOrigin,
            PolicySet, SessionPolicyOverlay,
        },
        tool::{PreparationContext, Tool, ToolContext, ToolOutput, execute_for_test},
    };

    async fn call(tool: Grep, args: serde_json::Value) -> ToolOutput {
        execute_for_test(Arc::new(tool), args, &ToolContext::new())
            .await
            .expect("no harness-level error")
    }

    #[tokio::test]
    async fn files_with_matches_is_the_default_and_reports_paths_only() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join("src")).expect("directory should be created");
        fs::write(
            tmp.path().join("src/lib.rs"),
            "fn alpha() {}\nfn beta() {}\n",
        )
        .expect("file should be written");
        fs::write(tmp.path().join("README.md"), "no functions here\n")
            .expect("file should be written");
        let tool = Grep::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "pattern": "^fn " })).await;

        assert!(output.ok);
        let data = output.data.expect("data present");
        assert_eq!(data["files"], serde_json::json!([{ "path": "src/lib.rs" }]));
        assert_eq!(data["total_files"], 1);
        // Stopping at the first match means the total is genuinely unknown, so
        // it is left out rather than reported as the `1` the search stopped at.
        assert!(data.get("total_matches").is_none());
    }

    #[tokio::test]
    async fn content_mode_returns_numbered_matching_lines() {
        let tmp = TestDir::new();
        fs::write(
            tmp.path().join("lib.rs"),
            "use std::io;\nfn alpha() {}\nfn beta() {}\n",
        )
        .expect("file should be written");
        let tool = Grep::new(tmp.workspace().await);

        let output = call(
            tool,
            serde_json::json!({ "pattern": "^fn ", "output_mode": "content" }),
        )
        .await;

        let data = output.data.expect("data present");
        assert_eq!(data["total_matches"], 2);
        assert_eq!(
            data["files"][0]["lines"],
            serde_json::json!([
                { "line_number": 2, "text": "fn alpha() {}" },
                { "line_number": 3, "text": "fn beta() {}" },
            ])
        );
    }

    #[tokio::test]
    async fn context_lines_are_marked_as_context() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("lib.rs"), "one\ntwo\nTARGET\nfour\nfive\n")
            .expect("file should be written");
        let tool = Grep::new(tmp.workspace().await);

        let output = call(
            tool,
            serde_json::json!({
                "pattern": "TARGET",
                "output_mode": "content",
                "context_lines": 1,
            }),
        )
        .await;

        let data = output.data.expect("data present");
        assert_eq!(
            data["files"][0]["lines"],
            serde_json::json!([
                { "line_number": 2, "text": "two", "context": true },
                { "line_number": 3, "text": "TARGET" },
                { "line_number": 4, "text": "four", "context": true },
            ])
        );
        // Context lines surround one match; they must not inflate the count.
        assert_eq!(data["total_matches"], 1);
    }

    #[tokio::test]
    async fn count_mode_reports_per_file_totals_without_lines() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("a.rs"), "x\nx\nx\n").expect("file should be written");
        fs::write(tmp.path().join("b.rs"), "x\n").expect("file should be written");
        let tool = Grep::new(tmp.workspace().await);

        let output = call(
            tool,
            serde_json::json!({ "pattern": "x", "output_mode": "count" }),
        )
        .await;

        let data = output.data.expect("data present");
        assert_eq!(
            data["files"],
            serde_json::json!([
                { "path": "a.rs", "match_count": 3 },
                { "path": "b.rs", "match_count": 1 },
            ])
        );
        assert_eq!(data["total_matches"], 4);
    }

    #[tokio::test]
    async fn glob_narrows_the_search_to_matching_paths() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join("src")).expect("directory should be created");
        fs::write(tmp.path().join("src/lib.rs"), "needle\n").expect("file should be written");
        fs::write(tmp.path().join("notes.md"), "needle\n").expect("file should be written");
        let tool = Grep::new(tmp.workspace().await);

        let output = call(
            tool,
            serde_json::json!({ "pattern": "needle", "glob": "**/*.rs" }),
        )
        .await;

        let data = output.data.expect("data present");
        assert_eq!(data["files"], serde_json::json!([{ "path": "src/lib.rs" }]));
        // The `.md` file was never opened, which is what `glob` is for.
        assert_eq!(data["searched_files"], 1);
    }

    #[tokio::test]
    async fn binary_files_are_skipped_rather_than_reported_as_text() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("blob.bin"), b"needle\x00\x01\x02needle").expect("write");
        fs::write(tmp.path().join("plain.txt"), "needle\n").expect("file should be written");
        let tool = Grep::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "pattern": "needle" })).await;

        let data = output.data.expect("data present");
        assert_eq!(data["files"], serde_json::json!([{ "path": "plain.txt" }]));
        assert_eq!(data["skipped_binary"], 1);
    }

    #[tokio::test]
    async fn gitignored_files_are_skipped_unless_asked_for() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join(".gitignore"), "build/\n").expect("gitignore written");
        fs::create_dir_all(tmp.path().join("build")).expect("directory should be created");
        fs::write(tmp.path().join("build/out.txt"), "needle\n").expect("file should be written");
        fs::write(tmp.path().join("keep.txt"), "needle\n").expect("file should be written");
        let tool = Grep::new(tmp.workspace().await);

        let default = call(tool.clone(), serde_json::json!({ "pattern": "needle" })).await;
        let widened = call(
            tool,
            serde_json::json!({ "pattern": "needle", "include_ignored": true }),
        )
        .await;

        assert_eq!(
            default.data.expect("data present")["files"],
            serde_json::json!([{ "path": "keep.txt" }])
        );
        assert_eq!(
            widened.data.expect("data present")["files"],
            serde_json::json!([{ "path": "build/out.txt" }, { "path": "keep.txt" }])
        );
    }

    #[tokio::test]
    async fn denied_paths_are_never_searched() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join("secrets")).expect("directory should be created");
        fs::write(tmp.path().join("secrets/prod.env"), "TOKEN=needle\n").expect("write");
        fs::write(tmp.path().join("app.env"), "TOKEN=needle\n").expect("write");
        let workspace = tmp.workspace().await;
        let root = CanonicalPath::from_absolute(workspace.root()).expect("absolute root");
        let mut policy = PolicySet::new(root);
        policy
            .compile_and_push("Read(secrets/**)", PolicyEffect::Deny, PolicyOrigin::User)
            .expect("rule compiles");
        let ctx = ToolContext::new()
            .with_visibility(policy.read_visibility(&SessionPolicyOverlay::default()));

        let output = execute_for_test(
            Arc::new(Grep::new(workspace)),
            serde_json::json!({ "pattern": "needle", "output_mode": "content" }),
            &ctx,
        )
        .await
        .expect("no harness-level error");

        // The point of the deny rule: the denied file's *contents* never appear.
        let data = output.data.expect("data present");
        assert_eq!(data["files"][0]["path"], "app.env");
        assert_eq!(data["total_files"], 1);
        assert_eq!(data["hidden_by_policy"], 1);
    }

    #[tokio::test]
    async fn limit_truncates_content_results_and_says_so() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("many.txt"), "x\n".repeat(10)).expect("file should be written");
        let tool = Grep::new(tmp.workspace().await);

        let output = call(
            tool,
            serde_json::json!({ "pattern": "x", "output_mode": "content", "limit": 3 }),
        )
        .await;

        assert!(output.truncated);
        let data = output.data.expect("data present");
        assert_eq!(
            data["files"][0]["lines"].as_array().expect("lines").len(),
            3
        );
        // The total is still exact, so the caller can tell how much it is missing.
        assert_eq!(data["total_matches"], 10);
    }

    #[tokio::test]
    async fn content_pages_across_files_without_gaps_or_repeats() {
        let tmp = TestDir::new();
        // Two files, so a page boundary has to fall inside one of them.
        fs::write(tmp.path().join("a.txt"), "x1\nx2\nx3\n").expect("file should be written");
        fs::write(tmp.path().join("b.txt"), "x4\nx5\n").expect("file should be written");
        let tool = Grep::new(tmp.workspace().await);

        let mut seen = Vec::new();
        let mut offset = Some(0);
        while let Some(next) = offset {
            let output = call(
                tool.clone(),
                serde_json::json!({
                    "pattern": "^x", "output_mode": "content", "limit": 2, "offset": next,
                }),
            )
            .await;
            let data = output.data.expect("data present");
            // The total is the whole search every time, not the page.
            assert_eq!(data["total_matches"], 5);
            for file in data["files"].as_array().expect("files is an array") {
                for line in file["lines"].as_array().expect("lines is an array") {
                    seen.push(line["text"].as_str().expect("text").to_string());
                }
            }
            offset = data["next_offset"].as_u64().map(|value| value as usize);
        }

        // Paging reassembles the search exactly once through, in order.
        assert_eq!(seen, ["x1", "x2", "x3", "x4", "x5"]);
    }

    #[tokio::test]
    async fn a_dense_file_keeps_every_match_across_pages() {
        let tmp = TestDir::new();
        // More matches in one file than a page holds, so the file spans pages.
        // Any per-file ceiling below `limit` would drop the matches between
        // that ceiling and the page boundary: they are neither collected nor
        // reachable from the next `offset`, which counts them as consumed.
        let dense: String = (1..=10).map(|n| format!("x{n:02}\n")).collect();
        fs::write(tmp.path().join("dense.txt"), dense).expect("file should be written");
        fs::write(tmp.path().join("tail.txt"), "x11\nx12\n").expect("file should be written");
        let tool = Grep::new(tmp.workspace().await);

        let mut seen = Vec::new();
        let mut offset = Some(0);
        while let Some(next) = offset {
            let output = call(
                tool.clone(),
                serde_json::json!({
                    "pattern": "^x", "output_mode": "content", "limit": 4, "offset": next,
                }),
            )
            .await;
            let data = output.data.expect("data present");
            for file in data["files"].as_array().expect("files is an array") {
                for line in file["lines"].as_array().expect("lines is an array") {
                    seen.push(line["text"].as_str().expect("text").to_string());
                }
            }
            offset = data["next_offset"].as_u64().map(|value| value as usize);
        }

        assert_eq!(
            seen,
            [
                "x01", "x02", "x03", "x04", "x05", "x06", "x07", "x08", "x09", "x10", "x11", "x12"
            ]
        );
    }

    #[tokio::test]
    async fn files_mode_pages_by_file() {
        let tmp = TestDir::new();
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(tmp.path().join(name), "needle\n").expect("file should be written");
        }
        let tool = Grep::new(tmp.workspace().await);

        let first = call(
            tool.clone(),
            serde_json::json!({ "pattern": "needle", "limit": 2 }),
        )
        .await;
        let second = call(
            tool,
            serde_json::json!({ "pattern": "needle", "limit": 2, "offset": 2 }),
        )
        .await;

        let first = first.data.expect("data present");
        assert_eq!(
            first["files"],
            serde_json::json!([{ "path": "a.txt" }, { "path": "b.txt" }])
        );
        assert_eq!(first["next_offset"], 2);
        let second = second.data.expect("data present");
        assert_eq!(second["files"], serde_json::json!([{ "path": "c.txt" }]));
        // The last page says so by leaving the offset out.
        assert!(second.get("next_offset").is_none());
    }

    #[tokio::test]
    async fn context_lines_do_not_leak_from_skipped_matches() {
        let tmp = TestDir::new();
        fs::write(
            tmp.path().join("a.txt"),
            "before1\nMATCH1\nafter1\nbefore2\nMATCH2\nafter2\n",
        )
        .expect("file should be written");
        let tool = Grep::new(tmp.workspace().await);

        let output = call(
            tool,
            serde_json::json!({
                "pattern": "MATCH", "output_mode": "content",
                "context_lines": 1, "offset": 1,
            }),
        )
        .await;

        // Only the second match is on this page, so only its neighbours come
        // with it — the skipped match must not drag its context along.
        assert_eq!(
            output.data.expect("data present")["files"][0]["lines"],
            serde_json::json!([
                { "line_number": 4, "text": "before2", "context": true },
                { "line_number": 5, "text": "MATCH2" },
                { "line_number": 6, "text": "after2", "context": true },
            ])
        );
    }

    #[tokio::test]
    async fn an_offset_past_the_end_is_reported_rather_than_returned_empty() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("a.txt"), "needle\n").expect("file should be written");
        let tool = Grep::new(tmp.workspace().await);

        let output = call(
            tool,
            serde_json::json!({ "pattern": "needle", "offset": 10 }),
        )
        .await;

        // An empty success here would read as "the pattern matches nothing",
        // which is exactly the wrong conclusion to hand back.
        assert!(!output.ok);
        let error = output.error.expect("error present");
        assert_eq!(error.kind.as_str(), "offset_past_end");
        assert!(error.message.contains('1'), "{}", error.message);
    }

    #[tokio::test]
    async fn an_offset_finds_nothing_when_the_pattern_itself_does_not_match() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("a.txt"), "needle\n").expect("file should be written");
        let tool = Grep::new(tmp.workspace().await);

        let output = call(
            tool,
            serde_json::json!({ "pattern": "absent", "offset": 10 }),
        )
        .await;

        // Nothing matched at all, so "past the end" would be misleading: the
        // honest answer is an empty result.
        assert!(output.ok);
        let data = output.data.expect("data present");
        assert_eq!(data["files"], serde_json::json!([]));
        assert_eq!(data["total_files"], 0);
    }

    #[tokio::test]
    async fn long_lines_are_capped_and_flagged() {
        let tmp = TestDir::new();
        fs::write(
            tmp.path().join("min.js"),
            format!("var x=\"needle{}\";\n", "a".repeat(2_000)),
        )
        .expect("file should be written");
        let tool = Grep::new(tmp.workspace().await);

        let output = call(
            tool,
            serde_json::json!({ "pattern": "needle", "output_mode": "content" }),
        )
        .await;

        let data = output.data.expect("data present");
        let line = &data["files"][0]["lines"][0];
        assert_eq!(line["truncated"], true);
        assert!(line["text"].as_str().expect("text").len() <= super::MAX_LINE_BYTES);
    }

    #[tokio::test]
    async fn multiline_is_required_for_a_pattern_that_spans_lines() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("a.txt"), "start\nend\n").expect("file should be written");
        let tool = Grep::new(tmp.workspace().await);

        let line_oriented = call(
            tool.clone(),
            serde_json::json!({ "pattern": "start(?s).*end" }),
        )
        .await;
        let spanning = call(
            tool,
            serde_json::json!({ "pattern": "start(?s).*end", "multiline": true }),
        )
        .await;

        // Line-oriented mode rewrites `(?s).` into "any byte but a newline", so
        // a cross-line pattern finds nothing rather than quietly spanning
        // lines. This is ripgrep's own default, and `multiline` is the way out.
        assert_eq!(
            line_oriented.data.expect("data present")["files"],
            serde_json::json!([])
        );
        assert_eq!(
            spanning.data.expect("data present")["files"],
            serde_json::json!([{ "path": "a.txt" }])
        );
    }

    #[tokio::test]
    async fn a_pattern_containing_a_newline_reports_the_way_out() {
        let tmp = TestDir::new();
        let tool = Grep::new(tmp.workspace().await);

        // Unlike `(?s).`, a literal newline cannot be rewritten away, so this
        // is the case that does fail to build — and the message has to name the
        // argument that makes it work.
        let output = call(tool, serde_json::json!({ "pattern": "start\\nend" })).await;

        assert!(!output.ok);
        let error = output.error.expect("error present");
        assert_eq!(error.kind.as_str(), "invalid_arguments");
        assert!(error.message.contains("multiline"), "{}", error.message);
    }

    #[tokio::test]
    async fn an_invalid_pattern_fails_before_touching_the_filesystem() {
        let tmp = TestDir::new();
        let tool = Grep::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "pattern": "fn (" })).await;

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "invalid_arguments"
        );
    }

    #[tokio::test]
    async fn a_search_authorizes_the_directory_it_cannot_leave() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join("src")).expect("directory should be created");
        let workspace = tmp.workspace().await;
        let expected = format!("{}/src", workspace.root().to_string_lossy());

        let preparation = Tool::prepare(
            Arc::new(Grep::new(workspace)),
            serde_json::json!({ "pattern": "needle", "path": "src" }),
            &PreparationContext::new(),
        )
        .await
        .expect("grep preparation succeeds");

        let target = preparation.checks().first().target();
        let PermissionTarget::Read(path) = target else {
            panic!("a search should authorize a read path, got {target}");
        };
        assert_eq!(path.as_str(), expected);
    }

    #[tokio::test]
    async fn include_ignored_adds_a_separate_approval_check() {
        let tmp = TestDir::new();
        let preparation = Tool::prepare(
            Arc::new(Grep::new(tmp.workspace().await)),
            serde_json::json!({ "pattern": "needle", "include_ignored": true }),
            &PreparationContext::new(),
        )
        .await
        .expect("grep preparation succeeds");

        assert_eq!(preparation.checks().len(), 2);
        assert!(
            preparation
                .checks()
                .iter()
                .any(|check| check.target().namespace() == PermissionNamespace::ExactTool)
        );
        // The authorized target is only a directory, so the pattern and the
        // bypass have to be visible in the summary an approver reads.
        let summary = preparation.display().summary();
        assert!(summary.contains("needle"), "summary was: {summary}");
        assert!(summary.contains("ignored"), "summary was: {summary}");
    }

    #[tokio::test]
    async fn the_vcs_store_is_not_searchable() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join(".git")).expect("directory should be created");
        fs::write(tmp.path().join(".git/config"), "needle\n").expect("file should be written");
        let tool = Grep::new(tmp.workspace().await);

        let rooted = call(
            tool.clone(),
            serde_json::json!({ "pattern": "needle", "path": ".git" }),
        )
        .await;
        let widened = call(
            tool,
            serde_json::json!({ "pattern": "needle", "include_ignored": true }),
        )
        .await;

        assert!(!rooted.ok);
        // Even the escape hatch must not reach into the VCS store.
        assert_eq!(
            widened.data.expect("data present")["files"],
            serde_json::json!([])
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_are_not_followed() {
        use std::os::unix::fs::symlink;

        let tmp = TestDir::new();
        let outside = tmp
            .path()
            .parent()
            .expect("temp root has parent")
            .join(format!("kuncode-grep-outside-{}.txt", std::process::id()));
        fs::write(&outside, "needle\n").expect("outside file should be written");
        fs::write(tmp.path().join("real.txt"), "needle\n").expect("file should be written");
        symlink(&outside, tmp.path().join("escape.txt")).expect("symlink should be created");
        symlink(tmp.path().join("real.txt"), tmp.path().join("alias.txt"))
            .expect("symlink should be created");
        let tool = Grep::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "pattern": "needle" })).await;

        let _ = fs::remove_file(outside);
        // The escaping link would read content outside the workspace; the
        // internal one would report the same file twice.
        assert_eq!(
            output.data.expect("data present")["files"],
            serde_json::json!([{ "path": "real.txt" }])
        );
    }
}
