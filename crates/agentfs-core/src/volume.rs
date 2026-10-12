//! A volume's state area, opened as a [`Recorder`] (used by the FUSE layer
//! on every change boundary) and a [`Committer`] (which turns records into
//! history in the background).
//!
//! Opening a volume is also crash recovery (design §3.3): records already in
//! history are dropped from the journal, the rest stay uncommitted, and
//! frozen copies nothing refers to are removed.
//!
//! The recorder applies path rules (design §4, §5) before freezing anything:
//! each input is recorded or skipped with a reason, and the set of tracked
//! paths (branch head plus uncommitted records) is kept current so ignore
//! rules only ever affect untracked paths. Each record claims its paths
//! ([`crate::claims`]) from the decision until the tracked set is updated, so
//! records touching the same path are decided and journaled in order.
//!
//! Kept inputs then go through the pre-commit checks ([`crate::checks`],
//! design §3.4). A refused path is skipped with [`SkipReason::Refused`] and
//! raises [`Event::PathRefused`]; the rest of the record carries on. The
//! committer checks every change again before committing it, for records
//! journaled before the limits changed.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::checks::{Checks, Limits, Refusal};
use crate::claims::PathClaims;
use crate::config::{ConfigError, validate_name};
use crate::events::{CheckStage, Event, Events};
use crate::history::{Committed, History, HistoryError};
use crate::identity::{Context, IdentityRenderer};
use crate::journal::{Journal, OpenReport};
use crate::paths::{PathError, RepoPath};
use crate::pending::{PendingId, PendingStore};
use crate::record::{Caller, Change, Draft, Record, Trigger};
use crate::rules::{Decision, Rules, RulesError, SkipReason};
use crate::tracked::Tracked;

/// Layout of a state area (design §2.2).
#[derive(Debug, Clone)]
pub struct StateDir {
    root: PathBuf,
}

impl StateDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn git(&self) -> PathBuf {
        self.root.join("git")
    }

    pub fn journal(&self) -> PathBuf {
        self.root.join("journal").join("journal")
    }

    pub fn pending(&self) -> PathBuf {
        self.root.join("pending")
    }

    /// The daemon's own bookkeeping.
    pub fn meta(&self) -> PathBuf {
        self.root.join("meta")
    }

    /// Per-remote push state (design §11.2).
    pub fn push_dir(&self) -> PathBuf {
        self.meta().join("push")
    }

    /// `remote`'s persisted push state.
    pub fn push_state(&self, remote: &str) -> PathBuf {
        self.push_dir().join(format!("{remote}.json"))
    }

    /// The lock that keeps two processes from pushing `remote` at once.
    pub fn push_lock(&self, remote: &str) -> PathBuf {
        self.push_dir().join(format!("{remote}.lock"))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VolumeError {
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Rules(#[from] RulesError),
}

/// One path's new state, as the FUSE layer sees it.
#[derive(Debug, Clone)]
pub enum Input {
    /// `path` holds the content currently at `backing`.
    Write {
        path: RepoPath,
        backing: PathBuf,
        executable: bool,
    },
    Symlink {
        path: RepoPath,
        target: String,
    },
    Remove {
        path: RepoPath,
    },
}

impl Input {
    pub fn path(&self) -> &RepoPath {
        match self {
            Self::Write { path, .. } | Self::Symlink { path, .. } | Self::Remove { path } => path,
        }
    }
}

/// What [`open`] found.
#[derive(Debug)]
pub struct Recovery {
    pub journal: OpenReport,
    /// Last sequence number already in history.
    pub committed_through: Option<u64>,
    /// Records recovered that still need committing.
    pub uncommitted: usize,
    /// Frozen copies removed because nothing referred to them.
    pub pruned_pending: usize,
}

/// Creates a new state area for `volume`.
pub fn init(state: &StateDir, volume: &str, branch: Option<&str>) -> Result<(), VolumeError> {
    validate_name("agentfs.volume", volume).map_err(config_error)?;
    History::init(&state.git(), volume, branch)?;
    std::fs::create_dir_all(state.journal().parent().expect("journal has a parent"))?;
    PendingStore::open(state.pending())?;
    std::fs::create_dir_all(state.push_dir())?;
    Ok(())
}

fn config_error(e: ConfigError) -> VolumeError {
    VolumeError::History(HistoryError::Config(e))
}

/// Opens an existing state area for the backing tree at `tree`. Alarms go to
/// `events` (design §13).
pub fn open(
    state: &StateDir,
    tree: &Path,
    context: Context,
    events: Events,
) -> Result<(Recorder, Committer, Recovery), VolumeError> {
    let history = History::open(&state.git())?;
    let checks = Arc::new(Checks::new(Limits::from_remotes(
        &history.settings().remotes,
    )));
    let identities = IdentityRenderer::new(history.settings().identity.clone(), &context)
        .map_err(HistoryError::from)?;
    let committed_through = history.head_seq()?;

    let pending = Arc::new(PendingStore::open(state.pending())?);
    let hook_store = pending.clone();
    let (journal, report) = Journal::open(
        state.journal(),
        committed_through.map_or(0, |s| s + 1),
        Box::new(move || hook_store.sync_dir()),
    )?;
    if let Some(seq) = committed_through {
        journal.truncate_through(seq)?;
    }
    let uncommitted = journal.uncommitted();
    let keep: HashSet<PendingId> = uncommitted.iter().flat_map(Record::pending_ids).collect();
    let pruned_pending = pending.prune_except(&keep)?;
    pending.sync_dir()?;

    let rules = Rules::load(&state.git(), tree, history.settings().into())?;
    let head_tree = history
        .head()?
        .map(|c| c.tree())
        .transpose()
        .map_err(HistoryError::from)?;
    let tracked = Tracked::from_tree(head_tree.as_ref()).map_err(HistoryError::from)?;
    drop(head_tree);
    for change in uncommitted.iter().flat_map(|r| &r.draft.changes) {
        tracked.apply(change);
    }

    let journal = Arc::new(journal);
    let recovery = Recovery {
        journal: report,
        committed_through,
        uncommitted: uncommitted.len(),
        pruned_pending,
    };
    let recorder = Recorder {
        journal: journal.clone(),
        pending: pending.clone(),
        rules: Arc::new(rules),
        tracked: Arc::new(tracked),
        claims: Arc::new(PathClaims::default()),
        checks: checks.clone(),
        events: events.clone(),
    };
    let committer = Committer {
        history,
        journal,
        pending,
        identities,
        context,
        checks,
        events,
    };
    Ok((recorder, committer, recovery))
}

/// An input the path rules kept out of history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: RepoPath,
    pub reason: SkipReason,
}

/// What one [`Recorder::record`] call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// The record's sequence number, or `None` if every input was skipped
    /// and nothing was journaled.
    pub seq: Option<u64>,
    pub skipped: Vec<Skipped>,
}

