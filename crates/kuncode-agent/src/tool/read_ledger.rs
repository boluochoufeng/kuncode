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
//!
//! Seeing does not last forever: compaction can drop the tool result that
//! carried a file's contents out of the active context, after which the session
//! "knows" the file only as a summary's paraphrase. Each sighting therefore
//! remembers the tool call that witnessed it, and when the active context is
//! replaced the session [evicts](ReadLedger::evict_unwitnessed) every sighting
//! whose witness is gone. Evicted entries are kept, not removed, so the refusal
//! can say what actually happened — "read, then compacted away" — instead of
//! the false "never read".

use std::{
    collections::{HashMap, HashSet},
    fs::Metadata,
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
    /// Seen, and the disk still matches — but the reading itself was compacted
    /// out of the conversation, so what was seen is no longer in view.
    Evicted,
    /// Seen, and unchanged since.
    Current,
}

/// How a file stood at the moment the session looked at it.
///
/// Two dimensions rather than one, because neither settles the question alone.
/// A modification time can be coarser than the edit-format-edit cycle that
/// rewrites a file twice within its resolution, and it can move *backwards*
/// when a change restores an older timestamp — `rsync -t`, `cp -p`, and
/// archives unpacked with times preserved all do that, so treating only a
/// *newer* time as a change would miss them. A size, meanwhile, says nothing
/// whenever an edit happens to preserve the byte count.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FileStamp {
    modified: Option<SystemTime>,
    size: Option<u64>,
}

impl FileStamp {
    /// Takes both dimensions off filesystem metadata.
    pub fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            modified: metadata.modified().ok(),
            size: Some(metadata.len()),
        }
    }

    /// Whether these two stamps describe demonstrably different files.
    ///
    /// A dimension missing on either side abstains rather than voting either
    /// way: a filesystem that reports no modification times cannot support that
    /// half of the check, and neither refusing every write there nor allowing
    /// every write there is something its absence actually argues for.
    fn differs_from(&self, other: &Self) -> bool {
        fn conflicts<T: PartialEq>(left: Option<T>, right: Option<T>) -> bool {
            matches!((left, right), (Some(left), Some(right)) if left != right)
        }
        conflicts(self.modified, other.modified) || conflicts(self.size, other.size)
    }
}

/// One file the session has seen: how it stood, and which tool result still
/// testifies to that in the conversation.
#[derive(Clone, Debug)]
struct Sighting {
    stamp: FileStamp,
    /// Id of the tool call whose result carried this sighting into the
    /// conversation. Exchanges are compacted as closed units, so this id
    /// appearing among the active tool results means the whole exchange —
    /// a read's returned contents, or a write's supplied contents — is still
    /// in view. `None` when the recording context named no call; such a
    /// sighting has no testimony to point at and does not survive eviction.
    witness: Option<Arc<str>>,
    /// Set once the witnessing result left the active context: the file was
    /// seen, but what was seen is no longer in view.
    evicted: bool,
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
pub struct ReadLedger {
    seen: Arc<Mutex<HashMap<PathBuf, Sighting>>>,
    /// Witness stamped into sightings recorded through this handle. The runner
    /// binds each tool call's id via [`Self::witnessed_by`]; the session's own
    /// handle, and plain [`Default`] ledgers, record without one.
    witness: Option<Arc<str>>,
}

impl ReadLedger {
    /// Recovers the guard even if a previous holder panicked, for the reason
    /// [`TodoHandle::lock`](crate::todo::TodoHandle) does: the critical sections
    /// are trivial, and a poison error is not something a caller could act on.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Sighting>> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A handle onto the same records whose future recordings name `call_id`
    /// as their witness.
    ///
    /// The runner binds the id of the tool call it is about to execute, so a
    /// sighting recorded by that call can later be matched against the tool
    /// results still present in the active context.
    pub fn witnessed_by(&self, call_id: &str) -> Self {
        Self {
            seen: Arc::clone(&self.seen),
            witness: Some(Arc::from(call_id)),
        }
    }

