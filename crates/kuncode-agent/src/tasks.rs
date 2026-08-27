//! Cross-session task store: a durable dependency graph of work items.
//!
//! Each task is one `{root}/{id}.json` document under the per-project tasks
//! root, outside the workspace. Where the todo plan manages attention inside
//! one session (in memory, overwritten wholesale), this store coordinates
//! work across sessions: tasks survive restarts, block on one another, and
//! carry an owner once claimed.
//!
//! Concurrency scope: id allocation is race-free through exclusive file
//! creation, but claim/complete/update are plain read-modify-write — two
//! processes racing the same task can lose an update. Atomic claims are the
//! multi-agent coordination work this store is groundwork for.
//!
//! The id grammar (`task_` plus fixed-width hex — no separators, no dots) is
//! also the path-confinement boundary: every id a tool accepts resolves
//! lexically to a file directly under the root.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use crate::session_store::project_slug;
use crate::tool::filesystem::write_no_follow;

/// Prefix of every task id.
pub const TASK_ID_PREFIX: &str = "task_";

/// Hex characters following the prefix.
pub const TASK_ID_HEX_CHARS: usize = 8;

/// Lifecycle of one task: claim moves pending to in-progress, complete moves
/// in-progress to completed. There is no way back.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

impl TaskStatus {
    /// Wire form, for diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

/// One durable task, mirroring the on-disk JSON document.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Task {
    /// `task_` plus [`TASK_ID_HEX_CHARS`] lowercase hex characters; also the
    /// file stem.
    pub id: String,
    /// One-line name of the work.
    pub subject: String,
    /// Free-form details; empty when none were given.
    #[serde(default)]
    pub description: String,
    pub status: TaskStatus,
    /// Set on claim; kept after completion as a record of who did the work.
    #[serde(default)]
    pub owner: Option<String>,
    /// Prerequisite task ids; camelCase on disk per the established document
    /// schema, unlike the other fields whose snake_case names match as-is.
    #[serde(default, rename = "blockedBy")]
    pub blocked_by: Vec<String>,
}

impl Task {
    /// A fresh pending, unowned, unblocked task.
    pub fn new(id: String, subject: String, description: String) -> Self {
        Self {
            id,
            subject,
            description,
            status: TaskStatus::Pending,
            owner: None,
            blocked_by: Vec::new(),
        }
    }
}

/// Slim projection for listings and unlocked reports: everything but the
/// description, which can dominate the payload and is one `get_task` away.
#[derive(Clone, Debug, Serialize)]
pub struct TaskSummary {
    pub id: String,
    pub subject: String,
    pub status: TaskStatus,
    pub owner: Option<String>,
    #[serde(rename = "blockedBy")]
    pub blocked_by: Vec<String>,
}

impl From<&Task> for TaskSummary {
    fn from(task: &Task) -> Self {
        Self {
            id: task.id.clone(),
            subject: task.subject.clone(),
            status: task.status,
            owner: task.owner.clone(),
            blocked_by: task.blocked_by.clone(),
        }
    }
}

/// Store failure; tools map the variants onto model-facing error kinds.
#[derive(Debug, Error)]
pub enum TaskStoreError {
    #[error(
        "task id must be `{TASK_ID_PREFIX}` followed by {TASK_ID_HEX_CHARS} lowercase hex \
         characters, e.g. `task_a1b2c3d4`"
    )]
    InvalidId,
    #[error("no task with id `{0}`")]
    NotFound(String),
    /// Exclusive creation saw an existing file; practically impossible under
    /// the digest id scheme, so retrying the whole call is the fix.
    #[error("generated task id `{0}` already exists; retry the call")]
    IdCollision(String),
    #[error("task file for `{id}` is not a valid task document: {reason}")]
    Corrupt { id: String, reason: String },
    #[error("task store I/O failure at `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Returns the per-project tasks root: `{home}/.kuncode/tasks/{project}`.
///
/// Keyed by [`project_slug`] so task graphs are isolated per project while
/// the files stay outside the workspace tree, like memories and sessions.
pub fn tasks_root(home: &Path, project_root: &Path) -> PathBuf {
    home.join(".kuncode")
        .join("tasks")
        .join(project_slug(project_root))
}

