//! Path resolution, tree walking, and error shaping shared by the workspace
//! filesystem tools.
//!
//! Every item here has at least two tool callers (read / write / edit / glob /
//! ls). A helper used by a single tool lives beside that tool instead, so this
//! module stays the genuinely shared base.

use std::{
    ffi::OsStr,
    fs::FileType,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use ignore::{
    Match, WalkBuilder,
    gitignore::{Gitignore, GitignoreBuilder},
};
use tokio::{
    fs::{File, OpenOptions},
    io::AsyncWriteExt,
};

use crate::{
    permission::{CanonicalPath, PathVisibility},
    tool::{FileStamp, PreparedInvocationState, ToolError, ToolOutput},
    workspace::{Workspace, WorkspaceError},
};

pub(super) fn non_empty_path<D>(path: &str) -> Result<&Path, ToolOutput<D>> {
    if path.trim().is_empty() {
        Err(ToolOutput::failure(
            "invalid_arguments",
            "`path` must not be empty",
        ))
    } else {
        Ok(Path::new(path))
    }
}

pub(super) fn workspace_error<D>(err: WorkspaceError) -> ToolOutput<D> {
    ToolOutput::failure("workspace_path", err.to_string())
}

pub(super) fn io_error<D>(
    kind: &str,
    path: &Path,
    err: io::Error,
    workspace: &Workspace,
) -> ToolOutput<D> {
    ToolOutput::failure(
        kind,
        format!(
            "failed to {kind} `{}`: {err}",
            workspace.relative_display(path)
        ),
    )
}

/// Re-resolves a prepared path and reports whether the invocation still stands.
///
/// Every path tool asks the same question before acting: does this path still
/// resolve to what was authorized? A failure is not raised as an error —
/// re-preparation produces the model-safe path diagnostic against current
/// metadata, without executing the stale payload.
pub(super) async fn revalidate_path(
    workspace: &Workspace,
    path: &Path,
) -> Result<PreparedInvocationState, ToolError> {
    Ok(if workspace.revalidate_target(path).await.is_ok() {
        PreparedInvocationState::Current
    } else {
        PreparedInvocationState::Stale
    })
}

/// Why a prepared path could not be opened.
#[derive(Debug)]
pub(crate) enum OpenError {
    /// The path is a symlink. A prepared path is resolved, so it is not one —
    /// something replaced it after the invocation was authorized.
    Symlink,
    /// Anything else the filesystem reported.
    Io(io::Error),
}

/// Opens a prepared path, refusing to follow a symlink in its final component.
///
/// Preparation resolves the path — canonicalizing it, expanding even a dangling
/// link — so no component of what a tool later opens should be a symlink. The
/// gap is time: authorization happens against the path as it was, and the open
/// happens against the path as it is. A link swapped in between the two would
/// be followed, and the tool would read or write a file no check ever saw.
///
/// `O_NOFOLLOW` refuses that inside the open syscall itself, so for the final
/// component there is no window left at all. A *parent* directory swapped
/// mid-call still slips through; closing that needs per-component `openat`,
/// which is a different tool from the portable ones used here.
pub(super) async fn open_no_follow(
    path: &Path,
    options: &mut OpenOptions,
) -> Result<File, OpenError> {
    #[cfg(unix)]
    {
        options.custom_flags(libc::O_NOFOLLOW);
        options.open(path).await.map_err(|err| {
            // Linux and macOS both report the refusal as `ELOOP`. Matched on
            // the raw code because `ErrorKind::FilesystemLoop` is still
            // unstable, and a parent-component loop cannot reach here: every
            // component above the last one was resolved during preparation.
            if err.raw_os_error() == Some(libc::ELOOP) {
                OpenError::Symlink
            } else {
                OpenError::Io(err)
            }
        })
    }
    #[cfg(not(unix))]
    {
        // Without `O_NOFOLLOW` the check has to precede the open, which leaves
        // exactly the window this closes elsewhere. A missing path is not a
        // symlink, so it falls through to the open and its own error.
        if let Ok(metadata) = tokio::fs::symlink_metadata(path).await
            && metadata.file_type().is_symlink()
        {
            return Err(OpenError::Symlink);
        }
        options.open(path).await.map_err(OpenError::Io)
    }
}

/// Replaces the contents of a prepared path, refusing to follow a symlink in
/// its final component; see [`open_no_follow`] for why.
pub(crate) async fn write_no_follow(path: &Path, content: &[u8]) -> Result<(), OpenError> {
    let mut file = open_no_follow(
        path,
        OpenOptions::new().write(true).create(true).truncate(true),
    )
    .await?;
    file.write_all(content).await.map_err(OpenError::Io)?;
    // A tokio `File` buffers, and dropping one discards whatever it has not
    // issued yet, so the write is completed here rather than at drop.
    file.flush().await.map_err(OpenError::Io)
}

/// How `path` stands right now, or an empty stamp when it cannot be read.
///
/// Taken without following symlinks, matching how `write_file` stamps the file
/// it compares a recorded reading against: a baseline and the check that
/// consumes it have to describe the same filesystem object, or a link would let
/// the two disagree about which file changed.
pub(crate) async fn file_stamp(path: &Path) -> FileStamp {
    tokio::fs::symlink_metadata(path)
        .await
        .as_ref()
        .map(FileStamp::from_metadata)
        .unwrap_or_default()
}

/// Shapes an [`OpenError`] into the model-facing failure for `kind`.
pub(super) fn open_error<D>(
    kind: &str,
    path: &Path,
    err: OpenError,
    workspace: &Workspace,
) -> ToolOutput<D> {
    match err {
        OpenError::Symlink => ToolOutput::failure(
            "symlink_not_followed",
            format!(
                "`{}` is a symlink and was not followed; \
                 pass the path it points to instead",
                workspace.relative_display(path)
            ),
        ),
        OpenError::Io(err) => io_error(kind, path, err, workspace),
    }
}

/// One walked entry, in the form every tool here consumes it.
pub(super) struct WalkedEntry<'a> {
    /// Absolute path on disk.
    pub path: &'a Path,
    /// Workspace-relative, slash-separated form of [`Self::path`].
    pub relative: String,
    /// What the entry is, resolved without following symlinks.
    pub file_type: FileType,
}

