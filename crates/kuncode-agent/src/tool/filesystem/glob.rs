//! The `glob` tool: list workspace paths matching a glob pattern.

use std::{
    ffi::OsStr,
    path::{Component, Path},
};

use async_trait::async_trait;
use ignore::WalkBuilder;
use kuncode_core::completion::ToolDefinition;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    glob::{glob_match, normalize_pattern},
    permission::{PermissionAction, PermissionRequest},
    tool::{ToolContext, ToolOutput, TypedTool, definition_for},
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

    fn permission(&self, args: &GlobArgs, _ctx: &ToolContext) -> PermissionRequest {
        let pattern = normalize_pattern(args.pattern.trim());
        PermissionRequest::new(
            "glob",
            PermissionAction::Read,
            Some(pattern.clone()),
            format!("Search files: {pattern}"),
        )
    }

    async fn run(&self, args: GlobArgs, _ctx: &ToolContext) -> ToolOutput<GlobOutput> {
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::Glob;
    use crate::tool::filesystem::test_support::TestDir;
    use crate::tool::{Tool, ToolContext};

    #[tokio::test]
    async fn glob_returns_sorted_workspace_relative_matches() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join("src/bin")).expect("directory should be created");
        fs::write(tmp.path().join("src/lib.rs"), "").expect("file should be written");
        fs::write(tmp.path().join("src/bin/main.rs"), "").expect("file should be written");
        fs::write(tmp.path().join("README.md"), "").expect("file should be written");
        let tool = Glob::new(tmp.workspace().await);

        let output = tool
            .call(
                serde_json::json!({ "pattern": "**/*.rs" }),
                &ToolContext::new(),
            )
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
            .call(
                serde_json::json!({ "pattern": "**/*.rs" }),
                &ToolContext::new(),
            )
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
            .call(
                serde_json::json!({ "pattern": "**/*.rs", "include_ignored": true }),
                &ToolContext::new(),
            )
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
            .call(
                serde_json::json!({ "pattern": "**/*.rs", "include_ignored": true }),
                &ToolContext::new(),
            )
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
            .call(
                serde_json::json!({ "pattern": "**/*.rs" }),
                &ToolContext::new(),
            )
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
            .call(
                serde_json::json!({ "pattern": "../*.rs" }),
                &ToolContext::new(),
            )
            .await
            .expect("no harness-level error");

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "invalid_arguments"
        );
    }
}
