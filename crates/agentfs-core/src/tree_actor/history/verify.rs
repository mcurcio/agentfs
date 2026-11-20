//! Saves history could not check: when the builder's replay of a save
//! disagrees with the save's record (its length or digest), the builder
//! builds the step from its own replay, flags its commit with a git note
//! under [`NOTES_REF`], raises `tree HistoryUnverified`, and the journal keeps
//! the step's records until `agentfs verify` checks it again and appends
//! `verified` or `mismatch-confirmed` to the note. History is never rewritten:
//! the commit stays, and the notes ref only gains commits (docs/recording.md,
//! "When history cannot check a save").
//!
//! The notes ref is History's own record, not a copy of a pointer: no
//! journal record names it, a start reads it from the store, and the store's
//! ref rebuild keeps it.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use git2::Oid;

use super::super::journal::durable::RecordNo;
use super::super::journal::segment::FramePos;
use super::super::journal::{Record, SaveOutcome};
use super::super::recording::{Call, FileMap};
use crate::tree_actor::{Result, TreeActor, TreeId};
use crate::tree_git::TreeGit;

/// The notes ref that flags each unverified save's commit: one note per
/// commit, git's own notes layout (fan-out by the commit's id).
#[allow(dead_code, reason = "frozen interface of agentfs-ln6.11")]
pub const NOTES_REF: &str = "refs/notes/agentfs-verify";

/// What the builder's check of a save compares: the length the save leaves
/// and the digest of what it wrote ([`FileMap::close_digest`]), a function of
/// the length before the save and the save's calls alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    pub len: u64,
    pub digest: u64,
}

impl Fingerprint {
    /// The fingerprint a save's record holds: `None` for an outcome that
    /// saved nothing (unchanged, dropped).
    #[allow(dead_code, reason = "frozen interface of agentfs-ln6.11")]
    pub(in crate::tree_actor) fn recorded(outcome: &SaveOutcome) -> Option<Self> {
        let _ = outcome;
        todo!("agentfs-ln6.11")
    }

    /// A replay's fingerprint: `map`'s length, and the digest of `calls` laid
    /// on a file of `saved_len` bytes. The builder's check and
    /// [`TreeActor::verify`] both compute it here, so the two can never differ
    /// in what they compare.
    ///
    /// # Errors
    ///
    /// As [`FileMap::close_digest`].
    #[allow(dead_code, reason = "frozen interface of agentfs-ln6.11")]
    pub(in crate::tree_actor) fn replayed(
        saved_len: u64,
        map: &FileMap,
        calls: &[(Call, FramePos)],
    ) -> Result<Self> {
        let _ = (saved_len, map, calls);
        todo!("agentfs-ln6.11")
    }
}

/// One save the builder built from its own replay while the replay
/// disagreed with the record: what its note holds before any verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unverified {
    /// The save's record: its step.
    pub step: RecordNo,
    /// The open the save closed: its record number, the open's own.
    pub handle: u64,
    pub tree: TreeId,
    /// The file's object number in `tree`.
    pub object: u64,
    /// The file's name in `tree` at the step.
    pub path: String,
    /// The page writes the save's pieces name, by record.
    pub page_writes: BTreeSet<RecordNo>,
    /// The commit the step made from the replay: the note's target.
    pub commit: Oid,
    pub recorded: Fingerprint,
    pub replayed: Fingerprint,
    /// The builder that flagged it: `agentfs <package version>`.
    pub builder: String,
}

/// What `agentfs verify` found for one flagged step, appended to its note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The replay now agrees with the record, and the commit's blob at the
    /// path holds the replayed bytes: the commit holds what the workload
    /// saved. Clears the alarm.
    Verified,
    /// The replay still disagrees with the record, or the commit's blob is
    /// not the replayed bytes. The flag stays, and so does the alarm.
    MismatchConfirmed,
}

impl Verdict {
    /// Its word in a note: `verified` or `mismatch-confirmed`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::MismatchConfirmed => "mismatch-confirmed",
        }
    }
}

/// One flagged step checked by [`TreeActor::verify`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    pub step: RecordNo,
    pub tree: TreeId,
    pub path: String,
    pub commit: Oid,
    pub verdict: Verdict,
}

/// The flagged steps by state: the alarm's gauge
/// (`agentfs.history.unverified{verdict}`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UnverifiedCounts {
    /// Flagged and not yet checked again: their records are kept.
    pub unresolved: u64,
    /// Checked again and still disagreeing.
    pub mismatch_confirmed: u64,
}

/// A note's state: its flag, then each verdict appended, oldest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) struct Note {
    pub flagged: Unverified,
    /// Each verdict with the `agentfs <version>` that reached it.
    pub verdicts: Vec<(Verdict, String)>,
}

/// The note's text: one `Key: value` line per field (git trailer syntax),
/// then one `Agentfs-Verdict: <verdict> by <builder>` line per verdict. The
/// one home of the format; [`parse_note`] reads it back.
#[allow(dead_code, reason = "frozen interface of agentfs-ln6.11")]
pub(in crate::tree_actor) fn render_note(note: &Note) -> String {
    let _ = note;
    todo!("agentfs-ln6.11")
}

/// A note's text read back.
///
/// # Errors
///
/// [`crate::tree_actor::Error::Format`] for text [`render_note`] did not write.
#[allow(dead_code, reason = "frozen interface of agentfs-ln6.11")]
pub(in crate::tree_actor) fn parse_note(commit: Oid, text: &str) -> Result<Note> {
    let _ = (commit, text);
    todo!("agentfs-ln6.11")
}