/// Records change boundaries. Cheap to clone; safe to share across threads.
#[derive(Clone)]
pub struct Recorder {
    journal: Arc<Journal>,
    pending: Arc<PendingStore>,
    rules: Arc<Rules>,
    tracked: Arc<Tracked>,
    claims: Arc<PathClaims>,
    checks: Arc<Checks>,
    events: Events,
}

impl Recorder {
    /// Durably records one change boundary, once it is safe to let the
    /// triggering call return.
    ///
    /// Each input is first checked against the path rules; skipped inputs
    /// cost nothing (no copy is frozen) and are reported with their reason.
    /// Removals are recorded only when something is tracked at or below the
    /// path, and are never filtered by rules, so history always follows the
    /// tree for tracked paths.
    ///
    /// Writes and symlinks the rules keep then go through the pre-commit
    /// checks (design §3.4). A refused input is skipped with
    /// [`SkipReason::Refused`], its frozen copy is removed at once, and
    /// [`Event::PathRefused`] is raised; the other inputs are still
    /// recorded. Removals are never refused. If the record fails,
    /// [`Event::RecordFailed`] is raised as well as returning the error.
    ///
    /// The record holds its paths, and so excludes every path above or below
    /// them, from the decision until the tracked set is updated
    /// ([`crate::claims`]). A removal racing an in-flight first write of the
    /// same path therefore waits for it, sees the path tracked, and is
    /// journaled after it. Records for unrelated paths do not wait for each
    /// other.
    ///
    /// Rule files and nested repositories are read without a claim on them,
    /// so a first write racing an edit of the `.gitignore` that covers it may
    /// be decided by either version of the rules.
    pub fn record(
        &self,
        trigger: Trigger,
        caller: Caller,
        inputs: Vec<Input>,
    ) -> Result<Recorded, VolumeError> {
        let paths: Vec<RepoPath> = inputs.iter().map(|i| i.path().clone()).collect();
        let result = self.record_inner(trigger, caller, inputs);
        if let Err(e) = &result {
            self.events.emit(Event::RecordFailed {
                trigger,
                paths,
                error: e.to_string(),
            });
        }
        result
    }

    fn record_inner(
        &self,
        trigger: Trigger,
        caller: Caller,
        inputs: Vec<Input>,
    ) -> Result<Recorded, VolumeError> {
        let _claim = self.claims.claim(inputs.iter().map(Input::path));
        let mut kept = Vec::with_capacity(inputs.len());
        let mut skipped = Vec::new();
        for input in inputs {
            match self.decide(&input)? {
                Decision::Record => kept.push(input),
                Decision::Skip(reason) => skipped.push(Skipped {
                    path: input.path().clone(),
                    reason,
                }),
            }
        }
        if kept.is_empty() {
            return Ok(Recorded { seq: None, skipped });
        }

        let mut changes = Vec::with_capacity(kept.len());
        let mut frozen = Vec::new();
        let result = (|| {
            for input in kept {
                let change = match input {
                    Input::Write {
                        path,
                        backing,
                        executable,
                    } => {
                        if let Some(refusal) = self.checks.check_path(&path) {
                            self.refuse(&mut skipped, path, refusal);
                            continue;
                        }
                        let (id, _) = self.pending.freeze(&backing)?;
                        frozen.push(id);
                        if let Some(refusal) =
                            self.checks.check_file(&path, &self.pending.path(id))?
                        {
                            frozen.pop();
                            self.pending.remove([id])?;
                            self.refuse(&mut skipped, path, refusal);
                            continue;
                        }
                        Change::Write {
                            path,
                            pending: id,
                            executable,
                        }
                    }
                    Input::Symlink { path, target } => {
                        if let Some(refusal) = self.checks.check_symlink(&path, &target) {
                            self.refuse(&mut skipped, path, refusal);
                            continue;
                        }
                        Change::Symlink { path, target }
                    }
                    Input::Remove { path } => Change::Remove { path },
                };
                changes.push(change);
            }
            if changes.is_empty() {
                return Ok(None);
            }
            let draft = Draft {
                trigger,
                caller,
                time_ms: now_ms(),
                changes: changes.clone(),
            };
            Ok(Some(self.journal.record(draft)?))
        })();
        match result {
            Ok(seq) => {
                for change in &changes {
                    self.tracked.apply(change);
                }
                Ok(Recorded { seq, skipped })
            }
            Err(e) => {
                // Copies frozen for a record that never became durable.
                let _ = self.pending.remove(frozen);
                Err(e)
            }
        }
    }

    fn refuse(&self, skipped: &mut Vec<Skipped>, path: RepoPath, refusal: Refusal) {
        self.events.emit(Event::PathRefused {
            path: path.to_string(),
            reason: refusal.clone(),
            stage: CheckStage::Record,
        });
        skipped.push(Skipped {
            path,
            reason: SkipReason::Refused(refusal),
        });
    }

    /// Reports a path the caller could not record at all because git cannot
    /// store it (design §5): a `.git` component or one of its look-alikes,
    /// or another [`PathError`]. Raises [`Event::PathRefused`].
    pub fn refuse_invalid(&self, raw: &str, error: PathError) {
        self.events.emit(Event::PathRefused {
            path: raw.to_owned(),
            reason: Refusal::InvalidPath(error),
            stage: CheckStage::Record,
        });
    }

    /// Where this volume's events go.
    pub fn events(&self) -> &Events {
        &self.events
    }

    fn decide(&self, input: &Input) -> Result<Decision, VolumeError> {
        Ok(match input {
            Input::Remove { path } => {
                if self.tracked.contains_or_under(path) {
                    Decision::Record
                } else {
                    Decision::Skip(SkipReason::Untracked)
                }
            }
            Input::Write { path, .. } | Input::Symlink { path, .. } => {
                self.rules.decide(path, self.tracked.contains(path))?
            }
        })
    }
}