/// What a walk produced, alongside what it could not report.
pub(super) struct Walked<T> {
    /// Whatever `visit` kept.
    pub kept: Vec<T>,
    /// Entries dropped for having no workspace-relative name.
    pub unnameable: usize,
    /// Entries the permission policy hid, counted so an emptied listing does
    /// not read as an empty directory.
    pub hidden: usize,
}

/// Walks below `root`, handing each nameable entry to `visit` and collecting
/// whatever `visit` keeps.
///
/// Entries denied by `visibility` never reach `visit`, and a denied directory
/// is not descended into. This is the single seam where both walking tools
/// enforce Read deny rules on their own output: authorization decides the
/// directory the walk starts from, and a walk rooted above a denied subtree is
/// a legitimate call whose results still must not name it.
///
/// Reporting [`Walked::unnameable`] and [`Walked::hidden`] is the caller's job:
/// an entry that cannot be described, or may not be shown, must not read as an
/// entry that is not there.
///
/// `max_depth` of `Some(1)` restricts the walk to the root's own children;
/// `None` walks the whole tree. The root itself is never visited, and an
/// unreadable entry is skipped rather than aborting the walk, matching
/// ripgrep's resilience. Ignore handling and the VCS-store skip come from
/// [`workspace_walk_builder`], including the ignore rules declared above a
/// `root` deeper than the workspace — so listing a subdirectory filters
/// exactly as the same paths would when seen from the workspace root.
///
/// Synchronous and thread-based; callers run it on the blocking pool.
pub(super) fn walk_entries<T>(
    workspace: &Workspace,
    root: &Path,
    max_depth: Option<usize>,
    honor_ignore_files: bool,
    visibility: &PathVisibility,
    mut visit: impl FnMut(WalkedEntry<'_>) -> Option<T>,
) -> Walked<T> {
    let hidden = Arc::new(AtomicUsize::new(0));
    let mut builder = workspace_walk_builder(
        workspace,
        root,
        honor_ignore_files,
        WithheldPaths::new(visibility, root, Arc::clone(&hidden)),
    );
    builder.max_depth(max_depth);

    let mut kept = Vec::new();
    let mut unnameable = 0;
    for result in builder.build() {
        let Ok(entry) = result else { continue };
        let path = entry.path();
        if path == root {
            continue;
        }
        // `None` here means the entry is stdin, which this walk cannot produce.
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        let Some(relative) = workspace.relative_path(path) else {
            unnameable += 1;
            continue;
        };
        if let Some(item) = visit(WalkedEntry {
            path,
            relative,
            file_type,
        }) {
            kept.push(item);
        }
    }

    let hidden = hidden.load(Ordering::Relaxed);
    if hidden > 0 {
        // The execution receipt records an authorized call; without this the
        // audit trail would never show that its output was cut down. Counted,
        // never named — the log must not restate what the rule withheld.
        tracing::info!(
            target: "kuncode::authorization",
            hidden,
            "walk output filtered by permission policy",
        );
    }

    Walked {
        kept,
        unnameable,
        hidden,
    }
}

/// Builds a walker over `walk_root`, optionally honoring the project's own
/// notion of noise.
///
/// Which paths are noise is delegated to the project rather than a hardcoded
/// name list: with `honor_ignore_files`, `.gitignore` / `.ignore` /
/// `.git/info/exclude` all apply. The user's global gitignore and ignore files
/// *above the workspace* are deliberately not consulted, so behavior is
/// reproducible and scoped to the workspace.
///
/// Hidden entries are always walked, which those rules are not asked about.
/// A leading dot marks a file as uninteresting to a human browsing a directory;
/// it says nothing about whether the file is source. `.github/workflows`,
/// `.cargo/config.toml`, and `.eslintrc` are as much a part of a project as
/// anything under `src`, and a search that cannot see them answers the wrong
/// question in silence.
///
/// Version-control stores are never traversed — see [`VCS_STORES`]. Note that
/// the walker exempts its own root from every filter, including that one, so a
/// caller that lets `walk_root` be user-supplied must reject a root inside a
/// store itself.
///
/// Permission filtering is folded in here rather than layered on by the caller:
/// [`WalkBuilder::filter_entry`] keeps only the predicate it was given last, so
/// a second call would silently retire the VCS and ancestor-ignore skips above.
/// Every reason to drop an entry has to be decided in the one closure below.
///
/// The returned walker is synchronous and thread-based; callers run it on the
/// blocking pool.
fn workspace_walk_builder(
    workspace: &Workspace,
    walk_root: &Path,
    honor_ignore_files: bool,
    withheld: WithheldPaths,
) -> WalkBuilder {
    // `WalkBuilder` reads ignore files only from the tree it walks, so a walk
    // rooted below the workspace would escape every rule the project declares
    // higher up. Supplying those rules separately keeps a subdirectory listing
    // filtered exactly like the same paths seen from the workspace root.
    let ancestors = if honor_ignore_files && walk_root != workspace.root() {
        AncestorIgnore::between(workspace.root(), walk_root)
    } else {
        AncestorIgnore::default()
    };
    let mut builder = WalkBuilder::new(walk_root);
    builder
        .hidden(false)
        .git_ignore(honor_ignore_files)
        .git_exclude(honor_ignore_files)
        .ignore(honor_ignore_files)
        .git_global(false)
        .parents(false)
        .require_git(false)
        .filter_entry(move |entry| {
            !is_vcs_store(entry.file_name())
                && !ancestors.is_ignored(
                    entry.path(),
                    entry
                        .file_type()
                        .is_some_and(|file_type| file_type.is_dir()),
                )
                && withheld.admits(entry.path())
        });
    builder
}

/// Directories holding version-control metadata rather than project content.
///
/// One name was enough while every hidden entry was skipped wholesale. Walking
/// them means each of these would otherwise be descended into, and an object
/// database is machine data that no search over a project wants back.
const VCS_STORES: [&str; 6] = [".git", ".hg", ".svn", ".bzr", ".jj", ".sl"];

fn is_vcs_store(name: &OsStr) -> bool {
    VCS_STORES.iter().any(|store| name == OsStr::new(store))
}

/// Read paths a walk must not surface, and the tally of what it dropped.
///
/// Ordered after the noise filters in the predicate above, so an entry skipped
/// as VCS or ignored noise is never also counted as withheld by policy.
struct WithheldPaths {
    visibility: PathVisibility,
    walk_root: PathBuf,
    counter: Arc<AtomicUsize>,
}

impl WithheldPaths {
    fn new(visibility: &PathVisibility, walk_root: &Path, counter: Arc<AtomicUsize>) -> Self {
        Self {
            visibility: visibility.clone(),
            walk_root: walk_root.to_path_buf(),
            counter,
        }
    }

    /// Whether the walk may surface `path`, counting it when it may not.
    fn admits(&self, path: &Path) -> bool {
        // The root's own authorization was decided before the call ran;
        // re-deciding it here would only turn a rejection into an empty listing.
        if self.visibility.is_unrestricted() || path == self.walk_root {
            return true;
        }
        // A path with no canonical form cannot be matched against a rule at all.
        // It is dropped by the caller and counted as unnameable, which says more
        // than counting it as withheld would.
        let Ok(candidate) = CanonicalPath::from_absolute(path) else {
            return true;
        };
        if self.visibility.hides(&candidate) {
            self.counter.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }
}

/// Ignore rules declared between the workspace root and a deeper walk root.
///
/// Matchers are stored shallowest-first and consulted deepest-first, so the
/// closest rule wins as it does in git.
#[derive(Default)]
struct AncestorIgnore {
    matchers: Vec<Gitignore>,
}

impl AncestorIgnore {
    /// Collects the ignore files from `workspace_root` down to, but excluding,
    /// `walk_root` — the ones the walker itself will not read.
    fn between(workspace_root: &Path, walk_root: &Path) -> Self {
        let mut matchers = Vec::new();
        let Ok(relative) = walk_root.strip_prefix(workspace_root) else {
            return Self { matchers };
        };

        let mut directory = workspace_root.to_path_buf();
        let mut components = relative.components().peekable();
        loop {
            if let Some(matcher) = ignore_files_in(&directory, directory == workspace_root) {
                matchers.push(matcher);
            }
            match components.next() {
                // The walk root's own ignore files are the walker's job.
                Some(component) if components.peek().is_some() => directory.push(component),
                _ => break,
            }
        }

        Self { matchers }
    }

    /// Whether the project ignores `path`.
    ///
    /// Only `path` itself is matched, never its parents: a walk root named
    /// explicitly by the caller has its own ignored-ness waived, while what
    /// lives inside it stays filtered.
    fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        for matcher in self.matchers.iter().rev() {
            match matcher.matched(path, is_dir) {
                Match::None => {}
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
            }
        }
        false
    }
}

/// Compiles one directory's ignore files, or `None` when it declares no rules.
fn ignore_files_in(directory: &Path, is_workspace_root: bool) -> Option<Gitignore> {
    let mut builder = GitignoreBuilder::new(directory);
    // A missing file and a glob git itself would reject look the same here;
    // both mean "no rule to apply", which is how the walker treats them too.
    builder.add(directory.join(".gitignore"));
    builder.add(directory.join(".ignore"));
    if is_workspace_root {
        builder.add(directory.join(".git").join("info").join("exclude"));
    }
    builder.build().ok().filter(|matcher| !matcher.is_empty())
}

/// Whether a count is zero, for `serde(skip_serializing_if)`.
///
/// Counters that report what a listing had to leave out are noise in the
/// common case where nothing was left out, so they stay out of the payload
/// entirely until they say something.
pub(super) fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// Where a symlink inside the workspace points.
///
/// Symlinks are listed but never followed, so a tool decides for itself what to
/// advertise. The `canonicalize` cost lands only on links, which are rare, and
/// callers are already on the blocking pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SymlinkTarget {
    /// Resolves to a path under the workspace root.
    Inside,
    /// Resolves outside the workspace root, where the file tools refuse to act.
    Outside,
    /// Resolves nowhere — dangling, a cycle, or an unreadable parent. Such a
    /// link cannot reach outside the workspace either, since it reaches nothing.
    Unresolvable,
}

