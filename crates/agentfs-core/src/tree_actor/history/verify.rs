//! Saves history could not check (docs/trees.md, "When history cannot check
//! a save"). The builder replays each save from its own inputs and compares
//! the replay's layout with the one the save recorded. When they differ, it
//! builds the step from the record all the same (the saved bytes win), and
//! the commit carries the flag: an `Agentfs-Unverified` trailer
//! (`message.rs`). The flag is history: it never moves, and every clone
//! carries it.
//!
//! Beside history, two notes refs, neither a copy:
//!
//! - [`EVIDENCE_REF`]: one note per flagged commit, the replay's inputs as
//!   metadata (no file bytes), written by the builder only;
//! - [`VERIFY_REF`]: the verdicts, one `Agentfs-Verdict:` line each, written
//!   by the check at a start and by `agentfs verify`, both under the
//!   volume's lock, and by an operator acknowledging a flag with `git notes
//!   --ref=agentfs-verify append`.
//!
//! A verdict covers the replay code only: it is this release's replay of the
//! builder's inputs, frozen at flag time. Every writer reads a notes ref's tip
//! just before its compare-and-swap, so nothing here is cached.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use agentfs_contracts::pod::PodContext;
use git2::Oid;
use serde::{Deserialize, Serialize};

use super::super::journal::PagePiece;
use super::super::journal::durable::RecordNo;
use super::super::recording::Call;
use super::super::recording::layout::{Fingerprint, Layout, Place};
use super::super::recording::pages::Run;
use super::message;
use super::refs;
use crate::tree_actor::pointers::Pointers;
use crate::tree_actor::{Error, Result, TreeId};
use crate::tree_git::TreeGit;

/// The verdicts' notes ref: one note per flagged commit, each line an
/// `Agentfs-Verdict:` (git's own notes layout, fan-out included).
pub const VERIFY_REF: &str = "refs/notes/agentfs-verify";

/// The evidence notes ref: one note per flagged commit, the replay's inputs
/// as JSON ([`Evidence`]). The builder is its only writer.
pub const EVIDENCE_REF: &str = "refs/notes/agentfs-verify-evidence";

/// The evidence's own format, apart from the state format.
const EVIDENCE_FORMAT: &str = "agentfs-verify-evidence-1";

/// A verdict line's key.
const VERDICT: &str = "Agentfs-Verdict: ";

/// This binary, as a flag or verdict names it: `agentfs <version> build
/// <first 16 hex digits of the SHA-256 of /proc/self/exe>`, computed once.
/// `None` when the binary cannot be read; a `None` is never "the same
/// release".
pub fn release() -> Option<&'static str> {
    static RELEASE: OnceLock<Option<String>> = OnceLock::new();
    RELEASE
        .get_or_init(|| {
            use sha2::Digest;
            use std::io::Read;
            let mut file = std::fs::File::open("/proc/self/exe").ok()?;
            let mut hash = sha2::Sha256::new();
            let mut buf = vec![0; 1 << 16];
            loop {
                match file.read(&mut buf).ok()? {
                    0 => break,
                    n => hash.update(&buf[..n]),
                }
            }
            let digest = hash.finalize();
            let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
            Some(format!("agentfs {} build {hex}", env!("CARGO_PKG_VERSION")))
        })
        .as_deref()
}

/// [`release`], or this version with an unknown build: what a flag or
/// verdict written by a binary that cannot read itself names.
pub(in crate::tree_actor) fn release_or_unknown() -> String {
    release().map_or_else(
        || format!("agentfs {} build unknown", env!("CARGO_PKG_VERSION")),
        str::to_owned,
    )
}

/// What a flag says: the recorded layout's and the replay's fingerprints
/// (the replay's `None` when a piece named no run), and the builder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flag {
    pub recorded: Fingerprint,
    pub replayed: Option<Fingerprint>,
    pub builder: String,
}

/// One flagged commit, as history holds it: its step (the commit's event),
/// the open it closed, its tree and the flag; the file's object and path come
/// from its evidence (none in the walk).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unverified {
    /// The save's record: its step.
    pub step: RecordNo,
    /// The open the save closed: its record number, the open's own.
    pub handle: u64,
    pub tree: TreeId,
    /// The file's object number in `tree`; 0 until read from the evidence.
    pub object: u64,
    /// The file's name in `tree` at the step; empty until read from the
    /// evidence.
    pub path: String,
    /// The commit the step made: the notes' target.
    pub commit: Oid,
    pub flag: Flag,
}

