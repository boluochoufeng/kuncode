//! The `ls` tool: list the entries of one workspace directory.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use kuncode_core::non_empty_vec::NonEmptyVec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::helpers::{
    SymlinkTarget, Walked, io_error, is_inside_vcs_store, non_empty_path, revalidate_path,
    symlink_target, walk_entries, workspace_error,
};
use crate::{
    permission::{
        CanonicalPath, CanonicalToolInput, PathVisibility, PermissionCheckSpec, PermissionTarget,
        ToolDisplay,
    },
    tool::{
        PreparationContext, PreparedInvocationState, ToolContext, ToolError, ToolOutput,
        TypedPreparation, TypedTool, definition_for,
    },
    workspace::Workspace,
};

/// Context-safety cap on returned entries.
///
/// Deliberately a constant rather than an argument: there is no offset to
/// resume from, so a caller-supplied limit could only shrink the answer, never
/// reach the rest of it. [`LsOutput::total_entries`] is what a caller acts on
/// instead — it reports how much was left out, so a truncated listing can be
/// narrowed (list a subdirectory) or handed to `glob` with a pattern.
const LS_ENTRY_CAP: usize = 200;

/// Arguments accepted by the [`Ls`] tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LsArgs {
    /// Workspace-relative or absolute directory path. Defaults to the workspace
    /// root.
    #[serde(default)]
    path: Option<String>,
    /// How many directory levels to descend. `1` (the default) lists only the
    /// directory's own entries; `2` also lists what its subdirectories hold,
    /// and so on. Raise it to understand how a tree is organized — to find
    /// files by name at any depth, use `glob` instead.
    #[serde(default)]
    depth: Option<usize>,
    /// Also list entries hidden or excluded by `.gitignore`. The VCS store
    /// (`.git`) is never listed and cannot be listed directly. Defaults to
    /// `false`.
    #[serde(default)]
    include_ignored: bool,
}

/// Kind of one listed filesystem entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LsEntryKind {
    /// A directory. Descended into only while `depth` allows.
    Directory,
    /// A regular file.
    File,
    /// A symbolic link, never followed. Links resolving outside the workspace
    /// are omitted from the listing entirely, since the file tools refuse to
    /// act on them; a link that resolves nowhere (dangling, a cycle) is still
    /// listed, because its absence would read as "no such entry" and `ls` is
    /// the tool used to diagnose exactly that.
    Symlink,
}