pub(super) fn symlink_target(path: &Path, workspace: &Workspace) -> SymlinkTarget {
    match std::fs::canonicalize(path) {
        Ok(target) if target.starts_with(workspace.root()) => SymlinkTarget::Inside,
        Ok(_) => SymlinkTarget::Outside,
        Err(_) => SymlinkTarget::Unresolvable,
    }
}

/// Whether `path` names a VCS store or something inside one.
///
/// The walk filter drops [`VCS_STORES`] entries, but never its own root, so any
/// tool taking a caller-supplied directory has to reject one there itself.
pub(super) fn is_inside_vcs_store(workspace: &Workspace, path: &Path) -> bool {
    path.strip_prefix(workspace.root()).is_ok_and(|relative| {
        relative
            .components()
            .any(|component| is_vcs_store(component.as_os_str()))
    })
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::{OpenError, open_no_follow, workspace_error, write_no_follow};
    use crate::{test_support::TestDir, workspace::WorkspaceError};

    #[cfg(unix)]
    #[tokio::test]
    async fn opening_refuses_to_follow_a_symlink() {
        let tmp = TestDir::new();
        let target = tmp.path().join("target.txt");
        fs::write(&target, "original").expect("file should be written");
        let link = tmp.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).expect("symlink should be created");

        let opened = open_no_follow(&link, tokio::fs::OpenOptions::new().read(true)).await;

        assert!(matches!(opened, Err(OpenError::Symlink)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn writing_refuses_a_symlink_without_touching_its_target() {
        let tmp = TestDir::new();
        let target = tmp.path().join("target.txt");
        fs::write(&target, "original").expect("file should be written");
        let link = tmp.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).expect("symlink should be created");

        let written = write_no_follow(&link, b"replaced").await;

        // The property worth having: the refusal happens in the open, so the
        // file the link pointed at is never truncated.
        assert!(matches!(written, Err(OpenError::Symlink)));
        assert_eq!(
            fs::read_to_string(&target).expect("target should be readable"),
            "original"
        );
    }

    #[tokio::test]
    async fn writing_creates_and_replaces_a_regular_file() {
        let tmp = TestDir::new();
        let path = tmp.path().join("notes.txt");

        write_no_follow(&path, b"first").await.expect("write");
        write_no_follow(&path, b"two").await.expect("rewrite");

        // Truncation matters: the shorter second write must not leave a tail of
        // the first one behind.
        assert_eq!(
            fs::read_to_string(&path).expect("file should be readable"),
            "two"
        );
    }

    #[test]
    fn workspace_errors_are_model_recoverable() {
        let output: crate::tool::ToolOutput<()> =
            workspace_error(WorkspaceError::MissingFileName {
                path: PathBuf::from("."),
            });

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "workspace_path"
        );
    }
}