impl Unverified {
    /// The flagged direct save `message` describes, at `commit`: `None` for
    /// a commit with no flag.
    pub(in crate::tree_actor) fn of_commit(commit: Oid, message: &[u8]) -> Option<Self> {
        let flag = message::unverified(message)?;
        Some(Self {
            step: RecordNo::new(message::direct_event(message)?.get()),
            handle: message::open_of(message)?,
            tree: message::tree_of(message).ok()??,
            object: 0,
            path: String::new(),
            commit,
            flag,
        })
    }
}

/// A verdict on a flagged commit, one `Agentfs-Verdict:` line of its note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// This release's replay of the evidence equals the recorded layout.
    /// Clears the flag.
    Verified,
    /// This release's replay of the evidence still differs.
    MismatchConfirmed,
    /// A person looked and accepted it (`git notes append`). Clears the
    /// flag; no program writes it.
    Acknowledged,
}

impl Verdict {
    /// Its word in a note.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::MismatchConfirmed => "mismatch-confirmed",
            Self::Acknowledged => "acknowledged",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        match word {
            "verified" => Some(Self::Verified),
            "mismatch-confirmed" => Some(Self::MismatchConfirmed),
            "acknowledged" => Some(Self::Acknowledged),
            _ => None,
        }
    }

    /// Whether it clears the flag.
    fn clears(self) -> bool {
        matches!(self, Self::Verified | Self::Acknowledged)
    }
}

/// What a check did with one flagged commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A verdict, appended to the commit's note.
    Wrote(Verdict),
    /// This release flagged it or judged it before: the same code would
    /// repeat the same computation, so nothing is written.
    SameRelease,
    /// The commit has no evidence that parses: nothing to replay.
    NoEvidence,
}

/// One flagged commit checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    pub step: RecordNo,
    pub tree: TreeId,
    pub path: String,
    pub commit: Oid,
    pub outcome: Outcome,
}

/// The flagged commits by state: the alarm's gauge
/// (`agentfs.history.unverified{verdict}`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UnverifiedCounts {
    /// No verdict, or a verdict note that does not parse.
    pub unresolved: u64,
    /// The latest verdict is mismatch-confirmed.
    pub mismatch_confirmed: u64,
}

/// One own call of the save, as evidence keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceCall {
    pub record: u64,
    /// `write`, `append` or `truncate`.
    pub kind: String,
    #[serde(default)]
    pub offset: u64,
    /// A write's or append's length; a truncate's size.
    pub len: u64,
    /// Where its bytes are: (record, offset into its tail).
    pub place: (u64, u64),
}

/// One page-write run of the file, as evidence keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRun {
    pub record: u64,
    pub offset: u64,
    pub len: u64,
    pub place: (u64, u64),
}

/// One page-write piece the save named.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidencePiece {
    pub record: u64,
    pub offset: u64,
    pub len: u64,
}

/// A layout, as evidence keeps it: its length and ranges (offset, length,
/// record, offset into the tail; record 0 for zeros).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceLayout {
    pub len: u64,
    pub ranges: Vec<[u64; 4]>,
}

/// A flagged save's replay inputs, metadata only: what `agentfs verify`
/// replays with a later release. JSON with its own format, apart from the
/// state format.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub format: String,
    pub step: u64,
    pub handle: u64,
    pub tree: String,
    pub object: u64,
    pub path: String,
    pub before_len: u64,
    pub calls: Vec<EvidenceCall>,
    pub runs: Vec<EvidenceRun>,
    pub pieces: Vec<EvidencePiece>,
    pub recorded: EvidenceLayout,
    pub replayed: Option<EvidenceLayout>,
}

impl EvidenceLayout {
    fn of(layout: &Layout) -> Self {
        Self {
            len: layout.len(),
            ranges: layout.ranges(),
        }
    }
}

/// What the builder knows of a save it flags.
pub(in crate::tree_actor) struct Inputs<'a, L: super::super::recording::layout::Located> {
    pub step: RecordNo,
    pub handle: u64,
    pub tree: TreeId,
    pub object: u64,
    pub path: &'a str,
    pub before_len: u64,
    /// The open's calls, where the builder holds them.
    pub own: &'a imbl::Vector<(u64, Call, L)>,
    pub writes: &'a [(u64, Run<L>)],
    pub pieces: &'a [PagePiece],
    pub recorded: &'a Layout,
    pub replayed: Option<&'a Layout>,
}

