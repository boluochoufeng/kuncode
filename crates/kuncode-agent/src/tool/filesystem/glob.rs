//! The `glob` tool: list workspace paths matching a glob pattern.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use kuncode_core::non_empty_vec::NonEmptyVec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::helpers::{SymlinkTarget, Walked, symlink_target, walk_entries};
use crate::{
    glob::{glob_match, normalize_pattern},
    permission::{
        CanonicalPath, CanonicalToolInput, PathVisibility, PermissionCheckSpec, PermissionTarget,
        ToolDisplay,
    },
    tool::{
        PreparationContext, ToolContext, ToolOutput, TypedPreparation, TypedTool, definition_for,
    },
    workspace::Workspace,
};

const DEFAULT_GLOB_LIMIT: usize = 200;
const MAX_GLOB_LIMIT: usize = 1_000;

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
    /// Paths that could not even be considered because their names have no
    /// workspace-relative text form.
    #[serde(skip_serializing_if = "super::helpers::is_zero")]
    pub unrepresentable_paths: usize,
    /// Paths the permission policy withheld from the search. Reported as a
    /// count rather than by name, and not as a failure: unlike a listing, a
    /// search returning nothing is an ordinary answer, and these entries need
    /// not have matched the pattern in the first place.
    #[serde(skip_serializing_if = "super::helpers::is_zero")]
    pub hidden_by_policy: usize,
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
    type Prepared = GlobArgs;
    type Output = GlobOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare_typed(
        &self,
        mut args: GlobArgs,
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
        validate_glob_pattern(pattern)
            .map_err(|message| ToolOutput::failure("invalid_arguments", message))?;
        let limit = args.limit.unwrap_or(DEFAULT_GLOB_LIMIT).min(MAX_GLOB_LIMIT);
        if limit == 0 {
            return Err(ToolOutput::failure(
                "invalid_arguments",
                "`limit` must be greater than zero",
            ));
        }
        let pattern = normalize_pattern(pattern);
        args.pattern = pattern.clone();
        args.limit = Some(limit);
        // A search names the deepest directory it cannot leave, not the pattern
        // it expands: rules decide paths, and the walk's own output is filtered
        // entry by entry, so a broad pattern cannot surface a denied path.
        let anchor = CanonicalPath::from_absolute(&self.workspace.root().join(anchor(&pattern)))
            .map_err(|error| ToolOutput::failure("invalid_arguments", error.to_string()))?;
        let canonical_input = CanonicalToolInput::new(serde_json::json!({
            "pattern": pattern,
            "limit": limit,
            "include_ignored": args.include_ignored,
        }));
        let mut checks = NonEmptyVec::new(PermissionCheckSpec::new(PermissionTarget::Read(anchor)));
        if args.include_ignored {
            let target = PermissionTarget::exact_tool("glob")
                .map_err(|error| ToolOutput::failure("invalid_arguments", error.to_string()))?;
            checks.push(PermissionCheckSpec::new(target));
        }
        Ok(TypedPreparation::new(
            args,
            canonical_input,
            checks,
            ToolDisplay::new("Search workspace paths"),
        ))
    }

    async fn run_prepared(&self, prepared: GlobArgs, ctx: &ToolContext) -> ToolOutput<GlobOutput> {
        let pattern = prepared.pattern;
        let limit = prepared
            .limit
            .unwrap_or(DEFAULT_GLOB_LIMIT)
            .min(MAX_GLOB_LIMIT);

        // The `ignore` walker is synchronous and thread-based, so the whole
        // tree walk runs on the blocking pool to keep the async runtime free.
        let workspace = self.workspace.clone();
        let include_ignored = prepared.include_ignored;
        let visibility = ctx.visibility.clone();
        let walked = match tokio::task::spawn_blocking(move || {
            walk_workspace(&workspace, include_ignored, &visibility)
        })
        .await
        {
            Ok(walked) => walked,
            Err(err) => {
                return ToolOutput::failure(
                    "internal",
                    format!("workspace walk did not complete: {err}"),
                );
            }
        };

        let normalized_pattern = pattern.clone();
        let mut matches = walked
            .kept
            .into_iter()
            .filter(|entry| glob_match(&normalized_pattern, entry))
            .collect::<Vec<_>>();
        matches.sort();

        let total_matches = matches.len();
        let truncated = total_matches > limit;
        matches.truncate(limit);

        let output = ToolOutput::success(GlobOutput {
            pattern,
            matches,
            total_matches,
            unrepresentable_paths: walked.unnameable,
            hidden_by_policy: walked.hidden,
        });

        if truncated {
            output.truncated()
        } else {
            output
        }
    }
}