/// Validates an id against the grammar that confines it to the store root.
///
/// # Errors
///
/// Returns [`TaskStoreError::InvalidId`] when the shape is wrong.
pub fn validate_task_id(id: &str) -> Result<String, TaskStoreError> {
    let Some(hex) = id.strip_prefix(TASK_ID_PREFIX) else {
        return Err(TaskStoreError::InvalidId);
    };
    if hex.len() != TASK_ID_HEX_CHARS
        || !hex
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    {
        return Err(TaskStoreError::InvalidId);
    }
    Ok(id.to_string())
}

/// Domain separator for task-id digests, mirroring the crate's digest habit.
const TASK_ID_DOMAIN: &[u8] = b"kuncode.task-id.v1\0";

/// Process-wide sequence making concurrent in-process ids distinct.
static NEXT_TASK_SEQ: AtomicU64 = AtomicU64::new(0);

/// Allocates a fresh id: `task_` plus the first 4 digest bytes as hex.
///
/// The digest input — timestamp, pid, process-wide sequence — is unique per
/// call (the same sources the session store draws ids from), so collisions
/// require a 32-bit digest coincidence; [`TaskStore::create_exclusive`]
/// still refuses to overwrite if one ever happens. No randomness dependency,
/// which this workspace deliberately avoids.
pub(crate) fn generate_task_id() -> String {
    let seq = NEXT_TASK_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Sha256::new();
    hasher.update(TASK_ID_DOMAIN);
    hasher.update(chrono::Utc::now().timestamp_micros().to_be_bytes());
    hasher.update(std::process::id().to_be_bytes());
    hasher.update(seq.to_be_bytes());
    let digest = hasher.finalize();
    format!(
        "{TASK_ID_PREFIX}{:08x}",
        u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])
    )
}

/// Ids currently stored under `root`, sorted.
///
/// Read live so preparation-time diagnostics include tasks created moments
/// ago; a missing root is an empty store.
pub(crate) fn known_task_ids(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| path.file_stem()?.to_str().map(str::to_string))
        .filter(|stem| validate_task_id(stem).is_ok())
        .collect();
    ids.sort();
    ids
}

/// Whether appending `added` to `task_id`'s blockers would close a cycle.
///
/// Pure over an id → blockers adjacency view so tests can drive topologies
/// directly. A dangling id simply has no outgoing edges. Iterative DFS with a
/// visited set keeps it linear and panic-free on any input.
pub(crate) fn would_cycle(
    adjacency: &BTreeMap<String, Vec<String>>,
    task_id: &str,
    added: &[String],
) -> bool {
    // A cycle through the new edges exists iff task_id is reachable from any
    // added dependency by following existing blockedBy edges.
    let mut stack: Vec<&str> = added.iter().map(String::as_str).collect();
    let mut visited: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    while let Some(current) = stack.pop() {
        if current == task_id {
            return true;
        }
        if !visited.insert(current) {
            continue;
        }
        if let Some(blockers) = adjacency.get(current) {
            stack.extend(blockers.iter().map(String::as_str));
        }
    }
    false
}

/// Whether every blocker of `task` is present in `by_id` and completed.
///
/// A missing referent counts as not completed — the shared verdict for
/// claiming, the unlocked scan, and the startup counts, so the three agree.
pub(crate) fn deps_satisfied(task: &Task, by_id: &BTreeMap<String, Task>) -> bool {
    task.blocked_by.iter().all(|dep| {
        by_id
            .get(dep)
            .is_some_and(|blocker| blocker.status == TaskStatus::Completed)
    })
}

/// The task store rooted at one per-project directory.
#[derive(Clone, Debug)]
pub struct TaskStore {
    root: PathBuf,
}