impl Evidence {
    /// The evidence of the save `inputs` describe.
    pub(in crate::tree_actor) fn of<L: super::super::recording::layout::Located>(
        inputs: &Inputs<'_, L>,
    ) -> Self {
        let place = |at: &L| {
            let place = at.place();
            (place.record.get(), place.at)
        };
        Self {
            format: EVIDENCE_FORMAT.to_owned(),
            step: inputs.step.get(),
            handle: inputs.handle,
            tree: inputs.tree.to_string(),
            object: inputs.object,
            path: inputs.path.to_owned(),
            before_len: inputs.before_len,
            calls: inputs
                .own
                .iter()
                .map(|(record, call, at)| {
                    let (kind, offset, len) = match *call {
                        Call::Write { offset, len } => ("write", offset, len),
                        Call::Append { len } => ("append", 0, len),
                        Call::Truncate(size) => ("truncate", 0, size),
                    };
                    EvidenceCall {
                        record: *record,
                        kind: kind.to_owned(),
                        offset,
                        len,
                        place: place(at),
                    }
                })
                .collect(),
            runs: inputs
                .writes
                .iter()
                .map(|(record, run)| EvidenceRun {
                    record: *record,
                    offset: run.offset,
                    len: run.len,
                    place: place(&run.at),
                })
                .collect(),
            pieces: inputs
                .pieces
                .iter()
                .map(|piece| EvidencePiece {
                    record: piece.record.get(),
                    offset: piece.offset,
                    len: piece.len,
                })
                .collect(),
            recorded: EvidenceLayout::of(inputs.recorded),
            replayed: inputs.replayed.map(EvidenceLayout::of),
        }
    }

    /// This release's replay of the evidence equals its recorded layout.
    ///
    /// # Errors
    ///
    /// [`Error::Format`] for evidence no builder wrote: a call of no kind, a
    /// recorded layout that is not canonical; a replay's own failure.
    fn replays_to_recorded(&self) -> Result<bool> {
        let at = |(record, at): (u64, u64)| Place {
            record: RecordNo::new(record),
            at,
        };
        let own = self
            .calls
            .iter()
            .map(|call| {
                let parsed = match call.kind.as_str() {
                    "write" => Call::Write {
                        offset: call.offset,
                        len: call.len,
                    },
                    "append" => Call::Append { len: call.len },
                    "truncate" => Call::Truncate(call.len),
                    _ => return Err(Error::Format),
                };
                Ok((call.record, parsed, at(call.place)))
            })
            .collect::<Result<Vec<_>>>()?;
        let writes: Vec<(u64, Run<Place>)> = self
            .runs
            .iter()
            .map(|run| {
                (
                    run.record,
                    Run {
                        offset: run.offset,
                        len: run.len,
                        at: at(run.place),
                    },
                )
            })
            .collect();
        let pieces: Vec<PagePiece> = self
            .pieces
            .iter()
            .map(|piece| PagePiece {
                record: RecordNo::new(piece.record),
                offset: piece.offset,
                len: piece.len,
            })
            .collect();
        let recorded = Layout::of_ranges(self.recorded.len, &self.recorded.ranges)?;
        let replayed = Layout::replay(self.before_len, own.iter(), &writes, &pieces)?;
        Ok(replayed.as_ref() == Some(&recorded))
    }
}

/// The evidence's text: its JSON. The one home of the format;
/// [`parse_evidence`] reads it back.
///
/// # Errors
///
/// The JSON writer's failure.
pub fn render_evidence(evidence: &Evidence) -> Result<String> {
    let mut text = serde_json::to_string(evidence)?;
    text.push('\n');
    Ok(text)
}

/// Evidence read back.
///
/// # Errors
///
/// [`Error::Format`] for text [`render_evidence`] did not write, or of
/// another evidence format.
pub fn parse_evidence(text: &str) -> Result<Evidence> {
    let evidence: Evidence = serde_json::from_str(text).map_err(|_| Error::Format)?;
    if evidence.format != EVIDENCE_FORMAT {
        return Err(Error::Format);
    }
    Ok(evidence)
}