/// Returns the leading wildcard-free segments of a pattern: the directory the
/// search is confined to, and therefore the path a rule gets to decide.
///
/// `secrets/*.key` yields `secrets`, `src/**/*.rs` yields `src`, and a pattern
/// that starts with a wildcard yields the workspace root itself.
fn anchor(pattern: &str) -> PathBuf {
    let segments = pattern.split('/').collect::<Vec<_>>();
    let literal = segments
        .iter()
        .take_while(|segment| !segment.contains(['*', '?']))
        .count();
    // A pattern with no wildcard at all names one file, and its last segment is
    // that name rather than a directory the search stays inside.
    let directories = literal.min(segments.len().saturating_sub(1));
    segments[..directories].iter().collect()
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

/// Walks the whole workspace, returning every entry as a workspace-relative,
/// slash-separated path, plus the count of entries that have no such path.
///
/// Matching happens on the relative text, so an entry without one cannot be
/// tested against the pattern at all — hence the count, so that an empty result
/// does not read as "no such file" while something stands there unnamed.
///
/// Traversal order is irrelevant: the caller sorts matches before returning.
/// Synchronous and thread-based; callers run it on the blocking pool.
fn walk_workspace(
    workspace: &Workspace,
    include_ignored: bool,
    visibility: &PathVisibility,
) -> Walked<String> {
    walk_entries(
        workspace,
        workspace.root(),
        None,
        include_ignored,
        visibility,
        |entry| {
            // A search advertises only links it could actually hand to
            // `read_file`/`write_file`, so anything that does not resolve inside the
            // workspace — escaping or dangling — is dropped.
            if entry.file_type.is_symlink()
                && symlink_target(entry.path, workspace) != SymlinkTarget::Inside
            {
                return None;
            }
            Some(entry.relative)
        },
    )
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use super::Glob;
    use crate::test_support::TestDir;
    use crate::{
        permission::{
            CanonicalPath, PermissionNamespace, PermissionTarget, PolicyEffect, PolicyOrigin,
            PolicySet, SessionPolicyOverlay,
        },
        tool::{PreparationContext, Tool, ToolContext, ToolOutput, execute_for_test},
    };

    async fn call(tool: Glob, args: serde_json::Value) -> ToolOutput {
        execute_for_test(Arc::new(tool), args, &ToolContext::new())
            .await
            .expect("no harness-level error")
    }

    #[tokio::test]
    async fn glob_returns_sorted_workspace_relative_matches() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join("src/bin")).expect("directory should be created");
        fs::write(tmp.path().join("src/lib.rs"), "").expect("file should be written");
        fs::write(tmp.path().join("src/bin/main.rs"), "").expect("file should be written");
        fs::write(tmp.path().join("README.md"), "").expect("file should be written");
        let tool = Glob::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "pattern": "**/*.rs" })).await;

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

        let output = call(tool, serde_json::json!({ "pattern": "**/*.rs" })).await;

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
        let output = call(
            tool,
            serde_json::json!({ "pattern": "**/*.rs", "include_ignored": true }),
        )
        .await;

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

        let output = call(
            tool,
            serde_json::json!({ "pattern": "**/*.rs", "include_ignored": true }),
        )
        .await;

        assert!(output.ok);
        let data = output.data.expect("data present");
        // The escape hatch reaches files the project ignores by default.
        assert_eq!(
            data["matches"],
            serde_json::json!(["build/out.rs", "keep.rs"])
        );
        assert_eq!(data["total_matches"], 2);
    }

    #[tokio::test]
    async fn denied_paths_are_withheld_from_a_search_that_spans_them() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join("secrets")).expect("directory should be created");
        fs::write(tmp.path().join("secrets/prod.key"), "").expect("file should be written");
        fs::write(tmp.path().join("local.key"), "").expect("file should be written");
        let workspace = tmp.workspace().await;
        let root = CanonicalPath::from_absolute(workspace.root()).expect("absolute root");
        let mut policy = PolicySet::new(root);
        policy
            .compile_and_push("Read(secrets/**)", PolicyEffect::Deny, PolicyOrigin::User)
            .expect("rule compiles");
        let ctx = ToolContext::new()
            .with_visibility(policy.read_visibility(&SessionPolicyOverlay::default()));

        let output = execute_for_test(
            Arc::new(Glob::new(workspace)),
            serde_json::json!({ "pattern": "**/*.key" }),
            &ctx,
        )
        .await
        .expect("no harness-level error");

        let data = output.data.expect("data present");
        assert_eq!(data["matches"], serde_json::json!(["local.key"]));
        assert_eq!(data["hidden_by_policy"], 1);
    }

    #[tokio::test]
    async fn a_search_authorizes_the_directory_it_cannot_leave() {
        let tmp = TestDir::new();
        let workspace = tmp.workspace().await;
        let root = workspace.root().to_string_lossy().to_string();

        for (pattern, expected) in [
            ("secrets/*.key", format!("{root}/secrets")),
            ("src/**/*.rs", format!("{root}/src")),
            ("docs/api/*.md", format!("{root}/docs/api")),
            // Nothing constrains a leading wildcard, so the search is only
            // bounded by the workspace itself.
            ("**/*.key", root.clone()),
            ("*.rs", root.clone()),
            // A wildcard-free pattern names one file; the directory holding it
            // is what a rule gets to decide.
            ("src/lib.rs", format!("{root}/src")),
        ] {
            let preparation = Tool::prepare(
                Arc::new(Glob::new(workspace.clone())),
                serde_json::json!({ "pattern": pattern }),
                &PreparationContext::new(),
            )
            .await
            .expect("glob preparation succeeds");

            let target = preparation.checks().first().target();
            let PermissionTarget::Read(path) = target else {
                panic!("{pattern} should authorize a read path, got {target}");
            };
            assert_eq!(path.as_str(), expected, "{pattern}");
        }
    }

    #[tokio::test]
    async fn include_ignored_adds_a_separate_approval_check() {
        let tmp = TestDir::new();
        let preparation = Tool::prepare(
            Arc::new(Glob::new(tmp.workspace().await)),
            serde_json::json!({ "pattern": "**/*", "include_ignored": true }),
            &PreparationContext::new(),
        )
        .await
        .expect("glob preparation succeeds");

        assert_eq!(preparation.checks().len(), 2);
        assert!(
            preparation
                .checks()
                .iter()
                .any(|check| check.target().namespace() == PermissionNamespace::ExactTool)
        );
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

        let output = call(tool, serde_json::json!({ "pattern": "**/*.rs" })).await;

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

    #[cfg(unix)]
    #[tokio::test]
    async fn a_backslash_in_a_name_is_matched_as_part_of_the_name() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("weird\\name.rs"), "").expect("file should be written");
        let tool = Glob::new(tmp.workspace().await);

        // The file lives at the workspace root, so `*.rs` must find it while
        // the nested-looking pattern must not.
        let flat = call(tool.clone(), serde_json::json!({ "pattern": "*.rs" })).await;
        let nested = call(tool, serde_json::json!({ "pattern": "weird/*.rs" })).await;

        assert_eq!(
            flat.data.expect("data present")["matches"],
            serde_json::json!(["weird\\name.rs"])
        );
        assert_eq!(
            nested.data.expect("data present")["matches"],
            serde_json::json!([])
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn names_that_are_not_utf8_are_counted_as_unconsidered() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

        let tmp = TestDir::new();
        fs::write(tmp.path().join(OsStr::from_bytes(b"caf\xff.rs")), "")
            .expect("file should be written");
        let tool = Glob::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "pattern": "*.rs" })).await;

        // Matching runs on the relative text, so a name without one is never
        // tested. Saying so keeps an empty result from reading as "no such
        // file".
        let data = output.data.expect("data present");
        assert_eq!(data["matches"], serde_json::json!([]));
        assert_eq!(data["unrepresentable_paths"], 1);
    }

    #[tokio::test]
    async fn glob_rejects_patterns_that_escape_workspace() {
        let tmp = TestDir::new();
        let tool = Glob::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "pattern": "../*.rs" })).await;

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "invalid_arguments"
        );
    }
}