/// One entry of a listed directory.
#[derive(Debug, Serialize)]
pub struct LsEntry {
    /// Workspace-relative, slash-separated path, in the same vocabulary `glob`
    /// reports and the file tools accept.
    ///
    /// Deliberately not a bare entry name: the value a caller has in hand is the
    /// value it will pass to `read_file`, and a name that first has to be joined
    /// with [`LsOutput::directory`] is a trap — the join is invisible in the
    /// output, and the output schema never reaches the model at all.
    pub path: String,
    /// What the entry is.
    pub kind: LsEntryKind,
    /// Size in bytes of a regular file, omitted whenever metadata is
    /// unreadable. Also omitted for directories, and for symlinks — where the
    /// only cheap answer is the length of the link itself, which a reader would
    /// almost certainly mistake for the size of its target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// Entries found under one workspace directory.
#[derive(Debug, Serialize)]
pub struct LsOutput {
    /// Directory that was listed, workspace-relative (`.` for the root).
    pub directory: String,
    /// Matching entries sorted by path, capped for context safety.
    pub entries: Vec<LsEntry>,
    /// Total entries found before the cap was applied. Compare against
    /// [`Self::entries`] to see how much the listing left out.
    pub total_entries: usize,
    /// Entries counted in [`Self::total_entries`] but impossible to name,
    /// because their file names have no workspace-relative text form.
    #[serde(skip_serializing_if = "super::helpers::is_zero")]
    pub unrepresentable_entries: usize,
    /// Entries the permission policy withheld, by count rather than by name.
    /// Left out of [`Self::total_entries`]: what a caller can act on is what it
    /// can see, and this says how much of the directory it is not seeing.
    #[serde(skip_serializing_if = "super::helpers::is_zero")]
    pub hidden_by_policy: usize,
}

/// Canonical directory paired with validated listing arguments.
#[derive(Debug)]
pub struct PreparedLs {
    path: PathBuf,
    display_path: String,
    depth: usize,
    include_ignored: bool,
}

/// Lists workspace directories.
#[derive(Clone, Debug)]
pub struct Ls {
    definition: ToolDefinition,
    workspace: Workspace,
}

impl Ls {
    /// Creates a directory listing tool bound to a workspace.
    pub fn new(workspace: Workspace) -> Self {
        Self {
            // The output schema is never sent — `definition_for` ships only the
            // argument schema — so the shape of what comes back has to be said
            // here or not at all.
            definition: definition_for::<LsArgs>(
                "ls",
                "List what a workspace directory contains, one level deep by \
                 default and further with `depth`. Each entry reports a \
                 workspace-relative path usable as-is with read_file and \
                 edit_file. Use it to see how a tree is organized; use glob to \
                 find files by name at any depth, and grep to search their \
                 contents.",
            ),
            workspace,
        }
    }
}

#[async_trait]
impl TypedTool for Ls {
    type Args = LsArgs;
    type Prepared = PreparedLs;
    type Output = LsOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare_typed(
        &self,
        args: LsArgs,
        _canonical_input: CanonicalToolInput,
        _ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
        let path = non_empty_path(args.path.as_deref().unwrap_or("."))?;
        let resolved = self
            .workspace
            .resolve_target(path)
            .await
            .map_err(workspace_error)?;

        // The walk filter drops `.git` entries but never the walk root itself,
        // so without this a listing rooted inside the VCS store would enumerate
        // it — the one thing every tool here promises never to traverse.
        if is_inside_vcs_store(&self.workspace, &resolved) {
            return Err(ToolOutput::failure(
                "invalid_arguments",
                "`.git` and its contents are not listable",
            ));
        }

        let canonical_path = CanonicalPath::from_absolute(&resolved)
            .map_err(|error| ToolOutput::failure("invalid_arguments", error.to_string()))?;
        // Not `relative_display`: this string is reported back as the listed
        // directory and prefixes every entry path, so a lossy rendering would
        // hand the caller names that resolve somewhere else.
        let Some(display_path) = self.workspace.relative_path(&resolved) else {
            return Err(ToolOutput::failure(
                "invalid_arguments",
                "the resolved directory name is not valid UTF-8 and cannot be listed",
            ));
        };
        let depth = args.depth.unwrap_or(1);
        if depth == 0 {
            return Err(ToolOutput::failure(
                "invalid_arguments",
                "`depth` must be at least 1; a depth of 0 would list nothing",
            ));
        }
        let canonical_input = CanonicalToolInput::new(serde_json::json!({
            "path": canonical_path.as_str(),
            "depth": depth,
            "include_ignored": args.include_ignored,
        }));
        // The listed directory is the whole authorization surface: a rule
        // decides this path, and what the walk then produces is filtered entry
        // by entry, so a listing rooted above a denied subtree cannot surface
        // it. A second check naming the subtree would only restate this one.
        let mut checks = NonEmptyVec::new(PermissionCheckSpec::new(PermissionTarget::Read(
            canonical_path,
        )));
        // Reaching past the project's own ignore rules can surface files it
        // deliberately keeps out of sight (`.env`, credentials), so the escape
        // hatch is authorized separately from the directory read.
        if args.include_ignored {
            let target = PermissionTarget::exact_tool("ls")
                .map_err(|error| ToolOutput::failure("invalid_arguments", error.to_string()))?;
            checks.push(PermissionCheckSpec::new(target));
        }
        Ok(TypedPreparation::new(
            PreparedLs {
                path: resolved,
                display_path: display_path.clone(),
                depth,
                include_ignored: args.include_ignored,
            },
            canonical_input,
            checks,
            ToolDisplay::new(listing_summary(&display_path, depth, args.include_ignored)),
        ))
    }

    async fn run_prepared(&self, prepared: PreparedLs, ctx: &ToolContext) -> ToolOutput<LsOutput> {
        let PreparedLs {
            path,
            display_path,
            depth,
            include_ignored,
        } = prepared;

        // Existence and type are diagnosed here rather than during preparation,
        // so an unauthorized path cannot reveal metadata by failing early.
        //
        // `symlink_metadata`, not `metadata`: a prepared path is resolved and
        // therefore not a symlink, so one appearing here was swapped in after
        // the check and must not be walked. Unlike the file tools this is a
        // check and not an atomic open — the walker reopens the root by path —
        // so it narrows the window rather than closing it.
        match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return ToolOutput::failure(
                    "not_a_directory",
                    format!("`{display_path}` is not a directory"),
                );
            }
            Err(err) => return io_error("list", &path, err, &self.workspace),
        }