/// A read-only snapshot of [`VERIFY_REF`]: each commit's verdicts, oldest
/// first, with who reached each, and the commits whose note does not parse.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::tree_actor) struct Notes {
    pub verdicts: BTreeMap<Oid, Vec<(Verdict, String)>>,
    pub unreadable: BTreeSet<Oid>,
}

/// A note's verdicts; `None` when a line is no verdict. Blank lines, which
/// `git notes append` puts between paragraphs, are passed over.
fn parse_verdicts(text: &str) -> Option<Vec<(Verdict, String)>> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let rest = line.strip_prefix(VERDICT)?;
            let (word, who) = rest.split_once(" by ")?;
            Some((Verdict::parse(word.trim())?, who.trim().to_owned()))
        })
        .collect()
}

impl Notes {
    /// The verdicts at the store's [`VERIFY_REF`]: none when it has no such
    /// ref. Never fails: a note that does not parse is unreadable, and a ref
    /// that cannot be read is logged as `tree HistoryNotesUnreadable` and
    /// read as no verdicts; either counts as unresolved.
    pub(in crate::tree_actor) fn read(repo: &TreeGit) -> Self {
        let mut notes = Self::default();
        match repo.notes(VERIFY_REF) {
            Ok(all) => {
                for (commit, text) in all {
                    match parse_verdicts(&text) {
                        Some(verdicts) => {
                            notes.verdicts.insert(commit, verdicts);
                        }
                        None => {
                            eprintln!(
                                "tree HistoryNotesUnreadable: commit={commit}: its note under \
                                 {VERIFY_REF} holds a line that is no verdict; the flag stands \
                                 (docs/runbooks/recording-failures.md)"
                            );
                            notes.unreadable.insert(commit);
                        }
                    }
                }
            }
            Err(e) => eprintln!(
                "tree HistoryNotesUnreadable: {VERIFY_REF}: {e}; every flag stands \
                 (docs/runbooks/recording-failures.md)"
            ),
        }
        notes
    }

    /// The latest verdict on `commit`, if its note parses and has one.
    fn latest(&self, commit: Oid) -> Option<Verdict> {
        if self.unreadable.contains(&commit) {
            return None;
        }
        self.verdicts
            .get(&commit)
            .and_then(|verdicts| verdicts.last())
            .map(|(verdict, _)| *verdict)
    }

    /// Whether `commit`'s flag still stands: its latest verdict clears it
    /// not.
    fn stands(&self, commit: Oid) -> bool {
        !self.latest(commit).is_some_and(Verdict::clears)
    }

    /// The flagged commits by state: unresolved (no verdict, or a note that
    /// does not parse), mismatch-confirmed (the latest verdict); a latest
    /// verdict of verified or acknowledged counts in neither.
    pub(in crate::tree_actor) fn counts(&self, flagged: &[Oid]) -> UnverifiedCounts {
        let mut counts = UnverifiedCounts::default();
        for commit in flagged {
            match self.latest(*commit) {
                None => counts.unresolved += 1,
                Some(Verdict::MismatchConfirmed) => counts.mismatch_confirmed += 1,
                Some(Verdict::Verified | Verdict::Acknowledged) => {}
            }
        }
        counts
    }
}

/// Writes `evidence` as `commit`'s note under [`EVIDENCE_REF`], on the tip
/// read just before; left as it is when the commit has evidence already (a
/// batch built again makes the same commit).
///
/// # Errors
///
/// A store read or write, or a tip moved since it was read.
pub(in crate::tree_actor) fn write_evidence(
    repo: &TreeGit,
    commit: Oid,
    evidence: &Evidence,
    committer: &git2::Signature<'_>,
) -> Result<()> {
    if repo.note(EVIDENCE_REF, commit)?.is_some() {
        return Ok(());
    }
    let tip = repo.note_tip(EVIDENCE_REF)?;
    repo.write_note(
        EVIDENCE_REF,
        commit,
        &render_evidence(evidence)?,
        committer,
        tip,
    )?;
    Ok(())
}