/// Turns recorded changes into commits.
pub struct Committer {
    history: History,
    journal: Arc<Journal>,
    pending: Arc<PendingStore>,
    identities: IdentityRenderer,
    context: Context,
    checks: Arc<Checks>,
    events: Events,
}

/// What one commit pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CommitReport {
    pub records: usize,
    pub commits: usize,
    pub last_seq: Option<u64>,
    /// Changes dropped by the commit-time checks (design §3.4).
    pub refused: usize,
}

impl Committer {
    pub fn history(&self) -> &History {
        &self.history
    }

    /// Records durable but not yet in history.
    pub fn backlog(&self) -> usize {
        self.journal.uncommitted().len()
    }

    /// Commits every durable record, grouping records whose times fall
    /// within the configured commit window into one commit.
    pub fn commit_pending(&self) -> Result<CommitReport, VolumeError> {
        let records = self.journal.uncommitted();
        let groups = group_by_window(&records, self.window_ms());
        self.commit_groups(&records, &groups)
    }

    /// Like [`Committer::commit_pending`], but leaves the last group alone
    /// while more records could still join it: until `now_ms` is past its
    /// first record's time plus the commit window. A group whose first
    /// record is later than `now_ms` (the clock stepped back) counts as
    /// settled, so a clock step cannot stall commits.
    pub fn commit_settled(&self, now_ms: u64) -> Result<CommitReport, VolumeError> {
        let records = self.journal.uncommitted();
        let window_ms = self.window_ms();
        let mut groups = group_by_window(&records, window_ms);
        if let Some(last) = groups.last() {
            let first = last[0].draft.time_ms;
            let settled = now_ms > first.saturating_add(window_ms) || now_ms < first;
            if !settled {
                groups.pop();
            }
        }
        self.commit_groups(&records, &groups)
    }

    fn window_ms(&self) -> u64 {
        self.history.settings().commit_window.as_millis() as u64
    }

    /// Commits `groups`, a prefix of `records` split into runs.
    fn commit_groups(
        &self,
        records: &[Record],
        groups: &[&[Record]],
    ) -> Result<CommitReport, VolumeError> {
        let result = self.commit_groups_inner(records, groups);
        if let Err(e) = &result {
            self.events.emit(Event::CommitFailed {
                error: e.to_string(),
                backlog: self.backlog(),
            });
        }
        result
    }

    fn commit_groups_inner(
        &self,
        records: &[Record],
        groups: &[&[Record]],
    ) -> Result<CommitReport, VolumeError> {
        let mut report = CommitReport::default();
        let mut result = Ok(());
        for group in groups {
            let checked = match self.recheck(group) {
                Ok((checked, refused)) => {
                    report.refused += refused;
                    checked
                }
                Err(e) => {
                    result = Err(e);
                    break;
                }
            };
            match self
                .history
                .commit(&checked, &self.pending, &self.identities, &self.context)
            {
                Ok(Committed { commit, last_seq }) => {
                    report.records += group.len();
                    report.commits += usize::from(commit.is_some());
                    report.last_seq = Some(last_seq);
                }
                Err(e) => {
                    result = Err(e.into());
                    break;
                }
            }
        }
        if let Some(seq) = report.last_seq {
            self.journal.truncate_through(seq)?;
            let done = records
                .iter()
                .take_while(|r| r.seq <= seq)
                .flat_map(Record::pending_ids);
            self.pending.remove(done)?;
            self.pending.sync_dir()?;
        }
        result.map(|()| report)
    }

    /// Runs the pre-commit checks again over `group` (design §3.4) and
    /// returns it without the changes they refuse, with the count refused.
    /// This catches records journaled before the limits changed, such as
    /// records recovered after a remount with a stricter remote.
    fn recheck(&self, group: &[Record]) -> Result<(Vec<Record>, usize), VolumeError> {
        let mut refused = 0;
        let mut out = Vec::with_capacity(group.len());
        for record in group {
            let mut record = record.clone();
            let mut kept = Vec::with_capacity(record.draft.changes.len());
            for change in record.draft.changes {
                let refusal = match &change {
                    Change::Write { path, pending, .. } => {
                        self.checks.check_file(path, &self.pending.path(*pending))?
                    }
                    Change::Symlink { path, target } => self.checks.check_symlink(path, target),
                    Change::Remove { .. } => None,
                };
                match refusal {
                    Some(reason) => {
                        refused += 1;
                        self.events.emit(Event::PathRefused {
                            path: change.path().to_string(),
                            reason,
                            stage: CheckStage::Commit,
                        });
                    }
                    None => kept.push(change),
                }
            }
            record.draft.changes = kept;
            out.push(record);
        }
        Ok((out, refused))
    }
}