    /// Notes that the session has seen `path` as `stamp` describes it.
    ///
    /// Callers stamp the file *before* taking its contents, so a write landing
    /// between the two leaves the file no longer matching what was recorded and
    /// is caught as a change rather than absorbed.
    ///
    /// Writing counts as seeing: a caller that supplied a file's contents whole
    /// knows them as surely as one that read them, and without this, writing the
    /// same file twice in a session would be refused the second time.
    ///
    /// Recording replaces the previous sighting outright, eviction included:
    /// re-reading is precisely the move that answers an evicted entry.
    pub fn record(&self, path: &Path, stamp: FileStamp) {
        self.lock().insert(
            path.to_path_buf(),
            Sighting {
                stamp,
                witness: self.witness.clone(),
                evicted: false,
            },
        );
    }

    /// Notes that `path` now stands at `stamp` after a call that changed part
    /// of it, without claiming the session has seen the whole.
    ///
    /// `edit_file` names the text it replaces and leaves the rest alone, so it
    /// proves nothing about the parts of a file nobody looked at — a file it
    /// edits sight-unseen stays unseen, or editing one character would license
    /// replacing the whole. What it must not do is leave the baseline
    /// describing a file its own write just changed, which would report the
    /// session's own edit back to it as somebody else's.
    ///
    /// Only the stamp moves. The witness stays with the read (or write) that
    /// saw the file whole — an edit's exchange carries `old_text`, not the
    /// file — and an eviction stays too, for the same reason: touching part of
    /// a forgotten file does not bring the rest back into view.
    pub fn touch(&self, path: &Path, stamp: FileStamp) {
        if let Some(sighting) = self.lock().get_mut(path) {
            sighting.stamp = stamp;
        }
    }

    /// What is known about `path`, given how it stands right now.
    ///
    /// A change on disk outranks an eviction: both demand a re-read, but the
    /// disk having moved is the fact the caller cannot infer from its own
    /// history, so it is the one reported.
    pub fn state(&self, path: &Path, now: FileStamp) -> ReadState {
        let Some(sighting) = self.lock().get(path).cloned() else {
            return ReadState::Never;
        };
        if sighting.stamp.differs_from(&now) {
            ReadState::Stale
        } else if sighting.evicted {
            ReadState::Evicted
        } else {
            ReadState::Current
        }
    }