/// Appends `Agentfs-Verdict: <verdict> by <by>` to `commit`'s note under
/// [`VERIFY_REF`], in one new notes commit on the tip read just before.
///
/// # Errors
///
/// A store read or write, or a tip moved since it was read.
pub(in crate::tree_actor) fn resolve(
    repo: &TreeGit,
    commit: Oid,
    verdict: Verdict,
    by: &str,
    committer: &git2::Signature<'_>,
) -> Result<()> {
    let tip = repo.note_tip(VERIFY_REF)?;
    let mut text = repo.note(VERIFY_REF, commit)?.unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("{VERDICT}{} by {by}\n", verdict.as_str()));
    repo.write_note(VERIFY_REF, commit, &text, committer, tip)?;
    Ok(())
}

/// The one check, shared by a start and `agentfs verify`: each flagged
/// commit whose flag still stands, replayed from its evidence by this
/// release. The same release as the flag's builder or any verdict's author
/// writes nothing; no parseable evidence writes nothing; otherwise the
/// verdict (verified iff the replay equals the recorded layout) is appended.
/// A commit whose verdict note does not parse is not checked.
///
/// # Errors
///
/// A store read or a verdict's write: the commits checked before it keep
/// their verdicts.
pub(in crate::tree_actor) fn check_flags(
    repo: &TreeGit,
    flagged: &[Unverified],
    release: Option<&str>,
    committer: &git2::Signature<'_>,
) -> Result<Vec<Checked>> {
    let notes = Notes::read(repo);
    let by = release.map_or_else(release_or_unknown, str::to_owned);
    let mut checked = Vec::new();
    for flag in flagged {
        if notes.unreadable.contains(&flag.commit) || !notes.stands(flag.commit) {
            continue;
        }
        let evidence = repo
            .note(EVIDENCE_REF, flag.commit)?
            .and_then(|text| parse_evidence(&text).ok());
        let judged_here = release.is_some_and(|release| {
            flag.flag.builder == release
                || notes
                    .verdicts
                    .get(&flag.commit)
                    .is_some_and(|verdicts| verdicts.iter().any(|(_, who)| who == release))
        });
        let outcome = if judged_here {
            Outcome::SameRelease
        } else {
            match &evidence {
                None => Outcome::NoEvidence,
                Some(evidence) => {
                    let verdict = match evidence.replays_to_recorded() {
                        Ok(true) => Verdict::Verified,
                        Ok(false) | Err(Error::Format | Error::TooLarge) => {
                            Verdict::MismatchConfirmed
                        }
                        Err(e) => return Err(e),
                    };
                    resolve(repo, flag.commit, verdict, &by, committer)?;
                    Outcome::Wrote(verdict)
                }
            }
        };
        checked.push(Checked {
            step: flag.step,
            tree: flag.tree,
            path: evidence.map(|e| e.path).unwrap_or_default(),
            commit: flag.commit,
            outcome,
        });
    }
    Ok(checked)
}

/// What `agentfs verify` found: each flag it checked, each commit whose
/// verdict note does not parse, and how many flags still stand (unresolved,
/// mismatched, unreadable or without evidence).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerifyReport {
    pub checked: Vec<Checked>,
    pub unreadable: Vec<Oid>,
    pub standing: u64,
}

/// The committer of a verdict: `agentfs`, named with the pod context it ran
/// in, now.
///
/// # Errors
///
/// libgit2's refusal of the name.
pub(in crate::tree_actor) fn verdict_committer(
    at: &PodContext,
) -> Result<git2::Signature<'static>> {
    let pod: Vec<String> = [&at.namespace, &at.pod, &at.node]
        .into_iter()
        .flatten()
        .map(ToString::to_string)
        .collect();
    let name = if pod.is_empty() {
        "agentfs".to_owned()
    } else {
        format!("agentfs ({})", pod.join(" "))
    };
    Ok(git2::Signature::now(&name, "agentfs@localhost")?)
}

/// Every flagged commit reachable from `pointers`' branches, queue entries
/// and collision refs, in one walk.
///
/// # Errors
///
/// The walk's failure.
fn flagged_in(repo: &TreeGit, pointers: &Pointers) -> Result<Vec<Unverified>> {
    let mut flagged = Vec::new();
    repo.for_each_commit_message(&refs::owner_tips(pointers), |oid, message| {
        flagged.extend(Unverified::of_commit(oid, message));
    })?;
    Ok(flagged)
}