/// Splits records into runs where each record is within `window_ms` of the
/// run's first record.
fn group_by_window(records: &[Record], window_ms: u64) -> Vec<&[Record]> {
    let mut groups = Vec::new();
    let mut start = 0;
    for i in 1..records.len() {
        let first = records[start].draft.time_ms;
        if records[i].draft.time_ms.saturating_sub(first) > window_ms {
            groups.push(&records[start..i]);
            start = i;
        }
    }
    if start < records.len() {
        groups.push(&records[start..]);
    }
    groups
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::events::{MemorySink, NullSink};
    use crate::rules::RuleSource;

    const CALLER: Caller = Caller {
        uid: 10000,
        gid: 10000,
        pid: 1,
    };

    fn context() -> Context {
        Context {
            volume: "vol".into(),
            replica: "pod-0".into(),
            pod_uid: "u".into(),
            ..Context::default()
        }
    }

    fn null() -> Events {
        Arc::new(NullSink)
    }

    fn setup() -> (tempfile::TempDir, StateDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path().join("state"));
        init(&state, "vol", None).unwrap();
        (dir, state)
    }

    fn write(dir: &Path, name: &str, content: &str) -> Input {
        let backing = dir.join(name.replace('/', "_"));
        fs::write(&backing, content).unwrap();
        Input::Write {
            path: RepoPath::new(name).unwrap(),
            backing,
            executable: false,
        }
    }

    fn tree_paths(c: &Committer) -> Vec<String> {
        let head = c.history().head().unwrap().unwrap();
        let mut out = Vec::new();
        head.tree()
            .unwrap()
            .walk(git2::TreeWalkMode::PreOrder, |root, e| {
                if e.kind() == Some(git2::ObjectType::Blob) {
                    out.push(format!("{root}{}", e.name().unwrap()));
                }
                git2::TreeWalkResult::Ok
            })
            .unwrap();
        out
    }

    #[test]
    fn record_then_commit() {
        let (dir, state) = setup();
        let (rec, com, recovery) = open(&state, dir.path(), context(), null()).unwrap();
        assert_eq!(recovery.committed_through, None);

        rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "a", "1")])
            .unwrap();
        rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "d/b", "2")])
            .unwrap();
        assert_eq!(com.backlog(), 2);

        let report = com.commit_pending().unwrap();
        assert_eq!(report.records, 2);
        assert_eq!(report.last_seq, Some(1));
        assert_eq!(com.backlog(), 0);
        assert_eq!(tree_paths(&com), ["a", "d/b"]);
        assert_eq!(fs::read_dir(state.pending()).unwrap().count(), 0);
    }

    #[test]
    fn content_is_what_it_was_at_the_boundary() {
        let (dir, state) = setup();
        let (rec, com, _) = open(&state, dir.path(), context(), null()).unwrap();
        let input = write(dir.path(), "a", "at close");
        rec.record(Trigger::Close, CALLER, vec![input]).unwrap();
        fs::write(dir.path().join("a"), "changed later").unwrap();

        com.commit_pending().unwrap();
        let head = com.history().head().unwrap().unwrap();
        let blob = head.tree().unwrap().get_name("a").unwrap().id();
        let content = com.history().repo().find_blob(blob).unwrap();
        assert_eq!(content.content(), b"at close");
    }

    #[test]
    fn recorded_but_uncommitted_survives_a_crash() {
        let (dir, state) = setup();
        {
            let (rec, com, _) = open(&state, dir.path(), context(), null()).unwrap();
            rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "a", "1")])
                .unwrap();
            com.commit_pending().unwrap();
            rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "b", "2")])
                .unwrap();
            // Crash: the second record is durable but never committed.
        }
        let (rec, com, recovery) = open(&state, dir.path(), context(), null()).unwrap();
        assert_eq!(recovery.committed_through, Some(0));
        assert_eq!(recovery.uncommitted, 1);

        com.commit_pending().unwrap();
        assert_eq!(tree_paths(&com), ["a", "b"]);
        // New records continue after the recovered ones.
        let seq = rec
            .record(Trigger::Close, CALLER, vec![write(dir.path(), "c", "3")])
            .unwrap()
            .seq;
        assert_eq!(seq, Some(2));
    }

    #[test]
    fn crash_after_commit_before_truncate_does_not_duplicate() {
        let (dir, state) = setup();
        {
            let (rec, com, _) = open(&state, dir.path(), context(), null()).unwrap();
            rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "a", "1")])
                .unwrap();
            // Commit without letting the committer truncate: simulates a crash
            // between the ref update and journal truncation.
            let records = com.journal.uncommitted();
            com.history
                .commit(&records, &com.pending, &com.identities, &com.context)
                .unwrap();
        }
        let (_, com, recovery) = open(&state, dir.path(), context(), null()).unwrap();
        assert_eq!(recovery.committed_through, Some(0));
        assert_eq!(recovery.uncommitted, 0);
        assert_eq!(recovery.pruned_pending, 1);
        assert_eq!(com.commit_pending().unwrap(), CommitReport::default());
        let head = com.history().head().unwrap().unwrap();
        assert_eq!(head.parent_count(), 0, "no duplicate commit");
    }

    #[test]
    fn failed_record_leaves_no_frozen_copy() {
        let (dir, state) = setup();
        let sink = Arc::new(MemorySink::new());
        let (rec, _, _) = open(&state, dir.path(), context(), sink.clone()).unwrap();
        let err = rec.record(
            Trigger::Close,
            CALLER,
            vec![
                write(dir.path(), "a", "1"),
                Input::Write {
                    path: RepoPath::new("b").unwrap(),
                    backing: dir.path().join("missing"),
                    executable: false,
                },
            ],
        );
        assert!(matches!(err, Err(VolumeError::Io(_))), "{err:?}");
        assert_eq!(fs::read_dir(state.pending()).unwrap().count(), 0);
        let events = sink.take();
        assert!(
            matches!(
                &events[..],
                [Event::RecordFailed { trigger: Trigger::Close, paths, .. }]
                    if paths.len() == 2 && paths[1].as_str() == "b"
            ),
            "{events:?}"
        );
    }

    #[test]
    fn symlinked_gitmodules_is_refused_not_an_error() {
        let (dir, state) = setup();
        let sink = Arc::new(MemorySink::new());
        let (rec, com, _) = open(&state, dir.path(), context(), sink.clone()).unwrap();
        let recorded = rec
            .record(
                Trigger::Close,
                CALLER,
                vec![
                    write(dir.path(), "a", "1"),
                    Input::Symlink {
                        path: RepoPath::new("sub/.GITMODULES.").unwrap(),
                        target: "x".into(),
                    },
                ],
            )
            .unwrap();
        assert_eq!(recorded.seq, Some(0));
        let refusal = Refusal::SymlinkedSpecialFile {
            file: crate::paths::SpecialFile::Gitmodules,
        };
        assert_eq!(
            recorded.skipped,
            [skipped(
                "sub/.GITMODULES.",
                SkipReason::Refused(refusal.clone())
            )]
        );
        assert_eq!(
            sink.take(),
            [Event::PathRefused {
                path: "sub/.GITMODULES.".into(),
                reason: refusal,
                stage: CheckStage::Record,
            }]
        );
        com.commit_pending().unwrap();
        assert_eq!(tree_paths(&com), ["a"]);
    }

    #[test]
    fn refused_path_does_not_stop_the_record() {
        let (dir, state) = setup();
        let sink = Arc::new(MemorySink::new());
        let (rec, com, _) = open(&state, dir.path(), context(), sink.clone()).unwrap();
        let attrs = format!("* text\n{}\n", "x".repeat(3000));
        let recorded = rec
            .record(
                Trigger::Rename,
                CALLER,
                vec![
                    write(dir.path(), "ok.txt", "fine"),
                    write(dir.path(), ".gitattributes", &attrs),
                ],
            )
            .unwrap();
        assert_eq!(recorded.seq, Some(0));
        assert_eq!(
            recorded.skipped,
            [skipped(
                ".gitattributes",
                SkipReason::Refused(Refusal::GitattributesLineTooLong {
                    line: 2,
                    bytes: 3000
                })
            )]
        );
        // Only the kept input's frozen copy is waiting.
        assert_eq!(fs::read_dir(state.pending()).unwrap().count(), 1);
        let events = sink.take();
        assert!(
            matches!(&events[..], [Event::PathRefused { path, stage: CheckStage::Record, .. }] if path == ".gitattributes"),
            "{events:?}"
        );
        com.commit_pending().unwrap();
        assert_eq!(tree_paths(&com), ["ok.txt"]);
        assert_eq!(fs::read_dir(state.pending()).unwrap().count(), 0);
        // A refused path is not tracked: removing it records nothing.
        let recorded = rec
            .record(Trigger::Unlink, CALLER, vec![remove(".gitattributes")])
            .unwrap();
        assert_eq!(recorded.seq, None);
    }

    fn set_remote_limit(state: &StateDir, remote: &str, limit: u64) {
        let mut config = git2::Config::open(&state.git().join("config")).unwrap();
        config
            .set_str(&format!("remote.{remote}.url"), "/nowhere")
            .unwrap();
        config
            .set_i64(&format!("remote.{remote}.agentfsMaxBlobSize"), limit as i64)
            .unwrap();
    }

    fn blob(com: &Committer, path: &str) -> Vec<u8> {
        let head = com.history().head().unwrap().unwrap();
        let id = head.tree().unwrap().get_path(Path::new(path)).unwrap().id();
        com.history()
            .repo()
            .find_blob(id)
            .unwrap()
            .content()
            .to_vec()
    }

    #[test]
    fn refusing_a_tracked_path_keeps_its_last_content() {
        let (dir, state) = setup();
        {
            let (rec, com, _) = open(&state, dir.path(), context(), null()).unwrap();
            rec.record(
                Trigger::Close,
                CALLER,
                vec![write(dir.path(), "a.bin", "abc")],
            )
            .unwrap();
            com.commit_pending().unwrap();
        }
        set_remote_limit(&state, "r", 2);
        let sink = Arc::new(MemorySink::new());
        let (rec, com, _) = open(&state, dir.path(), context(), sink.clone()).unwrap();
        let recorded = rec
            .record(
                Trigger::Close,
                CALLER,
                vec![write(dir.path(), "a.bin", "abcde")],
            )
            .unwrap();
        assert_eq!(recorded.seq, None);
        assert_eq!(
            recorded.skipped[0].reason,
            SkipReason::Refused(Refusal::BlobTooLarge {
                bytes: 5,
                limit: 2,
                remote: "r".into()
            })
        );
        assert_eq!(fs::read_dir(state.pending()).unwrap().count(), 0);
        assert_eq!(com.commit_pending().unwrap(), CommitReport::default());
        assert_eq!(blob(&com, "a.bin"), b"abc");
        assert_eq!(sink.take().len(), 1);
    }

    #[test]
    fn commit_time_recheck() {
        let (dir, state) = setup();
        {
            let (rec, com, _) = open(&state, dir.path(), context(), null()).unwrap();
            rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "keep", "1")])
                .unwrap();
            com.commit_pending().unwrap();
            rec.record(
                Trigger::Close,
                CALLER,
                vec![
                    write(dir.path(), "big", "0123456789"),
                    write(dir.path(), "small", "12"),
                ],
            )
            .unwrap();
            // Crash before committing; the remount brings a stricter remote.
        }
        set_remote_limit(&state, "r", 5);
        let sink = Arc::new(MemorySink::new());
        let (_, com, recovery) = open(&state, dir.path(), context(), sink.clone()).unwrap();
        assert_eq!(recovery.uncommitted, 1);
        let report = com.commit_pending().unwrap();
        assert_eq!(report.refused, 1);
        assert_eq!(report.records, 1);
        assert_eq!(report.commits, 1);
        assert_eq!(tree_paths(&com), ["keep", "small"]);
        assert_eq!(com.backlog(), 0);
        assert_eq!(fs::read_dir(state.pending()).unwrap().count(), 0);
        let events = sink.take();
        assert!(
            matches!(
                &events[..],
                [Event::PathRefused { path, stage: CheckStage::Commit, reason: Refusal::BlobTooLarge { bytes: 10, .. } }]
                    if path == "big"
            ),
            "{events:?}"
        );
    }

    #[test]
    fn a_record_with_every_change_refused_at_commit_is_consumed() {
        let (dir, state) = setup();
        {
            let (rec, _, _) = open(&state, dir.path(), context(), null()).unwrap();
            rec.record(
                Trigger::Close,
                CALLER,
                vec![write(dir.path(), "big", "0123456789")],
            )
            .unwrap();
        }
        set_remote_limit(&state, "r", 5);
        let (rec, com, _) = open(&state, dir.path(), context(), null()).unwrap();
        let report = com.commit_pending().unwrap();
        // With no branch yet, an empty root commit carries the sequence.
        assert_eq!((report.records, report.commits, report.refused), (1, 1, 1));
        assert_eq!(report.last_seq, Some(0));
        assert_eq!(com.history().head_seq().unwrap(), Some(0));
        assert!(tree_paths(&com).is_empty());
        assert_eq!(com.backlog(), 0);
        // Sequence numbers continue after the consumed record.
        let seq = rec
            .record(Trigger::Close, CALLER, vec![write(dir.path(), "s", "1")])
            .unwrap()
            .seq;
        assert_eq!(seq, Some(1));
    }

    fn set_window(state: &StateDir, window: &str) {
        git2::Config::open(&state.git().join("config"))
            .unwrap()
            .set_str("agentfs.commitWindow", window)
            .unwrap();
    }

    /// Journals a record at `time_ms` directly, as if the clock read that.
    fn record_at(rec: &Recorder, dir: &Path, name: &str, time_ms: u64) {
        let backing = dir.join(name);
        fs::write(&backing, name).unwrap();
        let (id, _) = rec.pending.freeze(&backing).unwrap();
        let change = Change::Write {
            path: RepoPath::new(name).unwrap(),
            pending: id,
            executable: false,
        };
        rec.journal
            .record(Draft {
                trigger: Trigger::Close,
                caller: CALLER,
                time_ms,
                changes: vec![change.clone()],
            })
            .unwrap();
        rec.tracked.apply(&change);
    }

    #[test]
    fn commit_settled_leaves_the_open_group() {
        let (dir, state) = setup();
        set_window(&state, "200ms");
        let (rec, com, _) = open(&state, dir.path(), context(), null()).unwrap();
        let t = 1_700_000_000_000;
        record_at(&rec, dir.path(), "a", t);
        record_at(&rec, dir.path(), "b", t + 50);
        assert_eq!(
            com.commit_settled(t + 100).unwrap(),
            CommitReport::default()
        );
        assert_eq!(
            com.commit_settled(t + 200).unwrap(),
            CommitReport::default()
        );
        assert_eq!(com.backlog(), 2);
        let report = com.commit_settled(t + 300).unwrap();
        assert_eq!((report.records, report.commits), (2, 1));
        assert_eq!(tree_paths(&com), ["a", "b"]);

        // Earlier groups are committed even while the last one is open.
        record_at(&rec, dir.path(), "c", t + 1000);
        record_at(&rec, dir.path(), "d", t + 2000);
        let report = com.commit_settled(t + 2100).unwrap();
        assert_eq!(report.records, 1);
        assert_eq!(com.backlog(), 1);

        // After the clock steps back, the group counts as settled.
        let report = com.commit_settled(t).unwrap();
        assert_eq!(report.records, 1);
        assert_eq!(com.backlog(), 0);
        assert_eq!(tree_paths(&com), ["a", "b", "c", "d"]);
    }

    #[test]
    fn refuse_invalid_emits_an_alarm() {
        let (dir, state) = setup();
        let sink = Arc::new(MemorySink::new());
        let (rec, _, _) = open(&state, dir.path(), context(), sink.clone()).unwrap();
        let err = RepoPath::new("sub/.git/config").unwrap_err();
        rec.refuse_invalid("sub/.git/config", err.clone());
        let events = sink.take();
        assert_eq!(
            events,
            [Event::PathRefused {
                path: "sub/.git/config".into(),
                reason: Refusal::InvalidPath(err),
                stage: CheckStage::Record,
            }]
        );
        assert!(events[0].to_string().contains(".git"), "{}", events[0]);
        rec.events().emit(Event::CommitFailed {
            error: "x".into(),
            backlog: 0,
        });
        assert_eq!(sink.take().len(), 1);
    }

    #[test]
    fn init_validates_the_volume_name() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path().join("state"));
        assert!(matches!(
            init(&state, "a/b", None),
            Err(VolumeError::History(HistoryError::Config(
                ConfigError::BadName { .. }
            )))
        ));
        assert!(!state.root().exists(), "nothing is created");
        init(&state, "ok", None).unwrap();
        assert!(state.push_dir().is_dir());
        assert_eq!(state.push_state("r"), state.push_dir().join("r.json"));
        assert_eq!(state.push_lock("r"), state.push_dir().join("r.lock"));
    }

    #[test]
    fn grouping_by_window() {
        let rec = |seq, time_ms| Record {
            seq,
            draft: Draft {
                trigger: Trigger::Close,
                caller: CALLER,
                time_ms,
                changes: vec![],
            },
        };
        let records = [
            rec(0, 0),
            rec(1, 150),
            rec(2, 200),
            rec(3, 201),
            rec(4, 900),
        ];
        let groups: Vec<Vec<u64>> = group_by_window(&records, 200)
            .into_iter()
            .map(|g| g.iter().map(|r| r.seq).collect())
            .collect();
        assert_eq!(groups, [vec![0, 1, 2], vec![3], vec![4]]);
        assert!(group_by_window(&[], 200).is_empty());
    }

    fn exclude(state: &StateDir, patterns: &str) {
        fs::write(state.git().join("info").join("exclude"), patterns).unwrap();
    }

    fn remove(name: &str) -> Input {
        Input::Remove {
            path: RepoPath::new(name).unwrap(),
        }
    }

    fn skipped(path: &str, reason: SkipReason) -> Skipped {
        Skipped {
            path: RepoPath::new(path).unwrap(),
            reason,
        }
    }

    #[test]
    fn excluded_untracked_path_is_skipped_without_cost() {
        let (dir, state) = setup();
        exclude(&state, "*.log\n");
        let (rec, com, _) = open(&state, dir.path(), context(), null()).unwrap();
        let recorded = rec
            .record(
                Trigger::Close,
                CALLER,
                vec![write(dir.path(), "a.log", "x")],
            )
            .unwrap();
        assert_eq!(
            recorded,
            Recorded {
                seq: None,
                skipped: vec![skipped(
                    "a.log",
                    SkipReason::Ignored {
                        source: RuleSource::OperatorExclude,
                        pattern: "*.log".into(),
                    }
                )],
            }
        );
        assert_eq!(com.backlog(), 0);
        assert_eq!(fs::read_dir(state.pending()).unwrap().count(), 0);
    }

    #[test]
    fn rename_into_an_excluded_path_records_only_the_removal() {
        let (dir, state) = setup();
        exclude(&state, "ignored/\n");
        let (rec, com, _) = open(&state, dir.path(), context(), null()).unwrap();
        rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "src", "1")])
            .unwrap();
        com.commit_pending().unwrap();

        let recorded = rec
            .record(
                Trigger::Rename,
                CALLER,
                vec![remove("src"), write(dir.path(), "ignored/dst", "1")],
            )
            .unwrap();
        assert_eq!(recorded.seq, Some(1));
        assert_eq!(recorded.skipped.len(), 1);
        assert_eq!(recorded.skipped[0].path.as_str(), "ignored/dst");
        com.commit_pending().unwrap();
        assert!(tree_paths(&com).is_empty());
        assert_eq!(fs::read_dir(state.pending()).unwrap().count(), 0);
    }

    #[test]
    fn removals_need_something_tracked() {
        let (dir, state) = setup();
        let (rec, com, _) = open(&state, dir.path(), context(), null()).unwrap();
        let recorded = rec
            .record(Trigger::Unlink, CALLER, vec![remove("never")])
            .unwrap();
        assert_eq!(recorded.seq, None);
        assert_eq!(recorded.skipped, [skipped("never", SkipReason::Untracked)]);
        assert_eq!(com.backlog(), 0);

        rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "d/a", "1")])
            .unwrap();
        // Tracked as soon as it is durable, before any commit.
        let recorded = rec
            .record(Trigger::Rmdir, CALLER, vec![remove("d")])
            .unwrap();
        assert_eq!(recorded.seq, Some(1));
        assert!(recorded.skipped.is_empty());
        com.commit_pending().unwrap();
        assert_eq!(
            com.history().head().unwrap().unwrap().tree().unwrap().len(),
            0
        );
    }

    #[test]
    fn uncommitted_records_stay_tracked_across_recovery() {
        let (dir, state) = setup();
        {
            let (rec, _, _) = open(&state, dir.path(), context(), null()).unwrap();
            rec.record(
                Trigger::Close,
                CALLER,
                vec![write(dir.path(), "a.log", "1")],
            )
            .unwrap();
            // Crash before committing.
        }
        exclude(&state, "*.log\n");
        let (rec, com, recovery) = open(&state, dir.path(), context(), null()).unwrap();
        assert_eq!(recovery.uncommitted, 1);
        let recorded = rec
            .record(
                Trigger::Close,
                CALLER,
                vec![write(dir.path(), "a.log", "2")],
            )
            .unwrap();
        assert_eq!(recorded.seq, Some(1));
        assert!(recorded.skipped.is_empty());
        // A new path is still excluded.
        let recorded = rec
            .record(
                Trigger::Close,
                CALLER,
                vec![write(dir.path(), "b.log", "3")],
            )
            .unwrap();
        assert_eq!(recorded.seq, None);
        com.commit_pending().unwrap();
        assert_eq!(tree_paths(&com), ["a.log"]);
    }

    #[test]
    fn committed_paths_stay_tracked_after_an_exclude() {
        let (dir, state) = setup();
        {
            let (rec, com, _) = open(&state, dir.path(), context(), null()).unwrap();
            rec.record(
                Trigger::Close,
                CALLER,
                vec![write(dir.path(), "a.log", "1")],
            )
            .unwrap();
            com.commit_pending().unwrap();
        }
        exclude(&state, "*.log\n");
        let (rec, com, _) = open(&state, dir.path(), context(), null()).unwrap();
        let recorded = rec
            .record(
                Trigger::Close,
                CALLER,
                vec![write(dir.path(), "a.log", "new")],
            )
            .unwrap();
        assert_eq!(recorded.seq, Some(1));
        com.commit_pending().unwrap();
        let head = com.history().head().unwrap().unwrap();
        let blob = head.tree().unwrap().get_name("a.log").unwrap().id();
        let content = com.history().repo().find_blob(blob).unwrap();
        assert_eq!(content.content(), b"new");
    }

    #[test]
    fn nested_repositories_follow_the_setting() {
        let (dir, state) = setup();
        fs::create_dir_all(dir.path().join("sub/.git")).unwrap();
        {
            let (rec, _, _) = open(&state, dir.path(), context(), null()).unwrap();
            let recorded = rec
                .record(
                    Trigger::Close,
                    CALLER,
                    vec![write(dir.path(), "sub/file", "1")],
                )
                .unwrap();
            assert_eq!(recorded.seq, None);
            assert!(matches!(
                recorded.skipped[0].reason,
                SkipReason::Nested { .. }
            ));
        }
        git2::Config::open(&state.git().join("config"))
            .unwrap()
            .set_str("agentfs.nested", "working-tree")
            .unwrap();
        let (rec, com, _) = open(&state, dir.path(), context(), null()).unwrap();
        let recorded = rec
            .record(
                Trigger::Close,
                CALLER,
                vec![write(dir.path(), "sub/file", "1")],
            )
            .unwrap();
        assert_eq!(recorded.seq, Some(0));
        com.commit_pending().unwrap();
        assert_eq!(tree_paths(&com), ["sub/file"]);
    }

    #[test]
    fn removal_waits_for_an_in_flight_first_write() {
        use std::io::Write as _;
        use std::sync::mpsc;
        use std::time::Duration;

        let (dir, state) = setup();
        let (rec, com, _) = open(&state, dir.path(), context(), null()).unwrap();
        // Freezing from a FIFO blocks until it is written and closed, which
        // stands in for a large file whose copy takes a while.
        let fifo = dir.path().join("fifo");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
        let writer = {
            let rec = rec.clone();
            let fifo = fifo.clone();
            std::thread::spawn(move || {
                rec.record(
                    Trigger::Close,
                    CALLER,
                    vec![Input::Write {
                        path: RepoPath::new("out.bin").unwrap(),
                        backing: fifo,
                        executable: false,
                    }],
                )
            })
        };
        // Opening the write end returns once the freeze has opened the read
        // end, so the first write is in flight from here on.
        let mut content = fs::OpenOptions::new().write(true).open(&fifo).unwrap();

        let (tx, rx) = mpsc::channel();
        let remover = {
            let rec = rec.clone();
            std::thread::spawn(move || {
                let r = rec.record(Trigger::Unlink, CALLER, vec![remove("out.bin")]);
                tx.send(()).unwrap();
                r
            })
        };
        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "the removal must wait for the write"
        );

        content.write_all(b"payload").unwrap();
        drop(content);
        let written = writer.join().unwrap().unwrap();
        let removed = remover.join().unwrap().unwrap();
        assert_eq!(written.seq, Some(0));
        assert_eq!(
            removed,
            Recorded {
                seq: Some(1),
                skipped: vec![],
            }
        );
        com.commit_pending().unwrap();
        assert!(tree_paths(&com).is_empty(), "history follows the tree");
    }

    /// Writes `rel` in the backing tree and returns the input a close of it
    /// would produce.
    fn write_in_tree(tree: &Path, rel: &str, content: &str) -> Input {
        let backing = tree.join(rel);
        fs::create_dir_all(backing.parent().unwrap()).unwrap();
        fs::write(&backing, content).unwrap();
        Input::Write {
            path: RepoPath::new(rel).unwrap(),
            backing,
            executable: false,
        }
    }

    /// Replays the boundaries `git init --bare store` produces, in the order
    /// git creates its files, and returns every record's outcome.
    fn replay_git_init_bare(rec: &Recorder, tree: &Path, template: bool) -> Vec<Recorded> {
        let mut out = Vec::new();
        let close = |out: &mut Vec<Recorded>, rel: &str, content: &str| {
            let input = write_in_tree(tree, rel, content);
            out.push(rec.record(Trigger::Close, CALLER, vec![input]).unwrap());
        };
        if template {
            close(
                &mut out,
                "store/description",
                "Unnamed repository; edit this file 'description' to name the repository.\n",
            );
            close(&mut out, "store/hooks/applypatch-msg.sample", "#!/bin/sh\n");
            close(&mut out, "store/hooks/pre-commit.sample", "#!/bin/sh\n");
            close(&mut out, "store/info/exclude", "# git ls-files --others\n");
        }
        fs::create_dir_all(tree.join("store/refs/heads")).unwrap();
        fs::create_dir_all(tree.join("store/refs/tags")).unwrap();
        for (name, content) in [
            ("HEAD", "ref: refs/heads/main\n"),
            (
                "config",
                "[core]\n\trepositoryformatversion = 0\n\tbare = true\n",
            ),
        ] {
            let lock = format!("store/{name}.lock");
            close(&mut out, &lock, content);
            fs::rename(tree.join(&lock), tree.join("store").join(name)).unwrap();
            let input = write_in_tree(tree, &format!("store/{name}"), content);
            out.push(
                rec.record(Trigger::Rename, CALLER, vec![remove(&lock), input])
                    .unwrap(),
            );
        }
        fs::create_dir_all(tree.join("store/objects/pack")).unwrap();
        // Later configuration, such as `git remote add`.
        close(
            &mut out,
            "store/config",
            "[core]\n\trepositoryformatversion = 0\n\tbare = true\n\
             [remote \"origin\"]\n\turl = https://token@example.com/r.git\n",
        );
        out
    }

    #[test]
    fn a_bare_repository_being_created_is_never_recorded() {
        for (nested, template) in [
            ("exclude", true),
            ("exclude", false),
            ("working-tree", true),
            ("working-tree", false),
        ] {
            let (dir, state) = setup();
            git2::Config::open(&state.git().join("config"))
                .unwrap()
                .set_str("agentfs.nested", nested)
                .unwrap();
            let (rec, com, _) = open(&state, dir.path(), context(), null()).unwrap();
            for recorded in replay_git_init_bare(&rec, dir.path(), template) {
                assert_eq!(recorded.seq, None, "{nested}, template {template}");
                for s in &recorded.skipped {
                    assert!(
                        matches!(&s.reason, SkipReason::Nested { repo, .. } if repo.as_str() == "store")
                            || s.reason == SkipReason::Untracked,
                        "{nested}, template {template}: {s:?}"
                    );
                }
            }
            assert_eq!(com.backlog(), 0);
        }
    }

    #[test]
    fn never_record_skips_writes_but_not_removals_of_tracked_paths() {
        let (dir, state) = setup();
        {
            let (rec, com, _) = open(&state, dir.path(), context(), null()).unwrap();
            rec.record(
                Trigger::Close,
                CALLER,
                vec![write(dir.path(), "secret", "1")],
            )
            .unwrap();
            com.commit_pending().unwrap();
        }
        fs::write(
            state.git().join("info").join("attributes"),
            "secret -agentfs\n",
        )
        .unwrap();
        let (rec, com, _) = open(&state, dir.path(), context(), null()).unwrap();
        let recorded = rec
            .record(
                Trigger::Close,
                CALLER,
                vec![write(dir.path(), "secret", "2")],
            )
            .unwrap();
        assert_eq!(
            recorded,
            Recorded {
                seq: None,
                skipped: vec![skipped("secret", SkipReason::NeverRecord)],
            }
        );
        assert_eq!(fs::read_dir(state.pending()).unwrap().count(), 0);

        let recorded = rec
            .record(Trigger::Unlink, CALLER, vec![remove("secret")])
            .unwrap();
        assert_eq!(recorded.seq, Some(1));
        assert!(recorded.skipped.is_empty());
        com.commit_pending().unwrap();
        assert!(tree_paths(&com).is_empty());
    }

    #[test]
    fn symlinks_go_through_the_rules() {
        let (dir, state) = setup();
        exclude(&state, "*.log\n");
        let (rec, com, _) = open(&state, dir.path(), context(), null()).unwrap();
        let recorded = rec
            .record(
                Trigger::Symlink,
                CALLER,
                vec![Input::Symlink {
                    path: RepoPath::new("latest.log").unwrap(),
                    target: "run-1.txt".into(),
                }],
            )
            .unwrap();
        assert_eq!(recorded.seq, None);
        assert!(matches!(
            recorded.skipped[0].reason,
            SkipReason::Ignored {
                source: RuleSource::OperatorExclude,
                ..
            }
        ));
        assert_eq!(com.backlog(), 0);
    }

    #[test]
    fn respect_tree_rules_comes_from_the_volume_config() {
        let (dir, state) = setup();
        fs::write(dir.path().join(".gitignore"), "*.tmp\n").unwrap();
        {
            let (rec, _, _) = open(&state, dir.path(), context(), null()).unwrap();
            let recorded = rec
                .record(
                    Trigger::Close,
                    CALLER,
                    vec![write(dir.path(), "a.tmp", "1")],
                )
                .unwrap();
            assert_eq!(recorded.seq, Some(0), "tree rules are off by default");
        }
        git2::Config::open(&state.git().join("config"))
            .unwrap()
            .set_bool("agentfs.respectTreeRules", true)
            .unwrap();
        let (rec, _, _) = open(&state, dir.path(), context(), null()).unwrap();
        let recorded = rec
            .record(
                Trigger::Close,
                CALLER,
                vec![write(dir.path(), "b.tmp", "1")],
            )
            .unwrap();
        assert_eq!(recorded.seq, None);
        assert_eq!(
            recorded.skipped[0].reason,
            SkipReason::Ignored {
                source: RuleSource::TreeIgnore(RepoPath::new(".gitignore").unwrap()),
                pattern: "*.tmp".into(),
            }
        );
    }
}
