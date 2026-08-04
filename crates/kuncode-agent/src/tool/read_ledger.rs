//! Records which files a session has read, so a whole-file write cannot
//! silently discard content nobody looked at.
//!
//! `write_file` truncates before writing: whatever the caller left out is gone.
//! That is safe when the caller has just read the file and is deciding what to
//! keep, and unrecoverable when it is writing from a guess. Only the session
//! knows which of the two happened, which is why this lives beside the plan on
//! [`ToolContext`](crate::tool::ToolContext) rather than inside either tool.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::SystemTime,
};

/// What the ledger knows about a file that is about to be overwritten.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadState {
    /// Never read in this session. Its current contents are unknown here.
    Never,
    /// Read, but not all of it — a page was requested, or a long line was cut
    /// short. The unseen part would still be overwritten blind.
    Partial,
    /// Read in full, but modified on disk afterwards, so what was read is no
    /// longer what is there.
    Stale,
    /// Read in full and unchanged since.
    Current,
}

/// One file's reading, as of the moment it was read.
#[derive(Clone, Copy, Debug)]
struct Entry {
    /// Modification time observed *before* the contents were read, so a write
    /// that lands between the two is caught rather than absorbed.
    modified: Option<SystemTime>,
    complete: bool,
}

/// Shared record of files read during a session.
///
/// Cloning shares the underlying map, mirroring
/// [`TodoHandle`](crate::todo::TodoHandle): the runner keeps one clone on the
/// session and hands another to tools through the
/// [`ToolContext`](crate::tool::ToolContext). [`Default`] yields a standalone
/// ledger attached to no session, so tests and non-interactive callers get a
/// usable target that simply records nothing anyone else will read.
#[derive(Clone, Debug, Default)]
pub struct ReadLedger(Arc<Mutex<HashMap<PathBuf, Entry>>>);

impl ReadLedger {
    /// Recovers the guard even if a previous holder panicked, for the reason
    /// [`TodoHandle::lock`](crate::todo::TodoHandle) does: the critical sections
    /// are trivial, and a poison error is not something a caller could act on.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Entry>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Notes that `path` was read. `complete` is false when only part of the
    /// file was returned, which is the difference between [`ReadState::Current`]
    /// and [`ReadState::Partial`] later.
    pub fn record_read(&self, path: &Path, modified: Option<SystemTime>, complete: bool) {
        self.lock()
            .insert(path.to_path_buf(), Entry { modified, complete });
    }

    /// Notes that `path` was written with contents the caller supplied whole.
    ///
    /// A writer knows what it just wrote as surely as a reader knows what it
    /// read, so this counts as a complete reading. Without it, writing the same
    /// file twice in one session would be refused the second time.
    pub fn record_write(&self, path: &Path, modified: Option<SystemTime>) {
        self.lock().insert(
            path.to_path_buf(),
            Entry {
                modified,
                complete: true,
            },
        );
    }

    /// What is known about `path`, given its modification time right now.
    ///
    /// `modified` of `None` on either side is treated as unchanged: a
    /// filesystem that does not report modification times cannot support this
    /// check, and refusing every write there would be worse than not checking.
    pub fn state(&self, path: &Path, modified: Option<SystemTime>) -> ReadState {
        let Some(entry) = self.lock().get(path).copied() else {
            return ReadState::Never;
        };
        if !entry.complete {
            return ReadState::Partial;
        }
        match (entry.modified, modified) {
            (Some(seen), Some(now)) if now > seen => ReadState::Stale,
            _ => ReadState::Current,
        }
    }

    /// An isolated ledger starting as a copy of this one's records.
    ///
    /// Unlike [`Clone`], which shares the `Arc`, later writes to either are
    /// invisible to the other. Used by
    /// [`AgentSession`](crate::session::AgentSession)'s manual `Clone` for the
    /// same reason [`TodoHandle::deep_clone`](crate::todo::TodoHandle) is: a
    /// cloned session is a separate timeline, and what one of them read says
    /// nothing about what the other may overwrite.
    pub fn deep_clone(&self) -> Self {
        Self(Arc::new(Mutex::new(self.lock().clone())))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(secs: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }

    #[test]
    fn an_unread_file_is_never() {
        let ledger = ReadLedger::default();
        assert_eq!(ledger.state(Path::new("/a"), at(1)), ReadState::Never);
    }

    #[test]
    fn a_full_read_of_an_unchanged_file_is_current() {
        let ledger = ReadLedger::default();
        ledger.record_read(Path::new("/a"), at(1), true);
        assert_eq!(ledger.state(Path::new("/a"), at(1)), ReadState::Current);
    }

    #[test]
    fn a_later_modification_makes_the_reading_stale() {
        let ledger = ReadLedger::default();
        ledger.record_read(Path::new("/a"), at(1), true);
        assert_eq!(ledger.state(Path::new("/a"), at(2)), ReadState::Stale);
    }

    #[test]
    fn a_partial_read_stays_partial_even_when_unchanged() {
        let ledger = ReadLedger::default();
        ledger.record_read(Path::new("/a"), at(1), false);
        assert_eq!(ledger.state(Path::new("/a"), at(1)), ReadState::Partial);
    }

    #[test]
    fn a_write_counts_as_having_read_the_file() {
        let ledger = ReadLedger::default();
        ledger.record_write(Path::new("/a"), at(5));
        assert_eq!(ledger.state(Path::new("/a"), at(5)), ReadState::Current);
    }

    #[test]
    fn a_missing_modification_time_does_not_read_as_a_change() {
        let ledger = ReadLedger::default();
        ledger.record_read(Path::new("/a"), None, true);
        assert_eq!(ledger.state(Path::new("/a"), None), ReadState::Current);
        assert_eq!(ledger.state(Path::new("/a"), at(9)), ReadState::Current);
    }

    #[test]
    fn clones_share_one_record() {
        let ledger = ReadLedger::default();
        let other = ledger.clone();
        other.record_read(Path::new("/a"), at(1), true);
        assert_eq!(ledger.state(Path::new("/a"), at(1)), ReadState::Current);
    }
}