/// `agentfs verify`: every flagged commit whose flag stands, checked by this
/// release ([`check_flags`]). Takes the state area's lock and opens only its
/// `history.git`, so a release of another state format can run it.
///
/// # Errors
///
/// [`Error::Busy`] while another process holds the volume;
/// [`Error::NotCreated`] for a store with no root branch; a store read or a
/// verdict's write.
pub fn verify_flagged(root: &Path, by: &PodContext, release: Option<&str>) -> Result<VerifyReport> {
    crate::history::init_libgit2();
    let root = root.canonicalize()?;
    let lock = crate::tree_actor::lock_area(&root)?;
    let repo = super::open_store(&root.join("history.git"), lock)?;
    let pointers = Pointers::new(
        repo.list_refs(refs::ALL)?
            .into_iter()
            .filter(|(name, _)| !name.starts_with("refs/notes/"))
            .collect(),
    );
    if pointers.get(&refs::branch(TreeId::Root)).is_none() {
        return Err(Error::NotCreated);
    }
    let flagged = flagged_in(&repo, &pointers)?;
    let checked = check_flags(&repo, &flagged, release, &verdict_committer(by)?)?;
    let notes = Notes::read(&repo);
    let unreadable: Vec<Oid> = flagged
        .iter()
        .map(|flag| flag.commit)
        .filter(|commit| notes.unreadable.contains(commit))
        .collect();
    let standing = flagged
        .iter()
        .filter(|flag| notes.stands(flag.commit))
        .count() as u64;
    Ok(VerifyReport {
        checked,
        unreadable,
        standing,
    })
}

/// A volume's flagged commits by state, shared by the builder (which adds
/// each flag once its batch is built), a start (which sets them from
/// history) and the alarm's gauge. Reading them takes only their own lock,
/// never the actor's, and no read is torn.
#[derive(Clone, Debug, Default)]
pub struct Verification {
    counts: Arc<Mutex<UnverifiedCounts>>,
}

impl Verification {
    fn lock(&self) -> std::sync::MutexGuard<'_, UnverifiedCounts> {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The flagged commits by state, now.
    pub fn counts(&self) -> UnverifiedCounts {
        *self.lock()
    }

    /// The counts as a start read them from history.
    pub(in crate::tree_actor) fn set(&self, counts: UnverifiedCounts) {
        *self.lock() = counts;
    }

    /// `n` more flagged commits, none checked yet.
    pub(in crate::tree_actor) fn add_unresolved(&self, n: u64) {
        self.lock().unresolved += n;
    }
}

impl crate::tree_actor::TreeActor {
    /// The volume's [`Verification`] handle, for the alarm's gauge.
    pub fn verification(&self) -> Verification {
        self.verification.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_lines_parse_and_anything_else_is_unreadable() {
        let text = "Agentfs-Verdict: mismatch-confirmed by agentfs 0.1.0 build aa\n\n\
                    Agentfs-Verdict: acknowledged by an operator\n";
        assert_eq!(
            parse_verdicts(text),
            Some(vec![
                (
                    Verdict::MismatchConfirmed,
                    "agentfs 0.1.0 build aa".to_owned()
                ),
                (Verdict::Acknowledged, "an operator".to_owned()),
            ])
        );
        assert_eq!(parse_verdicts("not a verdict\n"), None);
        assert_eq!(parse_verdicts("Agentfs-Verdict: fine by me\n"), None);
    }

    #[test]
    fn evidence_round_trips_and_refuses_another_format() {
        let evidence = Evidence {
            format: EVIDENCE_FORMAT.to_owned(),
            step: 9,
            handle: 3,
            tree: "root".to_owned(),
            object: 3,
            path: "a \"b\"".to_owned(),
            before_len: 0,
            calls: vec![EvidenceCall {
                record: 4,
                kind: "write".to_owned(),
                offset: 0,
                len: 2,
                place: (4, 0),
            }],
            runs: Vec::new(),
            pieces: Vec::new(),
            recorded: EvidenceLayout {
                len: 2,
                ranges: vec![[0, 2, 4, 0]],
            },
            replayed: None,
        };
        let text = render_evidence(&evidence).unwrap();
        assert_eq!(parse_evidence(&text).unwrap(), evidence);
        assert!(evidence.replays_to_recorded().unwrap());
        let other = text.replace(EVIDENCE_FORMAT, "agentfs-verify-evidence-0");
        assert!(matches!(parse_evidence(&other), Err(Error::Format)));
    }
}
