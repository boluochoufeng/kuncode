//! Append-only JSONL persistence of the conversation transcript.
//!
//! Every message appended to an [`AgentSession`](crate::session::AgentSession)
//! is mirrored, one JSON line each, into a per-session log file under a
//! **global** directory (`~/.kuncode/sessions/<project-slug>/`) — deliberately
//! outside the project: the transcript is a user asset that must survive a
//! re-clone and must not leak into tars, rsyncs, or docker build contexts.
//! The mirror happens at append time, before any future compaction touches the
//! in-memory window, so the log is always the immutable full history.
//!
//! Persistence is a best-effort side channel and must never fail a turn. The
//! writer is a three-state machine — [`Pending`](State::Pending) (nothing on
//! disk yet, so an empty session leaves no trace), [`Open`](State::Open)
//! (appending), [`Poisoned`](State::Poisoned) (first error recorded, all
//! further appends are no-ops). There is no retry: the errors that reach here
//! (disk full, read-only filesystem) do not heal by themselves, and
//! re-reporting every message would be noise — the stored error is handed out
//! exactly once via [`TranscriptLog::take_error`] so the runner can surface a
//! single warning.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::{SecondsFormat, Utc};
use kuncode_core::completion::Message;
use serde::Serialize;

/// Format version stamped on every log line. The domain [`Message`] serde
/// representation doubles as the persistence schema (there is no separate DTO
/// layer), so changing that representation — or the envelope shape — **is**
/// changing the log format and must come with a bump here.
const FORMAT_VERSION: u32 = 1;

/// How many `-2`, `-3`, … suffixes to try when the timestamp+pid filename
/// already exists (the same process opening several sessions within one
/// second) before giving up with the underlying `AlreadyExists` error.
const COLLISION_CAP: u32 = 100;

/// One log line: a thin envelope around the domain [`Message`].
///
/// `v` versions the format so old logs stay identifiable as the domain types
/// evolve; `ts` (RFC3339 UTC) answers the audit question "when did this
/// happen", which the bare `Message` cannot.
#[derive(Serialize)]
struct Envelope<'a> {
    v: u32,
    ts: String,
    message: &'a Message,
}

/// Writer lifecycle. Transitions are one-way: `Pending → Open` on the first
/// successful append, anything `→ Poisoned` on the first error.
#[derive(Debug)]
enum State {
    /// Nothing on disk yet; directory and file are created lazily on the
    /// first append.
    Pending { dir: PathBuf },
    /// Appending to the open log file.
    Open { writer: BufWriter<File> },
    /// Dead: the first error was recorded and every later append is a no-op.
    /// `error` is `Some` until collected by [`TranscriptLog::take_error`]
    /// (take-and-clear, so the failure is reported exactly once).
    Poisoned { error: Option<String> },
}

/// Best-effort append-only JSONL writer for one session's transcript.
///
/// Not `Clone` on purpose: two handles appending to one file would interleave
/// two timelines into an unreadable log, so a cloned
/// [`AgentSession`](crate::session::AgentSession) drops its writer instead.
#[derive(Debug)]
pub struct TranscriptLog {
    state: State,
}

impl TranscriptLog {
    /// A log that will lazily create `dir` (and a timestamped file inside it)
    /// on the first append.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            state: State::Pending { dir },
        }
    }

    /// A log that is dead on arrival, carrying `reason` as its takeable error.
    ///
    /// For assembly-time failures (e.g. no home directory): the caller cannot
    /// build a real log but still wants the failure to surface through the
    /// same one-shot warning channel instead of silently dropping history.
    pub fn poisoned(reason: impl Into<String>) -> Self {
        Self {
            state: State::Poisoned {
                error: Some(reason.into()),
            },
        }
    }

    /// Appends one message as one JSONL line, flushed to the OS.
    ///
    /// Never fails the caller: the first error (directory creation, open,
    /// serialization, write, flush) poisons the log and this becomes a no-op.
    pub(crate) fn append(&mut self, message: &Message) {
        // Park a `Poisoned { error: None }` so the writer can be moved out;
        // if anything in between ever unwound, the log would be left dead —
        // the safe side for a best-effort channel.
        let state = std::mem::replace(&mut self.state, State::Poisoned { error: None });
        self.state = match state {
            State::Poisoned { error } => State::Poisoned { error },
            State::Pending { dir } => match open_log(&dir) {
                Ok(writer) => write_or_poison(writer, message),
                Err(error) => State::Poisoned {
                    error: Some(error.to_string()),
                },
            },
            State::Open { writer } => write_or_poison(writer, message),
        };
    }

    /// Hands out the recorded failure once (take-and-clear), `None` otherwise.
    /// The log stays poisoned either way; only the *report* is consumed.
    ///
    /// The message is prefixed here — the single exit point — so whatever
    /// renders it (a raw OS error like "No space left on device", or an
    /// assembly-time reason) always says *what* degraded, not just why.
    pub(crate) fn take_error(&mut self) -> Option<String> {
        match &mut self.state {
            State::Poisoned { error } => error
                .take()
                .map(|e| format!("transcript persistence failed: {e}")),
            _ => None,
        }
    }
}

