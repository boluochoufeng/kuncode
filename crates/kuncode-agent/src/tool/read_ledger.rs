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

/// What one file's reads have covered so far.
///
/// A large file is read a page at a time, and reading it *whole* is what
/// licenses replacing it whole — so pages have to accumulate. Tracking only the
/// most recent one would deadlock the caller: every page after the first starts
/// past line 1, so no sequence of reads could ever satisfy the check.
#[derive(Clone, Copy, Debug)]
struct Entry {
    /// Modification time observed *before* the contents were read, so a write
    /// that lands between the two is caught rather than absorbed.
    modified: Option<SystemTime>,
    /// Last line of an unbroken run of pages starting at line 1: `Some(n)` means
    /// lines `1..=n` have been seen. `None` once a page starts past where the
    /// previous one ended, since the gap between them was never returned.
    covered: Option<usize>,
    /// A line's tail was dropped somewhere in that run, so even a run reaching
    /// the end of the file has not shown all of its bytes.
    lossy: bool,
    /// The most recent page ran to the end of the file.
    at_eof: bool,
}

impl Entry {
    /// Whether the file has been seen in full: an unbroken run from line 1, to
    /// the end, with no line's tail dropped along the way.
    fn complete(&self) -> bool {
        self.covered.is_some() && self.at_eof && !self.lossy
    }
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

    /// Notes that one page of `path` was returned: `returned` lines starting at
    /// `start_line`, with `has_more` lines after it and `lossy` set when some
    /// line's tail was dropped to fit a size cap.
    ///
    /// Pages join onto the previous run when they start exactly where it ended,
    /// which is what lets a file read in several calls count as read in full.
    pub fn record_read(
        &self,
        path: &Path,
        modified: Option<SystemTime>,
        start_line: usize,
        returned: usize,
        has_more: bool,
        lossy: bool,
    ) {
        let mut records = self.lock();
        let entry = records.entry(path.to_path_buf()).or_insert(Entry {
            modified,
            covered: None,
            lossy: false,
            at_eof: false,
        });
        // Pages read before a change describe content that is no longer there,
        // so the run restarts rather than extending across it.
        if entry.modified != modified {
            *entry = Entry {
                modified,
                covered: None,
                lossy: false,
                at_eof: false,
            };
        }
        entry.covered = match (start_line, entry.covered) {
            (1, _) => Some(returned),
            (start, Some(covered)) if start == covered + 1 => Some(covered + returned),
            // Starting anywhere else leaves lines nobody has seen behind it.
            _ => None,
        };
        entry.lossy = if start_line == 1 {
            lossy
        } else {
            entry.lossy || lossy
        };
        entry.at_eof = !has_more;
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
                covered: Some(0),
                lossy: false,
                at_eof: true,
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
        if !entry.complete() {
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

    /// One page: `start_line`, `returned`, `has_more`, and no dropped tails.
    fn page(ledger: &ReadLedger, at_time: Option<SystemTime>, page: (usize, usize, bool)) {
        let (start_line, returned, has_more) = page;
        ledger.record_read(
            Path::new("/a"),
            at_time,
            start_line,
            returned,
            has_more,
            false,
        );
    }

    #[test]
    fn a_full_read_of_an_unchanged_file_is_current() {
        let ledger = ReadLedger::default();
        page(&ledger, at(1), (1, 10, false));
        assert_eq!(ledger.state(Path::new("/a"), at(1)), ReadState::Current);
    }

    #[test]
    fn a_later_modification_makes_the_reading_stale() {
        let ledger = ReadLedger::default();
        page(&ledger, at(1), (1, 10, false));
        assert_eq!(ledger.state(Path::new("/a"), at(2)), ReadState::Stale);
    }

    #[test]
    fn one_page_of_a_longer_file_is_partial() {
        let ledger = ReadLedger::default();
        page(&ledger, at(1), (1, 10, true));
        assert_eq!(ledger.state(Path::new("/a"), at(1)), ReadState::Partial);
    }

    #[test]
    fn pages_read_end_to_end_add_up_to_a_whole_file() {
        let ledger = ReadLedger::default();
        // What a caller does with a file too long for one reply. Every page
        // after the first starts past line 1, so judging pages one at a time
        // would leave this file permanently unwritable.
        page(&ledger, at(1), (1, 1000, true));
        page(&ledger, at(1), (1001, 1000, true));
        page(&ledger, at(1), (2001, 1000, false));
        assert_eq!(ledger.state(Path::new("/a"), at(1)), ReadState::Current);
    }

    #[test]
    fn a_gap_between_pages_leaves_the_file_partial() {
        let ledger = ReadLedger::default();
        page(&ledger, at(1), (1, 1000, true));
        // Resuming past the gap: lines 1001..=1500 were never returned.
        page(&ledger, at(1), (1501, 500, false));
        assert_eq!(ledger.state(Path::new("/a"), at(1)), ReadState::Partial);
    }

    #[test]
    fn a_change_partway_through_paging_restarts_the_run() {
        let ledger = ReadLedger::default();
        page(&ledger, at(1), (1, 1000, true));
        // The tail belongs to a different file than the head did.
        page(&ledger, at(2), (1001, 1000, false));
        assert_eq!(ledger.state(Path::new("/a"), at(2)), ReadState::Partial);
    }

    #[test]
    fn a_dropped_line_tail_anywhere_in_the_run_keeps_it_partial() {
        let ledger = ReadLedger::default();
        ledger.record_read(Path::new("/a"), at(1), 1, 1000, true, true);
        page(&ledger, at(1), (1001, 1000, false));
        assert_eq!(ledger.state(Path::new("/a"), at(1)), ReadState::Partial);
    }

    #[test]
    fn an_empty_file_is_read_in_full_by_returning_nothing() {
        let ledger = ReadLedger::default();
        page(&ledger, at(1), (1, 0, false));
        assert_eq!(ledger.state(Path::new("/a"), at(1)), ReadState::Current);
    }

    #[test]
    fn re_reading_from_the_top_starts_a_fresh_run() {
        let ledger = ReadLedger::default();
        ledger.record_read(Path::new("/a"), at(1), 1, 1000, true, true);
        // A second pass from line 1 replaces the first, dropped tails included.
        page(&ledger, at(1), (1, 1000, false));
        assert_eq!(ledger.state(Path::new("/a"), at(1)), ReadState::Current);
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
        page(&ledger, None, (1, 10, false));
        assert_eq!(ledger.state(Path::new("/a"), None), ReadState::Current);
        assert_eq!(ledger.state(Path::new("/a"), at(9)), ReadState::Current);
    }

    #[test]
    fn clones_share_one_record() {
        let ledger = ReadLedger::default();
        let other = ledger.clone();
        page(&other, at(1), (1, 10, false));
        assert_eq!(ledger.state(Path::new("/a"), at(1)), ReadState::Current);
    }
}