/// The journal records the unresolved steps keep from compaction: each
/// step's `Save` record, every `Call` record of its open, and the page writes
/// its pieces name. Compaction carries them forward as it carries a live
/// open's; a start's fold passes over them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::tree_actor) struct Kept {
    steps: BTreeSet<RecordNo>,
    handles: BTreeSet<u64>,
    page_writes: BTreeSet<RecordNo>,
}

impl Kept {
    /// Whether `record` is one an unresolved step keeps.
    #[allow(dead_code, reason = "frozen interface of agentfs-ln6.11")]
    pub(in crate::tree_actor) fn holds(&self, record: &Record) -> bool {
        let _ = (record, &self.steps, &self.handles, &self.page_writes);
        todo!("agentfs-ln6.11")
    }
}

/// Every note under [`NOTES_REF`], and its one writer: the builder flags,
/// [`TreeActor::verify`] appends verdicts. Behind one lock ([`Verification`]).
#[derive(Debug, Default)]
pub(in crate::tree_actor) struct Notes {
    /// The notes ref's tip: `None` before the first flag.
    tip: Option<Oid>,
    /// Each flagged step's note, by step.
    notes: std::collections::BTreeMap<RecordNo, Note>,
}

impl Notes {
    /// The notes at the store's [`NOTES_REF`]; none when it has no such ref.
    ///
    /// # Errors
    ///
    /// A store read; [`crate::tree_actor::Error::Format`] for a note
    /// [`parse_note`] refuses.
    #[allow(dead_code, reason = "frozen interface of agentfs-ln6.11")]
    pub(in crate::tree_actor) fn read(repo: &TreeGit) -> Result<Self> {
        let _ = repo;
        todo!("agentfs-ln6.11")
    }

    /// Flags `flagged.commit` with its note, in one new notes commit on the
    /// tip; the notes objects and the ref are durable before it returns. A
    /// step already flagged is left as it is (a batch built again after a
    /// crash flags the same step).
    ///
    /// # Errors
    ///
    /// The store's write.
    #[allow(dead_code, reason = "frozen interface of agentfs-ln6.11")]
    pub(in crate::tree_actor) fn flag(
        &mut self,
        repo: &TreeGit,
        flagged: Unverified,
    ) -> Result<()> {
        let _ = (repo, flagged, &self.tip, &self.notes);
        todo!("agentfs-ln6.11")
    }

    /// Appends `verdict`, reached by `by` (`agentfs <version>`), to `step`'s
    /// note, in one new notes commit; durable before it returns.
    ///
    /// # Errors
    ///
    /// [`crate::tree_actor::Error::NotFound`] for a step with no note; the
    /// store's write.
    #[allow(dead_code, reason = "frozen interface of agentfs-ln6.11")]
    pub(in crate::tree_actor) fn resolve(
        &mut self,
        repo: &TreeGit,
        step: RecordNo,
        verdict: Verdict,
        by: &str,
    ) -> Result<()> {
        let _ = (repo, step, verdict, by);
        todo!("agentfs-ln6.11")
    }

    /// The flagged steps with no verdict yet, in step order.
    #[allow(dead_code, reason = "frozen interface of agentfs-ln6.11")]
    pub(in crate::tree_actor) fn unresolved(&self) -> Vec<Unverified> {
        todo!("agentfs-ln6.11")
    }

    /// What the unresolved steps keep from compaction.
    #[allow(dead_code, reason = "frozen interface of agentfs-ln6.11")]
    pub(in crate::tree_actor) fn kept(&self) -> Kept {
        todo!("agentfs-ln6.11")
    }

    /// The flagged steps by state.
    #[allow(dead_code, reason = "frozen interface of agentfs-ln6.11")]
    pub(in crate::tree_actor) fn counts(&self) -> UnverifiedCounts {
        todo!("agentfs-ln6.11")
    }
}

/// A shared handle on a volume's [`Notes`]: the builder, the actor and the
/// alarm's gauge each hold one. Reading the counts takes only its lock,
/// never the actor's.
#[derive(Clone, Debug, Default)]
pub struct Verification {
    #[allow(dead_code, reason = "frozen interface of agentfs-ln6.11")]
    notes: Arc<Mutex<Notes>>,
}

impl Verification {
    /// The flagged steps by state, now.
    pub fn counts(&self) -> UnverifiedCounts {
        todo!("agentfs-ln6.11")
    }
}

impl TreeActor {
    /// Checks every unresolved step again with this binary: reads its kept
    /// records, replays the save on the length of the file in the commit's
    /// first parent, compares the replay's [`Fingerprint`] with the record's
    /// and the replayed bytes' blob id with the commit's blob at the path,
    /// appends the verdict to its note, and lets compaction drop the records
    /// of each step it resolved. Steps resolved before are not checked again.
    ///
    /// # Errors
    ///
    /// A journal or store read, or a note's write: the steps checked before
    /// it keep their verdicts.
    pub fn verify(&mut self) -> Result<Vec<Checked>> {
        todo!("agentfs-ln6.11")
    }

    /// The volume's [`Verification`] handle, for the alarm's gauge.
    pub fn verification(&self) -> Verification {
        todo!("agentfs-ln6.11")
    }
}
