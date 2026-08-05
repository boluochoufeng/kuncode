//! Records which files a session has seen, so a whole-file write cannot
//! silently discard content nobody looked at.
//!
//! `write_file` truncates before writing: whatever the caller left out is gone.
//! That is safe when the caller has just read the file and is deciding what to
//! keep, and unrecoverable when it is writing from a guess. Only the session
//! knows which of the two happened, which is why this lives beside the plan on
//! [`ToolContext`](crate::tool::ToolContext) rather than inside either tool.
//!
//! "Seen" is deliberately coarse: one page of a long file counts, and so does a
//! read whose over-long lines came back clipped. Requiring every line, to the
//! end, unclipped was tried first and turned out to be a trap — a file holding
//! one line past the read cap could never satisfy it, because reading it again
//! clipped the same line again, so no sequence of calls left the file writable.
//! A guard meant to be satisfied by the caller's next move has to leave a move
//! that satisfies it.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::SystemTime,
};

/// What the ledger knows about a file that is about to be overwritten.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadState {
    /// Not seen in this session, so its contents are unknown here.
    Never,
    /// Seen, then changed on disk, so what was seen is no longer what is there.
    Stale,
    /// Seen, and unchanged since.
    Current,
}

/// Shared record of the files a session has seen, and how each stood when it
/// saw them.
///
/// Cloning shares the underlying map, mirroring
/// [`TodoHandle`](crate::todo::TodoHandle): the runner keeps one clone on the
/// session and hands another to tools through the
/// [`ToolContext`](crate::tool::ToolContext). [`Default`] yields a standalone
/// ledger attached to no session, so tests and non-interactive callers get a
/// usable target that simply records nothing anyone else will read.
#[derive(Clone, Debug, Default)]
pub struct ReadLedger(Arc<Mutex<HashMap<PathBuf, Option<SystemTime>>>>);

impl ReadLedger {
    /// Recovers the guard even if a previous holder panicked, for the reason
    /// [`TodoHandle::lock`](crate::todo::TodoHandle) does: the critical sections
    /// are trivial, and a poison error is not something a caller could act on.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Option<SystemTime>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Notes that the session has seen `path` as it stood at `modified`.
    ///
    /// Callers pass the modification time observed *before* taking the
    /// contents, so a write landing between the two leaves a newer time on disk
    /// than the one recorded and is caught as a change rather than absorbed.
    ///
    /// Writing counts as seeing: a caller that supplied a file's contents whole
    /// knows them as surely as one that read them, and without this, writing the
    /// same file twice in a session would be refused the second time.
    pub fn record(&self, path: &Path, modified: Option<SystemTime>) {
        self.lock().insert(path.to_path_buf(), modified);
    }

    /// Notes that `path` now stands at `modified` after a call that changed
    /// part of it, without claiming the session has seen the whole.
    ///
    /// `edit_file` names the text it replaces and leaves the rest alone, so it
    /// proves nothing about the parts of a file nobody looked at — a file it
    /// edits sight-unseen stays unseen, or editing one character would license
    /// replacing the whole. What it must not do is leave the baseline pointing
    /// at a modification time its own write invalidated, which would report the
    /// session's own edit back to it as somebody else's change.
    pub fn touch(&self, path: &Path, modified: Option<SystemTime>) {
        if let Some(baseline) = self.lock().get_mut(path) {
            *baseline = modified;
        }
    }

    /// What is known about `path`, given its modification time right now.
    ///
    /// `modified` of `None` on either side is treated as unchanged: a
    /// filesystem that does not report modification times cannot support this
    /// check, and refusing every write there would be worse than not checking.
    pub fn state(&self, path: &Path, modified: Option<SystemTime>) -> ReadState {
        let Some(seen) = self.lock().get(path).copied() else {
            return ReadState::Never;
        };
        match (seen, modified) {
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
    fn an_unseen_file_is_never() {
        let ledger = ReadLedger::default();
        assert_eq!(ledger.state(Path::new("/a"), at(1)), ReadState::Never);
    }

    #[test]
    fn a_file_seen_and_unchanged_is_current() {
        let ledger = ReadLedger::default();
        ledger.record(Path::new("/a"), at(1));
        assert_eq!(ledger.state(Path::new("/a"), at(1)), ReadState::Current);
    }

    #[test]
    fn a_later_modification_makes_the_reading_stale() {
        let ledger = ReadLedger::default();
        ledger.record(Path::new("/a"), at(1));
        assert_eq!(ledger.state(Path::new("/a"), at(2)), ReadState::Stale);
    }

    #[test]
    fn a_missing_modification_time_does_not_read_as_a_change() {
        let ledger = ReadLedger::default();
        ledger.record(Path::new("/a"), None);
        assert_eq!(ledger.state(Path::new("/a"), None), ReadState::Current);
        assert_eq!(ledger.state(Path::new("/a"), at(9)), ReadState::Current);
    }

    #[test]
    fn touching_a_seen_file_moves_its_baseline_forward() {
        let ledger = ReadLedger::default();
        ledger.record(Path::new("/a"), at(1));
        // What `edit_file` does to a file this session had already read: the
        // change is the session's own, so it must not read back as somebody
        // else's.
        ledger.touch(Path::new("/a"), at(2));
        assert_eq!(ledger.state(Path::new("/a"), at(2)), ReadState::Current);
    }

    #[test]
    fn touching_an_unseen_file_leaves_it_unseen() {
        let ledger = ReadLedger::default();
        // Editing one line of a file nobody read says nothing about the rest of
        // it. Recording it here would turn `edit_file` into a way around the
        // guard: one trivial edit, then a whole-file write over contents the
        // session never saw.
        ledger.touch(Path::new("/a"), at(1));
        assert_eq!(ledger.state(Path::new("/a"), at(1)), ReadState::Never);
    }

    #[test]
    fn clones_share_one_record() {
        let ledger = ReadLedger::default();
        let other = ledger.clone();
        other.record(Path::new("/a"), at(1));
        assert_eq!(ledger.state(Path::new("/a"), at(1)), ReadState::Current);
    }

    #[test]
    fn a_deep_clone_keeps_what_was_recorded_and_diverges_after() {
        let ledger = ReadLedger::default();
        ledger.record(Path::new("/a"), at(1));
        let forked = ledger.deep_clone();

        forked.record(Path::new("/b"), at(1));

        assert_eq!(forked.state(Path::new("/a"), at(1)), ReadState::Current);
        assert_eq!(ledger.state(Path::new("/b"), at(1)), ReadState::Never);
    }
}