impl TaskStore {
    /// Creates a store over the root from [`tasks_root`].
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The directory holding the task documents.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file an id denotes: `{root}/{id}.json`, purely lexical.
    pub fn task_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    /// Reads and parses one task.
    ///
    /// # Errors
    ///
    /// [`TaskStoreError::NotFound`] when no document exists,
    /// [`TaskStoreError::Corrupt`] when it does not parse.
    pub async fn load(&self, id: &str) -> Result<Task, TaskStoreError> {
        let path = self.task_path(id);
        let raw = match tokio::fs::read(&path).await {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(TaskStoreError::NotFound(id.to_string()));
            }
            Err(error) => {
                return Err(TaskStoreError::Io {
                    path: path.display().to_string(),
                    source: error,
                });
            }
        };
        serde_json::from_slice(&raw).map_err(|error| TaskStoreError::Corrupt {
            id: id.to_string(),
            reason: error.to_string(),
        })
    }

    /// Overwrites an existing task document.
    ///
    /// # Errors
    ///
    /// Returns [`TaskStoreError::Io`] when the write fails; a symlinked final
    /// component is refused inside the open itself.
    pub async fn save(&self, task: &Task) -> Result<(), TaskStoreError> {
        let path = self.task_path(&task.id);
        let body = render_document(task);
        write_no_follow(&path, &body)
            .await
            .map_err(|error| TaskStoreError::Io {
                path: path.display().to_string(),
                source: match error {
                    crate::tool::filesystem::OpenError::Symlink => std::io::Error::other(
                        "final path component is a symlink and was not followed",
                    ),
                    crate::tool::filesystem::OpenError::Io(io) => io,
                },
            })
    }

    /// Persists a brand-new task, refusing to overwrite an existing id.
    ///
    /// The workspace's first exclusive creation: `create_new` maps to
    /// `O_CREAT | O_EXCL` on unix and never follows a symlink, which is what
    /// makes id allocation race-free across processes without a lock file.
    /// The root is created lazily here — preparation stays side-effect-free,
    /// and most sessions never create a task.
    ///
    /// # Errors
    ///
    /// [`TaskStoreError::IdCollision`] when the id already exists, otherwise
    /// [`TaskStoreError::Io`].
    pub async fn create_exclusive(&self, task: &Task) -> Result<(), TaskStoreError> {
        let io_error = |path: &Path, source: std::io::Error| TaskStoreError::Io {
            path: path.display().to_string(),
            source,
        };
        tokio::fs::create_dir_all(&self.root)
            .await
            .map_err(|error| io_error(&self.root, error))?;
        let path = self.task_path(&task.id);
        let mut file = match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(TaskStoreError::IdCollision(task.id.clone()));
            }
            Err(error) => return Err(io_error(&path, error)),
        };
        let body = render_document(task);
        file.write_all(&body)
            .await
            .map_err(|error| io_error(&path, error))?;
        // A tokio `File` buffers, and dropping one discards whatever it has
        // not issued yet, so the write is completed here rather than at drop.
        file.flush().await.map_err(|error| io_error(&path, error))
    }

    /// All parseable tasks sorted by id.
    ///
    /// A missing root is an empty store; an unparsable file is skipped with a
    /// log line rather than failing the listing, so one corrupt document
    /// cannot hide the rest of the graph.
    ///
    /// # Errors
    ///
    /// Returns [`TaskStoreError::Io`] only when the root exists but cannot be
    /// read.
    pub async fn list(&self) -> Result<Vec<Task>, TaskStoreError> {
        let mut ids = Vec::new();
        let mut entries = match tokio::fs::read_dir(&self.root).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(TaskStoreError::Io {
                    path: self.root.display().to_string(),
                    source: error,
                });
            }
        };
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| TaskStoreError::Io {
                path: self.root.display().to_string(),
                source: error,
            })?
        {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if validate_task_id(stem).is_ok() {
                ids.push(stem.to_string());
            }
        }
        ids.sort();
        let mut tasks = Vec::with_capacity(ids.len());
        for id in ids {
            match self.load(&id).await {
                Ok(task) => tasks.push(task),
                Err(TaskStoreError::Corrupt { id, reason }) => {
                    tracing::warn!(
                        target: "kuncode::tasks",
                        id = %id,
                        reason = %reason,
                        "corrupt task document skipped",
                    );
                }
                // A file deleted mid-listing is simply no longer stored.
                Err(TaskStoreError::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(tasks)
    }
}

/// Serializes one task document: pretty JSON plus a trailing newline, so the
/// files read well under version control and text tools.
fn render_document(task: &Task) -> Vec<u8> {
    let mut body = serde_json::to_vec_pretty(task).unwrap_or_else(|_| {
        // Task holds only strings, an enum, and vectors thereof; serde_json
        // cannot fail on it.
        unreachable!("a task document always serializes")
    });
    body.push(b'\n');
    body
}

/// Startup snapshot for the system-prompt pointer line.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TaskStoreCounts {
    /// Pending plus in-progress tasks.
    pub open: usize,
    /// Pending tasks whose every blocker is completed.
    pub claimable: usize,
}