    /// Downgrades every sighting whose witness is not in `witnessed`.
    ///
    /// The session calls this after replacing its active context (compaction):
    /// a sighting whose witnessing tool result no longer appears there — or
    /// that never had a witness to look for — stops licensing whole-file
    /// writes, because the contents it vouched for are now known only as a
    /// summary's paraphrase. The entry itself stays, so the refusal can name
    /// the eviction instead of denying the read ever happened, and so a later
    /// disk change is still told apart as [`ReadState::Stale`].
    ///
    /// An id that does appear is trusted to carry its contents: lossy
    /// slimming rewrites only `Slimmable` results, and no file-content tool
    /// marks its results that way — a coupling this check depends on.
    pub(crate) fn evict_unwitnessed(&self, witnessed: &HashSet<&str>) {
        for sighting in self.lock().values_mut() {
            let stands = sighting
                .witness
                .as_deref()
                .is_some_and(|witness| witnessed.contains(witness));
            if !stands {
                sighting.evicted = true;
            }
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
        Self {
            seen: Arc::new(Mutex::new(self.lock().clone())),
            witness: self.witness.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A stamp with both dimensions known.
    fn stamp(secs: u64, size: u64) -> FileStamp {
        FileStamp {
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
            size: Some(size),
        }
    }

    #[test]
    fn an_unseen_file_is_never() {
        let ledger = ReadLedger::default();
        assert_eq!(
            ledger.state(Path::new("/a"), stamp(1, 10)),
            ReadState::Never
        );
    }

    #[test]
    fn a_file_seen_and_unchanged_is_current() {
        let ledger = ReadLedger::default();
        ledger.record(Path::new("/a"), stamp(1, 10));
        assert_eq!(
            ledger.state(Path::new("/a"), stamp(1, 10)),
            ReadState::Current
        );
    }

    #[test]
    fn a_later_modification_makes_the_reading_stale() {
        let ledger = ReadLedger::default();
        ledger.record(Path::new("/a"), stamp(1, 10));
        assert_eq!(
            ledger.state(Path::new("/a"), stamp(2, 10)),
            ReadState::Stale
        );
    }

    #[test]
    fn a_modification_time_moved_backwards_is_still_a_change() {
        let ledger = ReadLedger::default();
        ledger.record(Path::new("/a"), stamp(5, 10));
        // What `rsync -t`, `cp -p`, or an archive unpacked with times preserved
        // leaves behind. The file is not the one that was read, and which
        // direction its clock went says nothing about that.
        assert_eq!(
            ledger.state(Path::new("/a"), stamp(1, 10)),
            ReadState::Stale
        );
    }

    #[test]
    fn a_size_change_is_caught_even_when_the_clock_did_not_move() {
        let ledger = ReadLedger::default();
        ledger.record(Path::new("/a"), stamp(1, 10));
        // A second write inside the filesystem's timestamp resolution — the
        // edit-then-format cycle does this routinely on a coarse clock.
        assert_eq!(
            ledger.state(Path::new("/a"), stamp(1, 40)),
            ReadState::Stale
        );
    }

    #[test]
    fn a_dimension_nobody_can_report_does_not_vote() {
        let ledger = ReadLedger::default();
        let no_clock = FileStamp {
            modified: None,
            size: Some(10),
        };
        ledger.record(Path::new("/a"), no_clock);

        // The size still decides on its own where it can.
        assert_eq!(ledger.state(Path::new("/a"), no_clock), ReadState::Current);
        assert_eq!(
            ledger.state(Path::new("/a"), stamp(9, 10)),
            ReadState::Current
        );
        assert_eq!(
            ledger.state(Path::new("/a"), stamp(9, 40)),
            ReadState::Stale
        );
    }

    #[test]
    fn a_stamp_with_nothing_in_it_can_never_be_stale() {
        let ledger = ReadLedger::default();
        ledger.record(Path::new("/a"), FileStamp::default());
        // Refusing every write on a filesystem that reports neither dimension
        // would make the tool unusable there, which is worse than not checking.
        assert_eq!(
            ledger.state(Path::new("/a"), FileStamp::default()),
            ReadState::Current
        );
    }

    #[test]
    fn touching_a_seen_file_moves_its_baseline_forward() {
        let ledger = ReadLedger::default();
        ledger.record(Path::new("/a"), stamp(1, 10));
        // What `edit_file` does to a file this session had already read: the
        // change is the session's own, so it must not read back as somebody
        // else's.
        ledger.touch(Path::new("/a"), stamp(2, 40));
        assert_eq!(
            ledger.state(Path::new("/a"), stamp(2, 40)),
            ReadState::Current
        );
    }

    #[test]
    fn touching_an_unseen_file_leaves_it_unseen() {
        let ledger = ReadLedger::default();
        // Editing one line of a file nobody read says nothing about the rest of
        // it. Recording it here would turn `edit_file` into a way around the
        // guard: one trivial edit, then a whole-file write over contents the
        // session never saw.
        ledger.touch(Path::new("/a"), stamp(1, 10));
        assert_eq!(
            ledger.state(Path::new("/a"), stamp(1, 10)),
            ReadState::Never
        );
    }

    #[test]
    fn eviction_downgrades_a_sighting_whose_witness_is_gone() {
        let ledger = ReadLedger::default();
        ledger
            .witnessed_by("call-1")
            .record(Path::new("/a"), stamp(1, 10));

        // No tool result survived into the new active context.
        ledger.evict_unwitnessed(&HashSet::new());

        assert_eq!(
            ledger.state(Path::new("/a"), stamp(1, 10)),
            ReadState::Evicted
        );
    }

    #[test]
    fn a_sighting_whose_witness_survives_stays_current() {
        let ledger = ReadLedger::default();
        ledger
            .witnessed_by("call-1")
            .record(Path::new("/a"), stamp(1, 10));

        ledger.evict_unwitnessed(&HashSet::from(["call-1"]));

        assert_eq!(
            ledger.state(Path::new("/a"), stamp(1, 10)),
            ReadState::Current
        );
    }

    #[test]
    fn a_sighting_recorded_without_a_witness_does_not_survive_eviction() {
        let ledger = ReadLedger::default();
        // Recorded through an unbound handle: there is no testimony to look
        // for, so a context replacement must stop it licensing whole writes.
        ledger.record(Path::new("/a"), stamp(1, 10));

        ledger.evict_unwitnessed(&HashSet::from(["call-1"]));

        assert_eq!(
            ledger.state(Path::new("/a"), stamp(1, 10)),
            ReadState::Evicted
        );
    }

    #[test]
    fn re_reading_after_eviction_restores_current() {
        let ledger = ReadLedger::default();
        ledger
            .witnessed_by("call-1")
            .record(Path::new("/a"), stamp(1, 10));
        ledger.evict_unwitnessed(&HashSet::new());

        // The move an evicted refusal asks for.
        ledger
            .witnessed_by("call-2")
            .record(Path::new("/a"), stamp(1, 10));

        assert_eq!(
            ledger.state(Path::new("/a"), stamp(1, 10)),
            ReadState::Current
        );
    }

    #[test]
    fn a_disk_change_outranks_an_eviction() {
        let ledger = ReadLedger::default();
        ledger
            .witnessed_by("call-1")
            .record(Path::new("/a"), stamp(1, 10));
        ledger.evict_unwitnessed(&HashSet::new());

        // Both facts hold; the one the caller cannot infer from its own
        // history is the one reported.
        assert_eq!(
            ledger.state(Path::new("/a"), stamp(2, 10)),
            ReadState::Stale
        );
    }

    #[test]
    fn touching_an_evicted_sighting_does_not_bring_it_back() {
        let ledger = ReadLedger::default();
        ledger
            .witnessed_by("call-1")
            .record(Path::new("/a"), stamp(1, 10));
        ledger.evict_unwitnessed(&HashSet::new());

        // What `edit_file` does after compaction: the edit lands and moves the
        // baseline, but the rest of the file is still out of view.
        ledger.touch(Path::new("/a"), stamp(2, 40));

        assert_eq!(
            ledger.state(Path::new("/a"), stamp(2, 40)),
            ReadState::Evicted
        );
    }

    #[test]
    fn touch_leaves_the_witness_with_the_original_read() {
        let ledger = ReadLedger::default();
        ledger
            .witnessed_by("read-call")
            .record(Path::new("/a"), stamp(1, 10));
        // An edit exchange carries `old_text`, not the file, so touching
        // through a differently-bound handle moves the stamp only.
        ledger
            .witnessed_by("edit-call")
            .touch(Path::new("/a"), stamp(2, 40));

        // The read's testimony keeps the sighting standing…
        ledger.evict_unwitnessed(&HashSet::from(["read-call"]));
        assert_eq!(
            ledger.state(Path::new("/a"), stamp(2, 40)),
            ReadState::Current
        );

        // …and the edit's alone does not.
        ledger.evict_unwitnessed(&HashSet::from(["edit-call"]));
        assert_eq!(
            ledger.state(Path::new("/a"), stamp(2, 40)),
            ReadState::Evicted
        );
    }

    #[test]
    fn clones_share_one_record() {
        let ledger = ReadLedger::default();
        let other = ledger.clone();
        other.record(Path::new("/a"), stamp(1, 10));
        assert_eq!(
            ledger.state(Path::new("/a"), stamp(1, 10)),
            ReadState::Current
        );
    }

    #[test]
    fn a_deep_clone_keeps_what_was_recorded_and_diverges_after() {
        let ledger = ReadLedger::default();
        ledger.record(Path::new("/a"), stamp(1, 10));
        let forked = ledger.deep_clone();

        forked.record(Path::new("/b"), stamp(1, 10));

        assert_eq!(
            forked.state(Path::new("/a"), stamp(1, 10)),
            ReadState::Current
        );
        assert_eq!(
            ledger.state(Path::new("/b"), stamp(1, 10)),
            ReadState::Never
        );
    }
}