        // A directory can be stat-able yet unopenable (mode 0700 owned by
        // another user). The walker reports that as a per-entry error, which the
        // walk loop skips like any other unreadable entry — leaving a result
        // byte-for-byte identical to a genuinely empty directory. Opening it
        // here turns that silence into a diagnostic.
        if let Err(err) = tokio::fs::read_dir(&path).await {
            return io_error("list", &path, err, &self.workspace);
        }

        // The `ignore` walker is synchronous and thread-based, so the walk runs
        // on the blocking pool to keep the async runtime free.
        let workspace = self.workspace.clone();
        let directory = path.clone();
        let visibility = ctx.visibility.clone();
        let listing = match tokio::task::spawn_blocking(move || {
            walk_directory(&workspace, &directory, depth, include_ignored, &visibility)
        })
        .await
        {
            Ok(listing) => listing,
            Err(err) => {
                return ToolOutput::failure(
                    "internal",
                    format!("directory walk did not complete: {err}"),
                );
            }
        };

        // The cap is not the only reason the listing can be short of the total:
        // an unnameable entry is counted and then left out too, and either way
        // the result is incomplete.
        // Everything here being withheld is a permission answer, not an empty
        // directory, and saying so keeps a caller from concluding the directory
        // holds nothing and moving on.
        if listing.entries.is_empty() && listing.hidden > 0 {
            return ToolOutput::failure(
                "permission_denied",
                format!("every entry of `{display_path}` is withheld by permission policy"),
            );
        }
        let truncated = listing.total > listing.entries.len();
        let output = ToolOutput::success(LsOutput {
            directory: display_path,
            entries: listing.entries,
            total_entries: listing.total,
            unrepresentable_entries: listing.unrepresentable,
            hidden_by_policy: listing.hidden,
        });

        if truncated {
            output.truncated()
        } else {
            output
        }
    }

    async fn revalidate_prepared(
        &self,
        prepared: &mut PreparedLs,
        _ctx: &ToolContext,
    ) -> Result<PreparedInvocationState, ToolError> {
        revalidate_path(&self.workspace, &prepared.path).await
    }
}

/// Collects the entries under `directory` down to `depth` levels, returning the
/// capped listing together with the total found.
///
/// The walk is rooted at `directory` rather than at the workspace root, so an
/// explicit listing of a hidden or ignored directory (`ls target`) still
/// returns its contents; [`walk_entries`] supplies the ignore rules declared
/// above `directory` separately, so everything *inside* stays filtered exactly
/// as it would be when seen from the workspace root.
///
/// Synchronous and thread-based; callers run it on the blocking pool.
fn walk_directory(
    workspace: &Workspace,
    directory: &Path,
    depth: usize,
    include_ignored: bool,
    visibility: &PathVisibility,
) -> Listing {
    let walked = walk_entries(
        workspace,
        directory,
        Some(depth),
        include_ignored,
        visibility,
        |entry| {
            let kind = if entry.file_type.is_dir() {
                LsEntryKind::Directory
            } else if entry.file_type.is_symlink() {
                // Only a link that resolves *outside* is dropped; see
                // [`LsEntryKind::Symlink`] for why a dangling one is kept.
                if symlink_target(entry.path, workspace) == SymlinkTarget::Outside {
                    return None;
                }
                LsEntryKind::Symlink
            } else {
                LsEntryKind::File
            };
            Some((entry.relative, kind, entry.path.to_path_buf()))
        },
    );

    let Walked {
        mut kept,
        unnameable,
        hidden,
    } = walked;
    kept.sort_by(|left, right| left.0.cmp(&right.0));
    let total = kept.len() + unnameable;
    kept.truncate(LS_ENTRY_CAP);

    // `stat` runs only on entries that survive the cap: `DirEntry::metadata` is
    // an uncached syscall per entry on Unix, and a recursive walk discards
    // nearly all of them.
    let entries = kept
        .into_iter()
        .map(|(relative, kind, path)| LsEntry {
            size: match kind {
                LsEntryKind::File => std::fs::metadata(&path).ok().map(|meta| meta.len()),
                LsEntryKind::Directory | LsEntryKind::Symlink => None,
            },
            path: relative,
            kind,
        })
        .collect();

    Listing {
        entries,
        total,
        unrepresentable: unnameable,
        hidden,
    }
}