/// Scans the store once, synchronously, for the startup prompt line.
///
/// A missing or unreadable root and corrupt documents all degrade to "not
/// counted", mirroring how the other startup catalogs treat bad input.
pub fn scan_open_counts(root: &Path) -> TaskStoreCounts {
    let Ok(entries) = std::fs::read_dir(root) else {
        return TaskStoreCounts::default();
    };
    let mut by_id: BTreeMap<String, Task> = BTreeMap::new();
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if validate_task_id(stem).is_err() {
            continue;
        }
        let Ok(raw) = std::fs::read(&path) else {
            continue;
        };
        let Ok(task) = serde_json::from_slice::<Task>(&raw) else {
            continue;
        };
        by_id.insert(stem.to_string(), task);
    }
    let mut counts = TaskStoreCounts::default();
    for task in by_id.values() {
        match task.status {
            TaskStatus::Pending => {
                counts.open += 1;
                if deps_satisfied(task, &by_id) {
                    counts.claimable += 1;
                }
            }
            TaskStatus::InProgress => counts.open += 1,
            TaskStatus::Completed => {}
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;
    use std::collections::BTreeSet;

    fn task(id: &str, status: TaskStatus, blocked_by: &[&str]) -> Task {
        Task {
            id: id.to_string(),
            subject: format!("subject of {id}"),
            description: String::new(),
            status,
            owner: None,
            blocked_by: blocked_by.iter().map(|dep| dep.to_string()).collect(),
        }
    }

    #[test]
    fn generated_ids_match_the_grammar_and_never_repeat() {
        let mut seen = BTreeSet::new();
        for _ in 0..1000 {
            let id = generate_task_id();
            assert!(validate_task_id(&id).is_ok(), "{id}");
            assert!(seen.insert(id.clone()), "duplicate id {id}");
        }
    }

    #[test]
    fn the_id_grammar_admits_nothing_that_could_leave_the_root() {
        assert!(validate_task_id("task_a1b2c3d4").is_ok());
        assert!(validate_task_id("task_00000000").is_ok());
        for invalid in [
            "",
            "task_",
            "task_a1b2c3d",   // too short
            "task_a1b2c3d4e", // too long
            "task_A1B2C3D4",  // uppercase
            "task_a1b2c3dg",  // non-hex
            "job_a1b2c3d4",   // wrong prefix
            "task_../../etc", // traversal shape
        ] {
            assert!(
                matches!(validate_task_id(invalid), Err(TaskStoreError::InvalidId)),
                "{invalid} should be rejected",
            );
        }
    }

    #[test]
    fn documents_round_trip_and_use_the_camel_case_blocked_by_key() {
        let mut original = task("task_aaaaaaaa", TaskStatus::InProgress, &["task_bbbbbbbb"]);
        original.owner = Some("agent".to_string());
        original.description = "details".to_string();

        let json = serde_json::to_string(&original).expect("serializes");
        assert!(json.contains("\"blockedBy\""), "{json}");
        assert!(json.contains("\"in_progress\""), "{json}");
        let parsed: Task = serde_json::from_str(&json).expect("parses");
        assert_eq!(parsed, original);

        // Sparse documents (older or hand-written) still parse via defaults.
        let sparse: Task = serde_json::from_str(
            r#"{ "id": "task_cccccccc", "subject": "s", "status": "pending" }"#,
        )
        .expect("sparse document parses");
        assert_eq!(sparse.description, "");
        assert_eq!(sparse.owner, None);
        assert!(sparse.blocked_by.is_empty());
    }

    #[tokio::test]
    async fn exclusive_creation_creates_once_and_reports_collisions() {
        let tmp = TestDir::new();
        let store = TaskStore::new(tmp.path().join("tasks"));
        let subject = task("task_aaaaaaaa", TaskStatus::Pending, &[]);

        // The root did not exist; creation is lazy.
        store
            .create_exclusive(&subject)
            .await
            .expect("first creation succeeds");
        assert_eq!(
            store.load("task_aaaaaaaa").await.expect("loads").subject,
            "subject of task_aaaaaaaa"
        );

        let collision = store.create_exclusive(&subject).await;
        assert!(matches!(collision, Err(TaskStoreError::IdCollision(id)) if id == "task_aaaaaaaa"));
    }

    #[test]
    fn cycle_detection_covers_the_usual_topologies() {
        let adjacency: BTreeMap<String, Vec<String>> = [
            ("a", vec!["b"]),
            ("b", vec!["c"]),
            ("c", vec![]),
            ("d", vec!["b", "c"]),
        ]
        .into_iter()
        .map(|(id, deps)| {
            (
                id.to_string(),
                deps.into_iter().map(str::to_string).collect(),
            )
        })
        .collect();

        // Self-loop.
        assert!(would_cycle(&adjacency, "a", &["a".to_string()]));
        // Direct back-edge: c -> a while a -> b -> c already holds.
        assert!(would_cycle(&adjacency, "c", &["a".to_string()]));
        // Transitive back-edge through two hops.
        assert!(would_cycle(&adjacency, "c", &["d".to_string()]));
        // Diamond (a->b->c, a->c) is acyclic — no false positive.
        assert!(!would_cycle(&adjacency, "a", &["c".to_string()]));
        // Dangling referent has no outgoing edges and cannot cycle.
        assert!(!would_cycle(&adjacency, "a", &["zzz".to_string()]));
    }

    #[tokio::test]
    async fn listing_sorts_by_id_and_survives_bad_files() {
        let tmp = TestDir::new();
        let root = tmp.path().join("tasks");
        let store = TaskStore::new(root.clone());
        store
            .create_exclusive(&task("task_bbbbbbbb", TaskStatus::Pending, &[]))
            .await
            .expect("creates");
        store
            .create_exclusive(&task("task_aaaaaaaa", TaskStatus::Completed, &[]))
            .await
            .expect("creates");
        std::fs::write(root.join("task_cccccccc.json"), "not json").expect("write");
        std::fs::write(root.join("README.txt"), "not a task").expect("write");
        std::fs::write(root.join("BadStem.json"), "{}").expect("write");

        let tasks = store.list().await.expect("lists");

        let ids: Vec<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
        assert_eq!(ids, ["task_aaaaaaaa", "task_bbbbbbbb"]);

        let empty = TaskStore::new(tmp.path().join("missing"));
        assert!(empty.list().await.expect("missing root lists").is_empty());
    }

    #[tokio::test]
    async fn counts_follow_the_claimable_verdict() {
        let tmp = TestDir::new();
        let root = tmp.path().join("tasks");
        let store = TaskStore::new(root.clone());
        // a completed, b pending on a (claimable), c pending on missing dep,
        // d in progress, e pending unblocked (claimable).
        store
            .create_exclusive(&task("task_aaaaaaaa", TaskStatus::Completed, &[]))
            .await
            .expect("creates");
        store
            .create_exclusive(&task(
                "task_bbbbbbbb",
                TaskStatus::Pending,
                &["task_aaaaaaaa"],
            ))
            .await
            .expect("creates");
        store
            .create_exclusive(&task(
                "task_cccccccc",
                TaskStatus::Pending,
                &["task_ffffffff"],
            ))
            .await
            .expect("creates");
        store
            .create_exclusive(&task("task_dddddddd", TaskStatus::InProgress, &[]))
            .await
            .expect("creates");
        store
            .create_exclusive(&task("task_eeeeeeee", TaskStatus::Pending, &[]))
            .await
            .expect("creates");

        let counts = scan_open_counts(&root);

        assert_eq!(counts.open, 4);
        assert_eq!(counts.claimable, 2);
        assert_eq!(
            scan_open_counts(&tmp.path().join("missing")),
            TaskStoreCounts::default()
        );
    }
}