/// Derives the per-project bucket name under the global sessions directory
/// from the workspace root: every character outside `[A-Za-z0-9._]` becomes
/// `-`, so `/home/x/proj` → `-home-x-proj`. An allowlist rather than a
/// separator swap because the root is canonicalized upstream — on Windows
/// that yields verbatim paths (`\\?\C:\…`) whose `:` and `?` are illegal in
/// NTFS file names and would poison persistence on the very first append.
///
/// Known limitation: the mapping is not injective (`/a/b-c` and `/a/b/c`
/// share a slug), so two projects can share a bucket. Accepted: per-file
/// names still never collide (`create_new`), an escaping scheme would orphan
/// every existing bucket, and a future resume stage can disambiguate by
/// reading the logs themselves.
///
/// Lives here rather than in the CLI because a future resume stage must
/// compute the same slug to find the logs again.
pub fn project_slug(root: &Path) -> String {
    root.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Writes one line and flushes, keeping the writer on success and converting
/// the first failure into the poisoned state.
fn write_or_poison(mut writer: BufWriter<File>, message: &Message) -> State {
    match write_line(&mut writer, message) {
        Ok(()) => State::Open { writer },
        Err(error) => State::Poisoned { error: Some(error) },
    }
}

fn write_line(writer: &mut BufWriter<File>, message: &Message) -> Result<(), String> {
    let envelope = Envelope {
        v: FORMAT_VERSION,
        ts: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        message,
    };
    let line = serde_json::to_string(&envelope).map_err(|e| e.to_string())?;
    writeln!(writer, "{line}").map_err(|e| e.to_string())?;
    // Flush to the OS after every message so a process crash loses at most
    // the line being written. Deliberately no fsync: power loss may drop the
    // tail line, an accepted cost for a best-effort audit log.
    writer.flush().map_err(|e| e.to_string())
}

/// Creates the session directory and opens a fresh log file named
/// `<UTC-timestamp>-<pid>.jsonl`. The timestamp is taken here — at the first
/// append — not at construction, so a session that never says anything never
/// claims a name.
fn open_log(dir: &Path) -> std::io::Result<BufWriter<File>> {
    fs::create_dir_all(dir)?;
    let base = format!(
        "{}-{}",
        Utc::now().format("%Y%m%dT%H%M%SZ"),
        std::process::id()
    );
    open_with_base(dir, &base)
}

/// Opens `<base>.jsonl` with `create_new` (two writers must never share a
/// file), appending a `-2`, `-3`, … counter while the name is taken.
fn open_with_base(dir: &Path, base: &str) -> std::io::Result<BufWriter<File>> {
    let mut attempt = 1u32;
    loop {
        let name = if attempt == 1 {
            format!("{base}.jsonl")
        } else {
            format!("{base}-{attempt}.jsonl")
        };
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(name))
        {
            Ok(file) => return Ok(BufWriter::new(file)),
            Err(err)
                if err.kind() == std::io::ErrorKind::AlreadyExists && attempt < COLLISION_CAP =>
            {
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Scaffolding shared with the `session` / `runner` transcript tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Unique, self-cleaning temp directory; mirrors the filesystem tools'
    /// test scaffolding (pid + timestamp across runs, a process-wide counter
    /// across parallel tests within one run).
    pub(crate) struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        pub(crate) fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after unix epoch")
                .as_nanos();
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "kuncode-transcript-test-{}-{stamp}-{seq}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self { path }
        }

        pub(crate) fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// All lines across every `.jsonl` file in `dir` (empty when the
    /// directory does not exist yet — the lazy-init case).
    pub(crate) fn log_lines(dir: &Path) -> Vec<String> {
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                let content = fs::read_to_string(&path).expect("log file should be readable");
                lines.extend(content.lines().map(str::to_string));
            }
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{TestDir, log_lines};
    use super::*;
    use kuncode_core::completion::{AssistantContent, Message};
    use kuncode_core::non_empty_vec::NonEmptyVec;

    /// The three message shapes the runner appends (user text, assistant,
    /// tool result) each become exactly one line.
    #[test]
    fn appends_one_envelope_line_per_message() {
        let root = TestDir::new();
        let dir = root.path().join("bucket");
        let mut log = TranscriptLog::new(dir.clone());

        log.append(&Message::user("fix the bug"));
        log.append(&Message::assistant("looking"));
        log.append(&Message::tool_result("call_1", "exit 0"));

        assert_eq!(log_lines(&dir).len(), 3);
        assert!(log.take_error().is_none());
    }

    /// Every line is `{v, ts, message}` and the `message` field deserializes
    /// back to the original — the guard for the domain serde representation
    /// doubling as the persistence schema.
    #[test]
    fn envelope_roundtrip() {
        let root = TestDir::new();
        let dir = root.path().join("bucket");
        let mut log = TranscriptLog::new(dir.clone());

        let originals = [
            Message::user("fix the bug"),
            Message::assistant("looking"),
            Message::tool_result("call_1", "exit 0"),
            // The riskiest shape: `AssistantContent` is untagged, so its
            // variants must stay distinguishable by field shape alone.
            Message::Assistant {
                id: None,
                content: NonEmptyVec::from_first_rest(
                    AssistantContent::text("running it"),
                    vec![
                        AssistantContent::reasoning("the user wants ls"),
                        AssistantContent::tool_call(
                            "call_1",
                            "bash",
                            serde_json::json!({ "cmd": "ls" }),
                        ),
                    ],
                ),
            },
        ];
        for message in &originals {
            log.append(message);
        }

        let lines = log_lines(&dir);
        assert_eq!(lines.len(), originals.len());
        for (line, original) in lines.iter().zip(&originals) {
            let value: serde_json::Value =
                serde_json::from_str(line).expect("line should be valid JSON");
            assert_eq!(value["v"], 1);
            let ts = value["ts"].as_str().expect("ts should be a string");
            assert!(ts.ends_with('Z'), "ts should be UTC RFC3339: {ts}");
            let message: Message = serde_json::from_value(value["message"].clone())
                .expect("message should deserialize");
            assert_eq!(&message, original);
        }
    }

    /// Directory and file appear only once something is appended, so an empty
    /// session leaves no disk trace.
    #[test]
    fn lazy_creates_session_dir_on_first_append() {
        let root = TestDir::new();
        let dir = root.path().join("sessions").join("-home-x-proj");
        let mut log = TranscriptLog::new(dir.clone());
        assert!(!dir.exists());

        log.append(&Message::user("hello"));

        assert!(dir.exists());
        assert_eq!(log_lines(&dir).len(), 1);
    }

    #[test]
    fn project_slug_replaces_separators() {
        assert_eq!(
            project_slug(Path::new("/home/x/proj")),
            "-home-x-proj".to_string()
        );
    }

    /// Windows canonicalization produces verbatim roots; their `:` and `?`
    /// must not survive into the directory name (illegal on NTFS).
    #[test]
    fn project_slug_sanitizes_non_filename_characters() {
        assert_eq!(
            project_slug(Path::new(r"\\?\C:\work\proj")),
            "----C--work-proj".to_string()
        );
    }

    /// A taken base name falls through to the `-2` counter suffix instead of
    /// failing or appending to the existing file.
    #[test]
    fn filename_collision_appends_counter() {
        let root = TestDir::new();
        std::fs::write(root.path().join("base.jsonl"), "occupied\n")
            .expect("existing file should be written");

        let mut writer = open_with_base(root.path(), "base").expect("collision should retry");
        writeln!(writer, "fresh").expect("write should succeed");
        writer.flush().expect("flush should succeed");

        assert_eq!(
            std::fs::read_to_string(root.path().join("base.jsonl")).expect("read"),
            "occupied\n",
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("base-2.jsonl")).expect("read"),
            "fresh\n",
        );
    }

    /// First failure poisons: the error is takeable exactly once, later
    /// appends are no-ops and do not resurrect it.
    #[test]
    fn poisoned_after_first_failure_stops_writing() {
        let root = TestDir::new();
        // A plain file where a directory component is expected makes
        // `create_dir_all` fail deterministically on every platform.
        let blocker = root.path().join("blocker");
        std::fs::write(&blocker, "not a directory").expect("blocker file should be written");
        let mut log = TranscriptLog::new(blocker.join("sub"));

        log.append(&Message::user("hello"));

        let error = log.take_error().expect("first failure should be takeable");
        assert!(
            error.starts_with("transcript persistence failed: "),
            "report must say what degraded, got: {error}"
        );
        assert!(log.take_error().is_none(), "error reports exactly once");
        log.append(&Message::user("again"));
        assert!(log.take_error().is_none(), "no-op appends record nothing");
    }
}