/// One walk's result, before it is shaped into [`LsOutput`].
struct Listing {
    entries: Vec<LsEntry>,
    total: usize,
    unrepresentable: usize,
    hidden: usize,
}

/// Builds the approval-facing summary.
///
/// Both the reach and the escape hatch are named: the latter is authorized
/// through a separate `ExactTool` check, and an approver shown only "List
/// directory: src" would be granting the ignore/hidden bypass — persistently,
/// if they choose "always" — without ever seeing it.
fn listing_summary(display_path: &str, depth: usize, include_ignored: bool) -> String {
    let mut summary = format!("List directory: {display_path}");
    if depth > 1 {
        summary.push_str(&format!(" ({depth} levels deep)"));
    }
    if include_ignored {
        summary.push_str(" (including ignored and hidden entries)");
    }
    summary
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use super::{LS_ENTRY_CAP, Ls, Workspace};
    use crate::test_support::TestDir;
    use crate::{
        permission::{
            CanonicalPath, PermissionNamespace, PermissionTarget, PolicyEffect, PolicyOrigin,
            PolicySet, SessionPolicyOverlay,
        },
        tool::{PreparationContext, Tool, ToolContext, ToolOutput, execute_for_test},
    };

    /// A context carrying the entry filter one Read deny rule compiles to.
    fn denying(workspace: &Workspace, selector: &str) -> ToolContext {
        let root = CanonicalPath::from_absolute(workspace.root()).expect("absolute root");
        let mut policy = PolicySet::new(root);
        policy
            .compile_and_push(selector, PolicyEffect::Deny, PolicyOrigin::User)
            .expect("rule compiles");
        ToolContext::new().with_visibility(policy.read_visibility(&SessionPolicyOverlay::default()))
    }

    async fn call_with(tool: Ls, args: serde_json::Value, ctx: &ToolContext) -> ToolOutput {
        execute_for_test(Arc::new(tool), args, ctx)
            .await
            .expect("no harness-level error")
    }

    #[tokio::test]
    async fn denied_entries_are_withheld_from_a_listing_rooted_above_them() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join("secrets")).expect("directory should be created");
        fs::write(tmp.path().join("secrets/prod.key"), "").expect("file should be written");
        fs::create_dir_all(tmp.path().join("src")).expect("directory should be created");
        fs::write(tmp.path().join("src/lib.rs"), "").expect("file should be written");
        let workspace = tmp.workspace().await;
        let ctx = denying(&workspace, "Read(secrets/**)");

        let output = call_with(Ls::new(workspace), serde_json::json!({ "depth": 64 }), &ctx).await;

        let data = output.data.expect("data present");
        let paths = data["entries"]
            .as_array()
            .expect("entries present")
            .iter()
            .map(|entry| entry["path"].as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"src/lib.rs".to_string()));
        assert!(
            !paths.iter().any(|path| path.starts_with("secrets")),
            "{paths:?}"
        );
        // One: the denied directory is pruned rather than walked and dropped
        // entry by entry.
        assert_eq!(data["hidden_by_policy"], 1);
    }

    #[tokio::test]
    async fn a_wholly_withheld_directory_is_denied_rather_than_reported_empty() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join("secrets")).expect("directory should be created");
        fs::write(tmp.path().join("secrets/prod.key"), "").expect("file should be written");
        let workspace = tmp.workspace().await;
        let ctx = denying(&workspace, "Read(**/*.key)");

        let output = call_with(
            Ls::new(workspace),
            serde_json::json!({ "path": "secrets" }),
            &ctx,
        )
        .await;

        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "permission_denied"
        );
    }

    async fn call(tool: Ls, args: serde_json::Value) -> ToolOutput {
        execute_for_test(Arc::new(tool), args, &ToolContext::new())
            .await
            .expect("no harness-level error")
    }

    #[tokio::test]
    async fn ls_lists_one_level_with_kinds_and_sizes() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join("src/bin")).expect("directory should be created");
        fs::write(tmp.path().join("src/lib.rs"), "pub fn ok() {}").expect("file should be written");
        fs::write(tmp.path().join("src/bin/main.rs"), "").expect("file should be written");
        let tool = Ls::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "path": "src" })).await;

        assert!(output.ok);
        let data = output.data.expect("data present");
        assert_eq!(data["directory"], "src");
        assert_eq!(
            data["entries"],
            serde_json::json!([
                { "path": "src/bin", "kind": "directory" },
                { "path": "src/lib.rs", "kind": "file", "size": 14 },
            ])
        );
        assert_eq!(data["total_entries"], 2);
    }

    #[tokio::test]
    async fn ls_defaults_to_the_workspace_root() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("README.md"), "").expect("file should be written");
        let tool = Ls::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({})).await;

        assert!(output.ok);
        let data = output.data.expect("data present");
        assert_eq!(data["directory"], ".");
        assert_eq!(data["entries"][0]["path"], "README.md");
    }

    #[tokio::test]
    async fn depth_selects_how_far_the_listing_reaches() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join("a/b/c")).expect("directory should be created");
        fs::write(tmp.path().join("a/one.rs"), "").expect("file should be written");
        fs::write(tmp.path().join("a/b/two.rs"), "").expect("file should be written");
        fs::write(tmp.path().join("a/b/c/three.rs"), "").expect("file should be written");
        let tool = Ls::new(tmp.workspace().await);

        let paths = |output: ToolOutput| {
            output.data.expect("data present")["entries"]
                .as_array()
                .expect("entries is an array")
                .iter()
                .map(|entry| entry["path"].as_str().expect("path").to_string())
                .collect::<Vec<_>>()
        };

        // The default reaches one level; each step adds exactly one more. This
        // is the middle ground a boolean `recursive` could not express.
        let default = call(tool.clone(), serde_json::json!({ "path": "a" })).await;
        let two = call(tool.clone(), serde_json::json!({ "path": "a", "depth": 2 })).await;
        let three = call(tool, serde_json::json!({ "path": "a", "depth": 3 })).await;

        assert_eq!(paths(default), ["a/b", "a/one.rs"]);
        assert_eq!(paths(two), ["a/b", "a/b/c", "a/b/two.rs", "a/one.rs"]);
        assert_eq!(
            paths(three),
            ["a/b", "a/b/c", "a/b/c/three.rs", "a/b/two.rs", "a/one.rs"]
        );
    }

    #[tokio::test]
    async fn a_depth_of_zero_is_refused_rather_than_returning_nothing() {
        let tmp = TestDir::new();
        let tool = Ls::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "depth": 0 })).await;

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "invalid_arguments"
        );
    }

    #[tokio::test]
    async fn ls_at_depth_returns_workspace_relative_paths() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join("src/bin")).expect("directory should be created");
        fs::write(tmp.path().join("src/bin/main.rs"), "").expect("file should be written");
        fs::write(tmp.path().join("README.md"), "").expect("file should be written");
        let tool = Ls::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "path": "src", "depth": 64 })).await;

        assert!(output.ok);
        let data = output.data.expect("data present");
        let names = data["entries"]
            .as_array()
            .expect("entries is an array")
            .iter()
            .map(|entry| entry["path"].as_str().expect("path is a string"))
            .collect::<Vec<_>>();
        // Nested entries carry the same path `read_file` and `glob` would use,
        // and the sibling outside `src` is not reachable from this listing.
        assert_eq!(names, ["src/bin", "src/bin/main.rs"]);
    }

    #[tokio::test]
    async fn ls_hides_ignored_and_hidden_entries_by_default() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join(".gitignore"), "build/\n").expect("gitignore should be written");
        fs::create_dir_all(tmp.path().join("build")).expect("directory should be created");
        fs::write(tmp.path().join("build/out.rs"), "").expect("file should be written");
        fs::write(tmp.path().join("keep.rs"), "").expect("file should be written");
        let tool = Ls::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({})).await;

        assert!(output.ok);
        let data = output.data.expect("data present");
        // `build/` is ignored by the project and `.gitignore` itself is hidden.
        assert_eq!(
            data["entries"],
            serde_json::json!([{ "path": "keep.rs", "kind": "file", "size": 0 }])
        );
    }

    #[tokio::test]
    async fn ls_include_ignored_surfaces_ignored_and_hidden_entries() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join(".gitignore"), "build/\n").expect("gitignore should be written");
        fs::create_dir_all(tmp.path().join("build")).expect("directory should be created");
        fs::write(tmp.path().join("keep.rs"), "").expect("file should be written");
        let tool = Ls::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "include_ignored": true })).await;

        assert!(output.ok);
        let data = output.data.expect("data present");
        let names = data["entries"]
            .as_array()
            .expect("entries is an array")
            .iter()
            .map(|entry| entry["path"].as_str().expect("path is a string"))
            .collect::<Vec<_>>();
        assert_eq!(names, [".gitignore", "build", "keep.rs"]);
    }

    #[tokio::test]
    async fn ls_lists_an_explicitly_named_ignored_directory() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join(".gitignore"), "build/\n").expect("gitignore should be written");
        fs::create_dir_all(tmp.path().join("build")).expect("directory should be created");
        fs::write(tmp.path().join("build/out.rs"), "").expect("file should be written");
        let tool = Ls::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "path": "build" })).await;

        // Asking for an ignored directory by name is an explicit request, so the
        // listing is rooted there and its contents are returned.
        assert!(output.ok);
        let data = output.data.expect("data present");
        assert_eq!(data["entries"][0]["path"], "build/out.rs");
    }

    #[tokio::test]
    async fn ls_refuses_to_list_the_vcs_store() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join(".git/refs/heads"))
            .expect("directory should be created");
        fs::write(tmp.path().join(".git/HEAD"), "ref: refs/heads/main")
            .expect("file should be written");
        let workspace = tmp.workspace().await;

        // The walker never applies its `.git` filter to its own root, so naming
        // the store directly would otherwise enumerate it.
        for path in [".git", ".git/refs"] {
            let output = call(
                Ls::new(workspace.clone()),
                serde_json::json!({ "path": path }),
            )
            .await;

            assert!(!output.ok, "listing `{path}` must be refused");
            assert_eq!(
                output.error.expect("error present").kind.as_str(),
                "invalid_arguments"
            );
        }
    }

    #[tokio::test]
    async fn ls_applies_ignore_rules_declared_above_the_listed_directory() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join(".gitignore"), "*.key\nsrc/generated/\n")
            .expect("gitignore should be written");
        fs::create_dir_all(tmp.path().join("src/generated")).expect("directory should be created");
        fs::write(tmp.path().join("src/lib.rs"), "").expect("file should be written");
        fs::write(tmp.path().join("src/api.key"), "").expect("file should be written");
        fs::write(tmp.path().join("src/generated/out.rs"), "").expect("file should be written");
        let workspace = tmp.workspace().await;

        let output = call(
            Ls::new(workspace.clone()),
            serde_json::json!({ "path": "src", "depth": 64 }),
        )
        .await;

        assert!(output.ok);
        let data = output.data.expect("data present");
        // Rooting the walk at `src` must not escape the rules the project
        // declares at its root — that is what `include_ignored` is gated for.
        assert_eq!(
            data["entries"],
            serde_json::json!([{ "path": "src/lib.rs", "kind": "file", "size": 0 }])
        );

        let escaped = call(
            Ls::new(workspace),
            serde_json::json!({ "path": "src", "depth": 64, "include_ignored": true }),
        )
        .await;

        let data = escaped.data.expect("data present");
        let names = data["entries"]
            .as_array()
            .expect("entries is an array")
            .iter()
            .map(|entry| entry["path"].as_str().expect("path is a string"))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "src/api.key",
                "src/generated",
                "src/generated/out.rs",
                "src/lib.rs"
            ]
        );
    }

    #[tokio::test]
    async fn ls_always_skips_the_git_directory() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join(".git")).expect("directory should be created");
        fs::write(tmp.path().join(".git/packed.rs"), "").expect("file should be written");
        fs::write(tmp.path().join("keep.rs"), "").expect("file should be written");
        let tool = Ls::new(tmp.workspace().await);

        // Even with `include_ignored`, the VCS store must never be traversed.
        let output = call(tool, serde_json::json!({ "include_ignored": true })).await;

        assert!(output.ok);
        let data = output.data.expect("data present");
        assert_eq!(
            data["entries"],
            serde_json::json!([{ "path": "keep.rs", "kind": "file", "size": 0 }])
        );
    }

    #[tokio::test]
    async fn a_deny_rule_does_not_make_the_git_directory_listable() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join(".git")).expect("directory should be created");
        fs::write(tmp.path().join(".git/packed.rs"), "").expect("file should be written");
        fs::write(tmp.path().join("keep.rs"), "").expect("file should be written");
        let workspace = tmp.workspace().await;
        // An unrelated deny rule installs the visibility filter; the VCS skip
        // must survive it rather than be traded away for it.
        let ctx = denying(&workspace, "Read(secrets/**)");

        let output = call_with(
            Ls::new(workspace),
            serde_json::json!({ "include_ignored": true }),
            &ctx,
        )
        .await;

        assert!(output.ok);
        let data = output.data.expect("data present");
        assert_eq!(
            data["entries"],
            serde_json::json!([{ "path": "keep.rs", "kind": "file", "size": 0 }])
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ls_drops_escaping_symlinks_but_keeps_internal_ones() {
        use std::os::unix::fs::symlink;

        let tmp = TestDir::new();
        let outside = tmp
            .path()
            .parent()
            .expect("temp root has parent")
            .join(format!("kuncode-ls-outside-{}.rs", std::process::id()));
        fs::write(&outside, "").expect("outside file should be written");
        fs::write(tmp.path().join("keep.rs"), "").expect("file should be written");
        symlink(&outside, tmp.path().join("escape_link.rs")).expect("symlink should be created");
        symlink(
            tmp.path().join("keep.rs"),
            tmp.path().join("inside_link.rs"),
        )
        .expect("symlink should be created");
        let tool = Ls::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({})).await;

        let _ = fs::remove_file(outside);
        assert!(output.ok);
        let data = output.data.expect("data present");
        let entries = data["entries"].as_array().expect("entries is an array");
        // The escaping link is dropped; the internal one stays, matching the set
        // `read_file` would actually allow.
        let names = entries
            .iter()
            .map(|entry| entry["path"].as_str().expect("path is a string"))
            .collect::<Vec<_>>();
        assert_eq!(names, ["inside_link.rs", "keep.rs"]);
        assert_eq!(entries[0]["kind"], "symlink");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ls_keeps_a_dangling_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = TestDir::new();
        symlink(tmp.path().join("gone.toml"), tmp.path().join("config.toml"))
            .expect("symlink should be created");
        let tool = Ls::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({})).await;

        assert!(output.ok);
        let data = output.data.expect("data present");
        // A link that resolves nowhere cannot escape the workspace either, and
        // dropping it would answer "no such entry" to the very question `ls` is
        // used to settle.
        assert_eq!(
            data["entries"],
            serde_json::json!([{ "path": "config.toml", "kind": "symlink" }])
        );
        assert_eq!(data["total_entries"], 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ls_reports_an_unreadable_directory_instead_of_an_empty_one() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TestDir::new();
        let locked = tmp.path().join("locked");
        fs::create_dir(&locked).expect("directory should be created");
        fs::write(locked.join("secret.rs"), "").expect("file should be written");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))
            .expect("permissions should be set");
        // Running as root defeats the mode bits, and this test is about what the
        // kernel refuses, not about what the tool computes.
        let enforced = fs::read_dir(&locked).is_err();
        let tool = Ls::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "path": "locked" })).await;

        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o700));
        if !enforced {
            return;
        }
        // An empty success here is indistinguishable from a genuinely empty
        // directory, and reads as "this module has nothing in it".
        assert!(!output.ok);
        assert_eq!(output.error.expect("error present").kind.as_str(), "list");
    }

    #[tokio::test]
    async fn approval_summary_names_the_escape_hatch() {
        let tmp = TestDir::new();
        let preparation = Tool::prepare(
            Arc::new(Ls::new(tmp.workspace().await)),
            serde_json::json!({ "depth": 64, "include_ignored": true }),
            &PreparationContext::new(),
        )
        .await
        .expect("ls preparation succeeds");

        // The summary is the only free text an approver sees; granting the
        // bypass must not look like an ordinary listing.
        let summary = preparation.display().summary();
        assert!(summary.contains("64 levels deep"), "summary was: {summary}");
        assert!(
            summary.contains("ignored and hidden"),
            "summary was: {summary}"
        );
    }

    #[tokio::test]
    async fn ls_caps_entries_but_still_reports_the_total() {
        let tmp = TestDir::new();
        let total = LS_ENTRY_CAP + 3;
        for index in 0..total {
            fs::write(tmp.path().join(format!("file{index:04}.rs")), "")
                .expect("file should be written");
        }
        let tool = Ls::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({})).await;

        assert!(output.ok);
        assert!(output.truncated);
        let data = output.data.expect("data present");
        // What was left out stays visible through the total, which is how a
        // caller decides to narrow the listing.
        assert_eq!(
            data["entries"].as_array().expect("array").len(),
            LS_ENTRY_CAP
        );
        assert_eq!(data["total_entries"], total);
    }

    #[tokio::test]
    async fn ls_rejects_a_file_target() {
        let tmp = TestDir::new();
        fs::write(tmp.path().join("notes.md"), "").expect("file should be written");
        let tool = Ls::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "path": "notes.md" })).await;

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "not_a_directory"
        );
    }

    #[tokio::test]
    async fn ls_rejects_paths_that_escape_the_workspace() {
        let tmp = TestDir::new();
        let tool = Ls::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "path": ".." })).await;

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "workspace_path"
        );
    }

    #[tokio::test]
    async fn include_ignored_adds_a_separate_approval_check() {
        let tmp = TestDir::new();
        let preparation = Tool::prepare(
            Arc::new(Ls::new(tmp.workspace().await)),
            serde_json::json!({ "include_ignored": true }),
            &PreparationContext::new(),
        )
        .await
        .expect("ls preparation succeeds");

        // The listed directory plus the escape hatch.
        assert_eq!(preparation.checks().len(), 2);
        assert!(
            preparation
                .checks()
                .iter()
                .any(|check| check.target().namespace() == PermissionNamespace::ExactTool)
        );
    }

    /// The path a preparation authorizes, for rule-matching tests.
    async fn authorized_path(workspace: Workspace, args: serde_json::Value) -> String {
        let preparation = Tool::prepare(
            Arc::new(Ls::new(workspace)),
            args,
            &PreparationContext::new(),
        )
        .await
        .expect("ls preparation succeeds");

        preparation
            .checks()
            .iter()
            .find_map(|check| match check.target() {
                PermissionTarget::Read(path) => Some(path.as_str().to_string()),
                _ => None,
            })
            .expect("a read check is emitted")
    }

    #[tokio::test]
    async fn listing_authorizes_the_directory_it_walks() {
        let tmp = TestDir::new();
        fs::create_dir_all(tmp.path().join("secrets")).expect("directory should be created");
        let workspace = tmp.workspace().await;
        let root = workspace.root().to_string_lossy().to_string();

        // `deny: [Read(secrets/**)]` matches the directory itself, so naming it
        // is enough to stop the listing; entries a walk rooted higher up would
        // reach are filtered as they are produced.
        for args in [
            serde_json::json!({ "path": "secrets" }),
            serde_json::json!({ "path": "secrets", "depth": 64 }),
        ] {
            assert_eq!(
                authorized_path(workspace.clone(), args).await,
                format!("{root}/secrets")
            );
        }
        assert_eq!(
            authorized_path(workspace, serde_json::json!({})).await,
            root
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_backslash_in_a_name_is_reported_verbatim() {
        let tmp = TestDir::new();
        // Both exist, and folding `\` into a separator would report the first
        // one under the second one's name — an `edit_file` on the reported path
        // would then rewrite a file the caller never saw.
        fs::write(tmp.path().join("weird\\name.rs"), "").expect("file should be written");
        fs::create_dir_all(tmp.path().join("weird")).expect("directory should be created");
        fs::write(tmp.path().join("weird/name.rs"), "").expect("file should be written");
        let tool = Ls::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({ "depth": 64 })).await;

        let data = output.data.expect("data present");
        let paths = data["entries"]
            .as_array()
            .expect("entries present")
            .iter()
            .map(|entry| entry["path"].as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>();
        assert_eq!(paths, ["weird", "weird/name.rs", "weird\\name.rs"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn names_that_are_not_utf8_are_counted_instead_of_listed() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

        let tmp = TestDir::new();
        fs::write(tmp.path().join("readable.rs"), "").expect("file should be written");
        fs::write(tmp.path().join(OsStr::from_bytes(b"caf\xff.rs")), "")
            .expect("file should be written");
        let tool = Ls::new(tmp.workspace().await);

        let output = call(tool, serde_json::json!({})).await;

        let data = output.data.expect("data present");
        // Reporting the name lossily would hand back a path nothing can open,
        // so it is counted: an entry the caller cannot see still must not read
        // as an entry that is not there.
        assert_eq!(
            data["entries"],
            serde_json::json!([{ "path": "readable.rs", "kind": "file", "size": 0 }])
        );
        assert_eq!(data["total_entries"], 2);
        assert_eq!(data["unrepresentable_entries"], 1);
        assert!(output.truncated);
    }
}
