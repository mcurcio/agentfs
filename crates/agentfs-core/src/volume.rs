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

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use opentelemetry::KeyValue;

use crate::checks::{Checks, Limits, Refusal};
use crate::claims::PathClaims;
use crate::config::{ConfigError, validate_name};
use crate::events::{CheckStage, Event, Events};
use crate::history::{Committed, History, HistoryError};
use crate::identity::{Context, IdentityRenderer};
use crate::journal::{Journal, OpenReport};
use crate::nested;
use crate::operations::{ActorBusy, OperationGuard, Operations};
use crate::paths::{PathError, RepoPath};
use crate::pending::{PendingId, PendingStore};
use crate::record::{Caller, Change, Draft, Operation, OperationKind, Record, Trigger};
use crate::rules::{Decision, Rules, RulesError, SkipReason};
use crate::telemetry::{Telemetry, spans};
use crate::tracked::Tracked;

/// The control socket's name in `meta/control`.
pub const CONTROL_SOCKET: &str = "agentfs.sock";

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

    /// Paths the FUSE layer could not record, kept for the next mount to
    /// repair (design §3.5).
    pub fn repairs(&self) -> PathBuf {
        self.meta().join("repairs")
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

    /// Where a running mount listens for restores (design §2.2, §15),
    /// readable only by the daemon's user.
    pub fn control_dir(&self) -> PathBuf {
        self.meta().join("control")
    }

    /// The control socket in [`StateDir::control_dir`].
    pub fn control_socket(&self) -> PathBuf {
        self.control_dir().join(CONTROL_SOCKET)
    }

    /// The state-area lock (design §2.3, [`crate::lock`]).
    pub fn lock_file(&self) -> PathBuf {
        self.meta().join("lock")
    }

    /// Large-file objects (design §7).
    pub fn lfs(&self) -> PathBuf {
        self.root.join("lfs")
    }

    /// The apparent size in bytes (the sum of regular files' lengths) of
    /// each part of the state area: `git`, `journal`, `pending`, `lfs` and
    /// `meta` (design §13). Symlinks are not followed; a missing part, and
    /// files that vanish while it is walked, count 0.
    pub fn usage(&self) -> Vec<(&'static str, u64)> {
        [
            ("git", self.git()),
            ("journal", self.root.join("journal")),
            ("pending", self.pending()),
            ("lfs", self.lfs()),
            ("meta", self.meta()),
        ]
        .into_iter()
        .map(|(part, dir)| (part, apparent_size(&dir)))
        .collect()
    }
}

/// The sum of the lengths of the regular files under `dir`, without
/// following symlinks. Anything unreadable counts 0.
fn apparent_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
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
    #[error(transparent)]
    ActorBusy(#[from] ActorBusy),
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

/// Opens an existing state area for the backing tree at `tree`.
///
/// Alarms go to `events`, which the caller has already wrapped with
/// [`Telemetry::sink`] if telemetry is on; spans, `agentfs.record.duration`,
/// `agentfs.commit.count` and the `agentfs.journal.backlog` gauge go to
/// `telemetry` (design §13). Open a volume once per process: each open
/// registers another backlog gauge.
pub fn open(
    state: &StateDir,
    tree: &Path,
    context: Context,
    events: Events,
    telemetry: &Telemetry,
) -> Result<(Recorder, Committer, Recovery), VolumeError> {
    let history = History::open(&state.git())?;
    // A bad remote section only stops that remote's pushes (design §11.1).
    for bad in &history.settings().invalid_remotes {
        events.emit(Event::RemoteMisconfigured {
            remote: bad.name.clone(),
            error: bad.error.clone(),
        });
    }
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

    let rules = Arc::new(Rules::load(&state.git(), tree, history.settings().into())?);
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

    let operations = Arc::new(Operations::default());
    let journal = Arc::new(journal);
    telemetry.observe_journal(Arc::downgrade(&journal));
    let recovery = Recovery {
        journal: report,
        committed_through,
        uncommitted: uncommitted.len(),
        pruned_pending,
    };
    let recorder = Recorder {
        journal: journal.clone(),
        operations: operations.clone(),
        pending: pending.clone(),
        rules: rules.clone(),
        tracked: Arc::new(tracked),
        claims: Arc::new(PathClaims::default()),
        checks: checks.clone(),
        events: events.clone(),
        telemetry: telemetry.clone(),
    };
    let committer = Committer {
        history,
        journal,
        operations,
        pending,
        identities,
        context,
        checks,
        rules,
        tree_known: true,
        events,
        telemetry: telemetry.clone(),
    };
    Ok((recorder, committer, recovery))
}

/// What [`inspect`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspection {
    /// The branch head, if the branch exists.
    pub head: Option<git2::Oid>,
    /// Last sequence number already in history.
    pub committed_through: Option<u64>,
    /// Journaled records not yet in history.
    pub backlog: usize,
    /// Bytes past the last valid journal frame: a torn tail the next
    /// [`open`] will discard, or a record being appended right now.
    pub torn_bytes: u64,
}

/// Reads a state area without changing it, for `agentfs status`.
///
/// Unlike [`open`], this runs no recovery: it never truncates the journal or
/// prunes frozen copies, so it is safe while another process holds the
/// state-area lock and is writing (design §2.3). A record being appended at
/// that moment may be missed, or counted in [`Inspection::torn_bytes`].
pub fn inspect(state: &StateDir) -> Result<(History, Inspection), VolumeError> {
    let history = History::open(&state.git())?;
    let head = history.head()?.map(|c| c.id());
    let committed_through = history.head_seq()?;
    let (records, torn_bytes) = crate::journal::read_records(&state.journal())?;
    let backlog = records
        .iter()
        .filter(|r| committed_through.is_none_or(|c| r.seq > c))
        .count();
    Ok((
        history,
        Inspection {
            head,
            committed_through,
            backlog,
            torn_bytes,
        },
    ))
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
    operations: Arc<Operations>,
    pending: Arc<PendingStore>,
    rules: Arc<Rules>,
    tracked: Arc<Tracked>,
    claims: Arc<PathClaims>,
    checks: Arc<Checks>,
    events: Events,
    telemetry: Telemetry,
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
        let start = Instant::now();
        // A record by an operation's actor is part of that operation (design
        // §15): trigger `restore`, made by the operator who asked for it.
        let actor = caller.pid;
        let operation = self.operations.of_actor(actor);
        let (trigger, caller) = match &operation {
            Some(op) => (Trigger::Restore, op.caller),
            None => (trigger, caller),
        };
        let in_operation = operation.is_some();
        let span = self.telemetry.span(spans::RECORD, || {
            vec![
                KeyValue::new("agentfs.trigger", trigger.as_str()),
                KeyValue::new("agentfs.inputs", inputs.len() as i64),
                KeyValue::new("agentfs.caller.uid", i64::from(caller.uid)),
                KeyValue::new("process.pid", i64::from(caller.pid)),
            ]
        });
        let paths: Vec<RepoPath> = inputs.iter().map(|i| i.path().clone()).collect();
        let result = self.record_inner(trigger, caller, inputs, operation);
        match &result {
            Ok(recorded) => {
                if let Some(seq) = recorded.seq {
                    span.set(KeyValue::new("agentfs.seq", seq as i64));
                    if in_operation {
                        self.operations.recorded(actor, seq);
                    }
                }
                span.set(KeyValue::new(
                    "agentfs.skipped",
                    recorded.skipped.len() as i64,
                ));
            }
            Err(e) => {
                span.error(e);
                // Inside the span, so the alarm log carries its trace.
                self.events.emit(Event::RecordFailed {
                    trigger,
                    paths,
                    error: e.to_string(),
                });
            }
        }
        self.telemetry.record_duration(trigger, start.elapsed());
        span.end();
        result
    }

    fn record_inner(
        &self,
        trigger: Trigger,
        caller: Caller,
        inputs: Vec<Input>,
        operation: Option<Operation>,
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
                        if let Some(refusal) = self
                            .checks
                            .check_path(&path)
                            .or_else(|| self.checks.check_backing_size(&backing))
                        {
                            self.refuse(&mut skipped, path, refusal);
                            continue;
                        }
                        let (id, _) = self.pending.freeze(&backing)?;
                        frozen.push(id);
                        if let Some(refusal) = self.checks.check_file(
                            &path,
                            &self.pending.path(id),
                            CheckStage::Record,
                        )? {
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
                operation,
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

    /// Starts an operator operation (design §15). Until the guard is
    /// finished or dropped, every record whose caller pid is one of
    /// `actors` is part of it: trigger `restore`, `caller` as its author,
    /// and the operation in the journal record, so the committer gives it
    /// its own commit. Fails if an actor is already part of one.
    pub fn begin_operation(
        &self,
        kind: OperationKind,
        source: String,
        summary: String,
        caller: Caller,
        actors: &[u32],
    ) -> Result<OperationGuard, VolumeError> {
        let op = Operation {
            id: self.operations.next_id(now_ms()),
            kind,
            source,
            summary,
            caller,
        };
        Ok(self.operations.begin(op, actors)?)
    }

    /// Why a write at `path` (a regular file, or a symlink to
    /// `symlink_target`) would not be recorded now, if it would not: the
    /// path rules, given whether the path is tracked (design §4, §5), and
    /// the checks on the path and symlink target (§3.4). A file's content
    /// is checked with [`Checks::check_blob`].
    pub fn would_record(
        &self,
        path: &RepoPath,
        symlink_target: Option<&str>,
    ) -> Result<Option<SkipReason>, VolumeError> {
        if let Decision::Skip(reason) = self.rules.decide(path, self.tracked.contains(path))? {
            return Ok(Some(reason));
        }
        let refusal = match symlink_target {
            Some(target) => self.checks.check_symlink(path, target),
            None => self.checks.check_path(path),
        };
        Ok(refusal.map(SkipReason::Refused))
    }

    /// The pre-commit checks in force.
    pub fn checks(&self) -> &Checks {
        &self.checks
    }

    /// The last sequence number given to a record, journaled or in flight.
    pub fn last_seq(&self) -> Option<u64> {
        self.journal.last_seq()
    }

    /// Waits up to `timeout` until every record up to `seq` is in history
    /// (or failed and was never journaled), including records still being
    /// written to the journal when it is called. Returns whether they all
    /// are.
    pub fn wait_committed(&self, seq: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.journal.settled_through(seq) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
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
    operations: Arc<Operations>,
    pending: Arc<PendingStore>,
    identities: IdentityRenderer,
    context: Context,
    checks: Arc<Checks>,
    rules: Arc<Rules>,
    /// Whether `rules.tree()` is the real backing tree.
    tree_known: bool,
    events: Events,
    telemetry: Telemetry,
}

/// What one commit pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CommitReport {
    pub records: usize,
    pub commits: usize,
    pub last_seq: Option<u64>,
    /// Changes dropped by the commit-time checks (design §3.4).
    pub refused: usize,
    /// Writes dropped because their directory has since become a git
    /// directory (design §5).
    pub git_dir_dropped: usize,
}

impl Committer {
    pub fn history(&self) -> &History {
        &self.history
    }

    /// Records durable but not yet in history.
    pub fn backlog(&self) -> usize {
        self.journal.backlog()
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
    /// first record's time plus the commit window. An operation's
    /// group is left alone while the operation runs and settles as soon as
    /// it ends. A group whose first
    /// record is later than `now_ms` (the clock stepped back) counts as
    /// settled, so a clock step cannot stall commits.
    pub fn commit_settled(&self, now_ms: u64) -> Result<CommitReport, VolumeError> {
        let records = self.journal.uncommitted();
        let window_ms = self.window_ms();
        let mut groups = group_by_window(&records, window_ms);
        if let Some(last) = groups.last() {
            let first = last[0].draft.time_ms;
            let settled = match &last[0].draft.operation {
                // An operation's group grows until it ends (design §15).
                Some(op) => !self.operations.is_active(op.id),
                None => now_ms > first.saturating_add(window_ms) || now_ms < first,
            };
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
            let span = self.telemetry.span(spans::COMMIT, || {
                let seq = |r: Option<&Record>| r.map_or(0, |r| r.seq as i64);
                vec![
                    KeyValue::new("agentfs.seq.first", seq(group.first())),
                    KeyValue::new("agentfs.seq.last", seq(group.last())),
                    KeyValue::new("agentfs.records", group.len() as i64),
                ]
            });
            let checked = match self.recheck(group) {
                Ok(rechecked) => {
                    report.refused += rechecked.refused;
                    report.git_dir_dropped += rechecked.git_dir_dropped;
                    span.set(KeyValue::new("agentfs.refused", rechecked.refused as i64));
                    if rechecked.git_dir_dropped > 0 {
                        span.set(KeyValue::new(
                            "agentfs.git_dir_dropped",
                            rechecked.git_dir_dropped as i64,
                        ));
                    }
                    rechecked.records
                }
                Err(e) => {
                    span.error(&e);
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
                    match commit {
                        Some(id) => {
                            self.telemetry.commit_made();
                            span.set_with(|| KeyValue::new("agentfs.commit.id", id.to_string()));
                        }
                        None => span.set(KeyValue::new("agentfs.commit.empty", true)),
                    }
                }
                Err(e) => {
                    span.error(&e);
                    result = Err(e.into());
                    break;
                }
            }
            span.end();
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
    ///
    /// With the backing tree known ([`Committer::set_tree_known`]), it
    /// also drops, silently, every write or symlink at a path new to
    /// history whose ancestor directory has since become a git directory
    /// (a bare layout, design §5): git's own data written by a custom
    /// template before `HEAD` existed, which the recorder could not yet
    /// tell from ordinary files.
    fn recheck(&self, group: &[Record]) -> Result<Rechecked, VolumeError> {
        let mut rechecked = Rechecked {
            records: Vec::with_capacity(group.len()),
            refused: 0,
            git_dir_dropped: 0,
        };
        let head = self.history.head()?;
        let head_tree = head
            .as_ref()
            .map(|c| c.tree())
            .transpose()
            .map_err(HistoryError::from)?;
        let mut git_dirs: HashMap<String, bool> = HashMap::new();
        for record in group {
            let mut record = record.clone();
            let mut kept = Vec::with_capacity(record.draft.changes.len());
            for change in record.draft.changes {
                let refusal = match &change {
                    Change::Write { path, pending, .. } => self.checks.check_file(
                        path,
                        &self.pending.path(*pending),
                        CheckStage::Commit,
                    )?,
                    Change::Symlink { path, target } => self.checks.check_symlink(path, target),
                    Change::Remove { .. } => None,
                };
                if let Some(reason) = refusal {
                    rechecked.refused += 1;
                    self.events.emit(Event::PathRefused {
                        path: change.path().to_string(),
                        reason,
                        stage: CheckStage::Commit,
                    });
                    continue;
                }
                if self.tree_known
                    && !matches!(change, Change::Remove { .. })
                    && !in_tree(head_tree.as_ref(), change.path())?
                    && self.under_git_dir(change.path(), &mut git_dirs)?
                {
                    rechecked.git_dir_dropped += 1;
                    continue;
                }
                kept.push(change);
            }
            record.draft.changes = kept;
            rechecked.records.push(record);
        }
        Ok(rechecked)
    }

    /// Whether a proper ancestor directory of `path` (never the root) is a
    /// git directory in the backing tree, caching each directory's answer
    /// in `cache` for one pass.
    fn under_git_dir(
        &self,
        path: &RepoPath,
        cache: &mut HashMap<String, bool>,
    ) -> Result<bool, VolumeError> {
        let components: Vec<&str> = path.components().collect();
        let mut prefix = String::new();
        for component in &components[..components.len() - 1] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            let bare = match cache.get(&prefix) {
                Some(bare) => *bare,
                None => {
                    let bare = nested::detect(self.rules.tree(), &prefix)?
                        == Some(nested::NestedKind::Bare);
                    cache.insert(prefix.clone(), bare);
                    bare
                }
            };
            if bare {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether the backing tree this volume was opened with is the real
    /// one, so the commit-time recheck of nested git directories can probe
    /// it (default `true`). `flush` and `run` without an explicit `--root`
    /// do not know the tree and turn it off.
    pub fn set_tree_known(&mut self, known: bool) {
        self.tree_known = known;
    }
}

/// What [`Committer::recheck`] kept and dropped.
struct Rechecked {
    records: Vec<Record>,
    refused: usize,
    git_dir_dropped: usize,
}

/// Whether `path` is an entry of `tree`.
fn in_tree(tree: Option<&git2::Tree<'_>>, path: &RepoPath) -> Result<bool, VolumeError> {
    let Some(tree) = tree else {
        return Ok(false);
    };
    match tree.get_path(Path::new(path.as_str())) {
        Ok(_) => Ok(true),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(false),
        Err(e) => Err(HistoryError::from(e).into()),
    }
}

/// Splits records into runs where each record is within `window_ms` of the
/// run's first record. Records of an operator operation (design §15) form
/// runs of their own, however long the operation takes; workload records
/// interleaved with them (a live restore) split them, since commits
/// follow the journal's order.
fn group_by_window(records: &[Record], window_ms: u64) -> Vec<&[Record]> {
    let op = |r: &Record| r.draft.operation.as_ref().map(|o| o.id);
    let mut groups = Vec::new();
    let mut start = 0;
    for i in 1..records.len() {
        let first = &records[start];
        let split = if op(&records[i]) != op(first) {
            true
        } else if op(first).is_some() {
            false
        } else {
            records[i].draft.time_ms.saturating_sub(first.draft.time_ms) > window_ms
        };
        if split {
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
        let (rec, com, recovery) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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
            let (rec, com, _) =
                open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
            rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "a", "1")])
                .unwrap();
            com.commit_pending().unwrap();
            rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "b", "2")])
                .unwrap();
            // Crash: the second record is durable but never committed.
        }
        let (rec, com, recovery) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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
            let (rec, com, _) =
                open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
            rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "a", "1")])
                .unwrap();
            // Commit without letting the committer truncate: simulates a crash
            // between the ref update and journal truncation.
            let records = com.journal.uncommitted();
            com.history
                .commit(&records, &com.pending, &com.identities, &com.context)
                .unwrap();
        }
        let (_, com, recovery) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
        assert_eq!(recovery.committed_through, Some(0));
        assert_eq!(recovery.uncommitted, 0);
        assert_eq!(recovery.pruned_pending, 1);
        assert_eq!(com.commit_pending().unwrap(), CommitReport::default());
        let head = com.history().head().unwrap().unwrap();
        assert_eq!(head.parent_count(), 0, "no duplicate commit");
    }

    #[test]
    fn inspect_reads_without_recovery() {
        let (dir, state) = setup();
        let (_, i) = inspect(&state).unwrap();
        assert_eq!(
            i,
            Inspection {
                head: None,
                committed_through: None,
                backlog: 0,
                torn_bytes: 0
            }
        );
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
        rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "a", "1")])
            .unwrap();
        com.commit_pending().unwrap();
        rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "b", "2")])
            .unwrap();
        // A copy frozen for a record that is not journaled yet: open would
        // prune it, inspect must not.
        let stray = state.pending().join(format!("{:016x}", 999));
        fs::write(&stray, "in flight").unwrap();

        let (history, i) = inspect(&state).unwrap();
        assert_eq!(i.head, Some(history.head().unwrap().unwrap().id()));
        assert_eq!(i.committed_through, Some(0));
        assert_eq!(i.backlog, 1);
        assert!(stray.exists(), "inspect pruned a pending copy");
        drop((rec, com));
    }

    #[test]
    fn failed_record_leaves_no_frozen_copy() {
        let (dir, state) = setup();
        let sink = Arc::new(MemorySink::new());
        let (rec, _, _) = open(
            &state,
            dir.path(),
            context(),
            sink.clone(),
            &Telemetry::noop(),
        )
        .unwrap();
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
        let (rec, com, _) = open(
            &state,
            dir.path(),
            context(),
            sink.clone(),
            &Telemetry::noop(),
        )
        .unwrap();
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
        let (rec, com, _) = open(
            &state,
            dir.path(),
            context(),
            sink.clone(),
            &Telemetry::noop(),
        )
        .unwrap();
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

    #[test]
    fn a_large_rule_file_is_scanned_at_commit_time() {
        let (dir, state) = setup();
        let sink = Arc::new(MemorySink::new());
        let (rec, com, _) = open(
            &state,
            dir.path(),
            context(),
            sink.clone(),
            &Telemetry::noop(),
        )
        .unwrap();
        let attrs = format!(
            "{}{}",
            "* text\n".repeat((crate::checks::SYNC_SCAN_LIMIT as usize) / 7 + 1),
            "x".repeat(3000)
        );
        let recorded = rec
            .record(
                Trigger::Close,
                CALLER,
                vec![
                    write(dir.path(), ".gitattributes", &attrs),
                    write(dir.path(), "ok", "1"),
                ],
            )
            .unwrap();
        assert_eq!(recorded.seq, Some(0));
        assert!(recorded.skipped.is_empty(), "{:?}", recorded.skipped);
        assert!(sink.take().is_empty());

        let report = com.commit_pending().unwrap();
        assert_eq!(report.refused, 1);
        assert_eq!(tree_paths(&com), ["ok"]);
        let events = sink.take();
        assert!(
            matches!(
                &events[..],
                [Event::PathRefused {
                    path,
                    stage: CheckStage::Commit,
                    reason: Refusal::GitattributesLineTooLong { .. },
                }] if path == ".gitattributes"
            ),
            "{events:?}"
        );
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
            let (rec, com, _) =
                open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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
        let (rec, com, _) = open(
            &state,
            dir.path(),
            context(),
            sink.clone(),
            &Telemetry::noop(),
        )
        .unwrap();
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

    /// The limit is checked on the live file before the freeze copies it.
    /// The backing file is unreadable, so a freeze would fail the record.
    #[test]
    fn oversized_file_is_refused_before_the_freeze() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, state) = setup();
        set_remote_limit(&state, "r", 5);
        let sink = Arc::new(MemorySink::new());
        let (rec, com, _) = open(
            &state,
            dir.path(),
            context(),
            sink.clone(),
            &Telemetry::noop(),
        )
        .unwrap();
        let input = write(dir.path(), "big", "0123456789");
        let Input::Write { backing, .. } = &input else {
            unreachable!()
        };
        fs::set_permissions(backing, fs::Permissions::from_mode(0o000)).unwrap();
        let recorded = rec.record(Trigger::Close, CALLER, vec![input]).unwrap();
        assert_eq!(recorded.seq, None);
        assert_eq!(
            recorded.skipped[0].reason,
            SkipReason::Refused(Refusal::BlobTooLarge {
                bytes: 10,
                limit: 5,
                remote: "r".into()
            })
        );
        assert_eq!(fs::read_dir(state.pending()).unwrap().count(), 0);
        assert_eq!(com.backlog(), 0);
        assert_eq!(sink.take().len(), 1);
    }

    /// A bad remote section is reported and left out; the volume still
    /// records, and the valid remotes keep their limits.
    #[test]
    fn invalid_remote_does_not_stop_the_volume() {
        let (dir, state) = setup();
        set_remote_limit(&state, "r", 5);
        {
            let mut config = git2::Config::open(&state.git().join("config")).unwrap();
            config
                .set_str("remote.team/backup.url", "/nowhere")
                .unwrap();
            config.set_str("remote.rel.url", "/nowhere").unwrap();
            config
                .set_str("remote.rel.agentfsCredentials", "token")
                .unwrap();
        }
        let sink = Arc::new(MemorySink::new());
        let (rec, com, _) = open(
            &state,
            dir.path(),
            context(),
            sink.clone(),
            &Telemetry::noop(),
        )
        .unwrap();
        let events = sink.take();
        let names: Vec<&str> = events
            .iter()
            .map(|e| match e {
                Event::RemoteMisconfigured { remote, .. } => remote.as_str(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(names, ["rel", "team/backup"]);
        let recorded = rec
            .record(
                Trigger::Close,
                CALLER,
                vec![
                    write(dir.path(), "ok", "1"),
                    write(dir.path(), "big", "0123456789"),
                ],
            )
            .unwrap();
        assert_eq!(recorded.seq, Some(0));
        assert_eq!(recorded.skipped.len(), 1);
        com.commit_pending().unwrap();
        assert_eq!(tree_paths(&com), ["ok"]);
    }

    #[test]
    fn commit_time_recheck() {
        let (dir, state) = setup();
        {
            let (rec, com, _) =
                open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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
        let (_, com, recovery) = open(
            &state,
            dir.path(),
            context(),
            sink.clone(),
            &Telemetry::noop(),
        )
        .unwrap();
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
            let (rec, _, _) =
                open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
            rec.record(
                Trigger::Close,
                CALLER,
                vec![write(dir.path(), "big", "0123456789")],
            )
            .unwrap();
        }
        set_remote_limit(&state, "r", 5);
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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
                operation: None,
            })
            .unwrap();
        rec.tracked.apply(&change);
    }

    #[test]
    fn commit_settled_leaves_the_open_group() {
        let (dir, state) = setup();
        set_window(&state, "200ms");
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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

    const OPERATOR: Caller = Caller {
        uid: 0,
        gid: 0,
        pid: 4242,
    };

    fn begin(rec: &Recorder, actors: &[u32]) -> OperationGuard {
        rec.begin_operation(
            OperationKind::Restore,
            "0123456789abcdef0123456789abcdef01234567".into(),
            "restore a from 0123456".into(),
            OPERATOR,
            actors,
        )
        .unwrap()
    }

    fn head_message(com: &Committer) -> String {
        let head = com.history().head().unwrap().unwrap();
        head.message().unwrap().to_owned()
    }

    #[test]
    fn records_by_an_actor_are_part_of_the_operation() {
        let (dir, state) = setup();
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
        let actor = Caller { pid: 77, ..CALLER };
        let guard = begin(&rec, &[77]);
        assert!(matches!(
            rec.begin_operation(
                OperationKind::Revert,
                "x".into(),
                "y".into(),
                OPERATOR,
                &[77]
            ),
            Err(VolumeError::ActorBusy(_))
        ));
        let a = rec
            .record(Trigger::Close, actor, vec![write(dir.path(), "a", "1")])
            .unwrap()
            .seq
            .unwrap();
        let other = rec
            .record(Trigger::Close, CALLER, vec![write(dir.path(), "b", "2")])
            .unwrap()
            .seq
            .unwrap();
        assert_eq!(guard.last_seq(), Some(a));
        let records = com.journal.uncommitted();
        assert_eq!(records[0].draft.trigger, Trigger::Restore);
        assert_eq!(records[0].draft.caller, OPERATOR);
        assert_eq!(
            records[0].draft.operation.as_ref().map(|o| o.id),
            Some(guard.operation().id)
        );
        assert_eq!(records[1].seq, other);
        assert_eq!(records[1].draft.trigger, Trigger::Close);
        assert_eq!(records[1].draft.caller, CALLER);
        assert_eq!(records[1].draft.operation, None);
        assert_eq!(guard.finish(), Some(a));
        // After the operation, the actor is an ordinary caller again.
        rec.record(Trigger::Close, actor, vec![write(dir.path(), "c", "3")])
            .unwrap();
        assert_eq!(com.journal.uncommitted()[2].draft.operation, None);
    }

    #[test]
    fn an_operation_is_its_own_commit() {
        let (dir, state) = setup();
        set_window(&state, "10s");
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
        let t = 1_700_000_000_000;
        let actor = Caller { pid: 77, ..CALLER };
        record_at(&rec, dir.path(), "before", t);
        let guard = begin(&rec, &[77]);
        rec.record(Trigger::Close, actor, vec![write(dir.path(), "a", "1")])
            .unwrap();
        rec.record(Trigger::Unlink, actor, vec![remove("before")])
            .unwrap();
        // Held while the operation runs, however long it takes.
        let report = com.commit_settled(now_ms() + 60_000).unwrap();
        assert_eq!((report.records, report.commits), (1, 1), "only `before`");
        assert_eq!(com.backlog(), 2);
        guard.finish();
        let report = com.commit_settled(now_ms()).unwrap();
        assert_eq!((report.records, report.commits), (2, 1), "settled at once");
        assert_eq!(tree_paths(&com), ["a"]);

        let head = com.history().head().unwrap().unwrap();
        assert_eq!(head.author().name().unwrap(), "uid-0");
        let message = head_message(&com);
        assert!(
            message.starts_with("restore a from 0123456\n\n"),
            "{message}"
        );
        for line in [
            "write   a\n",
            "remove  before\n",
            "Agentfs-Trigger: restore\nAgentfs-Restore: 0123456789abcdef0123456789abcdef01234567\n",
            "Agentfs-Uid: 0\n",
            "Agentfs-Pid: 4242\n",
        ] {
            assert!(message.contains(line), "{line:?} in {message}");
        }
        assert!(!message.contains("Agentfs-Uid: 10000"), "{message}");
    }

    #[test]
    fn grouping_isolates_operations() {
        let (dir, state) = setup();
        let (rec, _com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
        let mut records = Vec::new();
        let t = 1_700_000_000_000;
        let op = |id| Operation {
            id,
            kind: OperationKind::Revert,
            source: "s".into(),
            summary: "revert s".into(),
            caller: OPERATOR,
        };
        for (i, (time, operation)) in [
            (t, None),
            (t + 1, Some(op(1))),
            (t + 2, Some(op(1))),
            (t + 50_000, Some(op(1))),
            (t + 50_001, None),
            (t + 50_002, Some(op(1))),
            (t + 50_003, Some(op(2))),
            (t + 50_004, None),
        ]
        .into_iter()
        .enumerate()
        {
            records.push(Record {
                seq: i as u64,
                draft: Draft {
                    trigger: Trigger::Close,
                    caller: CALLER,
                    time_ms: time,
                    changes: vec![],
                    operation,
                },
            });
        }
        let groups: Vec<Vec<u64>> = group_by_window(&records, 200)
            .iter()
            .map(|g| g.iter().map(|r| r.seq).collect())
            .collect();
        assert_eq!(
            groups,
            [vec![0], vec![1, 2, 3], vec![4], vec![5], vec![6], vec![7]]
        );
        drop(rec);
    }

    #[test]
    fn would_record_follows_rules_and_checks() {
        let (dir, state) = setup();
        fs::write(state.git().join("info/attributes"), "secret -agentfs\n").unwrap();
        exclude(&state, "*.log\n");
        set_remote_limit(&state, "origin", 10);
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
        let p = |s: &str| RepoPath::new(s).unwrap();
        assert_eq!(rec.would_record(&p("a.txt"), None).unwrap(), None);
        assert_eq!(
            rec.would_record(&p("secret"), None).unwrap(),
            Some(SkipReason::NeverRecord)
        );
        assert!(matches!(
            rec.would_record(&p("x.log"), None).unwrap(),
            Some(SkipReason::Ignored { .. })
        ));
        assert!(matches!(
            rec.would_record(&p("l"), Some("a-target-over-ten-bytes"))
                .unwrap(),
            Some(SkipReason::Refused(Refusal::BlobTooLarge { .. }))
        ));
        assert!(matches!(
            rec.would_record(&p(".gitmodules"), Some("x")).unwrap(),
            Some(SkipReason::Refused(Refusal::SymlinkedSpecialFile { .. }))
        ));
        assert!(matches!(
            rec.checks().check_blob(&p("big"), b"more than ten bytes"),
            Some(Refusal::BlobTooLarge { .. })
        ));
        // An ignored path that is tracked is recorded (design §4).
        fs::write(dir.path().join("y.log"), "1").unwrap();
        exclude(&state, "");
        drop((rec, com));
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
        rec.record(
            Trigger::Close,
            CALLER,
            vec![write(dir.path(), "y.log", "1")],
        )
        .unwrap();
        com.commit_pending().unwrap();
        drop((rec, com));
        exclude(&state, "*.log\n");
        let (rec, _com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
        assert_eq!(rec.would_record(&p("y.log"), None).unwrap(), None);
        assert!(rec.would_record(&p("z.log"), None).unwrap().is_some());
    }

    #[test]
    fn wait_committed_waits_for_history() {
        let (dir, state) = setup();
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
        assert_eq!(rec.last_seq(), None);
        rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "a", "1")])
            .unwrap();
        let seq = rec.last_seq().unwrap();
        let start = Instant::now();
        assert!(!rec.wait_committed(seq, Duration::from_millis(50)));
        assert!(start.elapsed() >= Duration::from_millis(50));
        let com = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            com.commit_pending().unwrap();
            com
        });
        assert!(rec.wait_committed(seq, Duration::from_secs(10)));
        drop(com.join().unwrap());
    }

    #[test]
    fn refuse_invalid_emits_an_alarm() {
        let (dir, state) = setup();
        let sink = Arc::new(MemorySink::new());
        let (rec, _, _) = open(
            &state,
            dir.path(),
            context(),
            sink.clone(),
            &Telemetry::noop(),
        )
        .unwrap();
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
                operation: None,
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
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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
            let (rec, _, _) =
                open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
            rec.record(
                Trigger::Close,
                CALLER,
                vec![write(dir.path(), "a.log", "1")],
            )
            .unwrap();
            // Crash before committing.
        }
        exclude(&state, "*.log\n");
        let (rec, com, recovery) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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
            let (rec, com, _) =
                open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
            rec.record(
                Trigger::Close,
                CALLER,
                vec![write(dir.path(), "a.log", "1")],
            )
            .unwrap();
            com.commit_pending().unwrap();
        }
        exclude(&state, "*.log\n");
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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
            let (rec, _, _) =
                open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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

    /// Records `rel` (written in the tree first) as a close.
    fn close_in_tree(rec: &Recorder, tree: &Path, rel: &str, content: &str) -> Recorded {
        let input = write_in_tree(tree, rel, content);
        rec.record(Trigger::Close, CALLER, vec![input]).unwrap()
    }

    /// Turns `dir` into a bare git directory: what `git init --bare` with a
    /// custom template writes after the template's files.
    fn make_head(tree: &Path, dir: &str) {
        fs::create_dir_all(tree.join(dir)).unwrap();
        fs::write(tree.join(dir).join("HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    #[test]
    fn custom_template_files_are_dropped_at_commit_time() {
        let (dir, state) = setup();
        let tree = dir.path().join("tree");
        fs::create_dir_all(&tree).unwrap();
        let (rec, com, _) = open(&state, &tree, context(), null(), &Telemetry::noop()).unwrap();
        // A custom template's files, written before HEAD: nothing marks the
        // directory as a git directory yet, so they are recorded.
        for rel in ["r.git/hooks/custom", "r.git/info/x", "keep.txt"] {
            assert!(close_in_tree(&rec, &tree, rel, "x").seq.is_some(), "{rel}");
        }
        make_head(&tree, "r.git");
        let report = com.commit_pending().unwrap();
        assert_eq!(report.git_dir_dropped, 2);
        assert_eq!(report.refused, 0);
        assert_eq!(tree_paths(&com), ["keep.txt"]);
        assert_eq!(com.backlog(), 0);
    }

    #[test]
    fn the_git_dir_recheck_keeps_history_and_working_trees() {
        let (dir, state) = setup();
        let tree = dir.path().join("tree");
        fs::create_dir_all(&tree).unwrap();
        let (rec, com, _) = open(&state, &tree, context(), null(), &Telemetry::noop()).unwrap();
        // Committed before the directory became a git directory: tracked,
        // so later writes stay recorded (design §5).
        close_in_tree(&rec, &tree, "store/notes", "1");
        com.commit_pending().unwrap();
        make_head(&tree, "store");
        close_in_tree(&rec, &tree, "store/notes", "2");
        // A working tree (`git init` makes `.git`): its files are kept.
        close_in_tree(&rec, &tree, "proj/src.rs", "fn main() {}");
        make_head(&tree, "proj/.git");
        fs::create_dir_all(tree.join("proj/.git/objects")).unwrap();
        fs::create_dir_all(tree.join("proj/.git/refs")).unwrap();
        // Removals are never dropped.
        fs::remove_file(tree.join("store/notes")).unwrap();
        let report = com.commit_pending().unwrap();
        assert_eq!(report.git_dir_dropped, 0);
        assert_eq!(tree_paths(&com), ["proj/src.rs", "store/notes"]);
        assert_eq!(blob(&com, "store/notes"), b"2");
        rec.record(Trigger::Unlink, CALLER, vec![remove("store/notes")])
            .unwrap();
        com.commit_pending().unwrap();
        assert_eq!(tree_paths(&com), ["proj/src.rs"]);
    }

    #[test]
    fn the_git_dir_recheck_is_off_without_a_known_tree() {
        let (dir, state) = setup();
        let tree = dir.path().join("tree");
        fs::create_dir_all(&tree).unwrap();
        let (rec, mut com, _) = open(&state, &tree, context(), null(), &Telemetry::noop()).unwrap();
        com.set_tree_known(false);
        close_in_tree(&rec, &tree, "r.git/hooks/custom", "x");
        make_head(&tree, "r.git");
        let report = com.commit_pending().unwrap();
        assert_eq!(report.git_dir_dropped, 0);
        assert_eq!(tree_paths(&com), ["r.git/hooks/custom"]);
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
            let (rec, com, _) =
                open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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
            let (rec, com, _) =
                open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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
            let (rec, _, _) =
                open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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
        let (rec, _, _) = open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
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

    mod telemetry {
        use opentelemetry::trace::Status;

        use super::*;
        use crate::telemetry::testing::{Harness, any_str, span_attr};
        use crate::telemetry::{names, spans};

        /// Opens the volume with `h`'s telemetry, events wrapped as the CLI
        /// wraps them.
        fn open_traced(state: &StateDir, dir: &Path, h: &Harness) -> (Recorder, Committer) {
            let events = h.telemetry.sink(null());
            let (rec, com, _) = open(state, dir, context(), events, &h.telemetry).unwrap();
            (rec, com)
        }

        #[test]
        fn record_duration_and_span() {
            let (dir, state) = setup();
            let h = Harness::new();
            let (rec, _com) = open_traced(&state, dir.path(), &h);
            let seq = rec
                .record(Trigger::Close, CALLER, vec![write(dir.path(), "a", "1")])
                .unwrap()
                .seq
                .unwrap();
            let skipped = rec
                .record(Trigger::Unlink, CALLER, vec![remove("never-tracked")])
                .unwrap();
            assert_eq!(skipped.seq, None);

            for trigger in ["close", "unlink"] {
                assert_eq!(
                    h.histogram_count(names::RECORD_DURATION, &[("trigger", trigger)]),
                    Some(1),
                    "{trigger}"
                );
            }
            let records = h.spans(spans::RECORD);
            assert_eq!(records.len(), 2);
            assert_eq!(
                span_attr(&records[0], "agentfs.trigger").as_deref(),
                Some("close")
            );
            assert_eq!(span_attr(&records[0], "agentfs.seq"), Some(seq.to_string()));
            assert_eq!(
                span_attr(&records[0], "agentfs.inputs").as_deref(),
                Some("1")
            );
            assert_eq!(
                span_attr(&records[0], "agentfs.caller.uid").as_deref(),
                Some("10000")
            );
            assert_eq!(
                span_attr(&records[0], "agentfs.skipped").as_deref(),
                Some("0")
            );
            assert_eq!(records[0].status, Status::Unset);
            assert_eq!(span_attr(&records[1], "agentfs.seq"), None);
            assert_eq!(
                span_attr(&records[1], "agentfs.skipped").as_deref(),
                Some("1")
            );
        }

        #[test]
        fn journal_backlog_gauge() {
            let (dir, state) = setup();
            {
                let h = Harness::new();
                let (rec, com) = open_traced(&state, dir.path(), &h);
                for name in ["a", "b"] {
                    rec.record(Trigger::Close, CALLER, vec![write(dir.path(), name, "1")])
                        .unwrap();
                }
                assert_eq!(h.gauge_u64(names::JOURNAL_BACKLOG, &[]), Some(2));
                com.commit_pending().unwrap();
                assert_eq!(h.gauge_u64(names::JOURNAL_BACKLOG, &[]), Some(0));
                rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "c", "1")])
                    .unwrap();
                // Crash before committing.
            }
            let h = Harness::new();
            let (rec, com) = open_traced(&state, dir.path(), &h);
            assert_eq!(
                h.gauge_u64(names::JOURNAL_BACKLOG, &[]),
                Some(1),
                "the recovered record"
            );
            drop((rec, com));
            assert_eq!(
                h.gauge_u64(names::JOURNAL_BACKLOG, &[]),
                None,
                "a closed volume reports nothing"
            );
        }

        #[test]
        fn commit_count_and_span() {
            let (dir, state) = setup();
            set_window(&state, "1h");
            let h = Harness::new();
            let (rec, com) = open_traced(&state, dir.path(), &h);
            let seqs: Vec<u64> = ["a", "b"]
                .into_iter()
                .map(|name| {
                    rec.record(Trigger::Close, CALLER, vec![write(dir.path(), name, "1")])
                        .unwrap()
                        .seq
                        .unwrap()
                })
                .collect();
            let report = com.commit_pending().unwrap();
            assert_eq!(report.commits, 1);
            assert_eq!(h.sum(names::COMMIT_COUNT, &[]), Some(1));
            let commits = h.spans(spans::COMMIT);
            assert_eq!(commits.len(), 1);
            let head = com.history().head().unwrap().unwrap().id();
            assert_eq!(
                span_attr(&commits[0], "agentfs.seq.first"),
                Some(seqs[0].to_string())
            );
            assert_eq!(
                span_attr(&commits[0], "agentfs.seq.last"),
                Some(seqs[1].to_string())
            );
            assert_eq!(
                span_attr(&commits[0], "agentfs.records").as_deref(),
                Some("2")
            );
            assert_eq!(
                span_attr(&commits[0], "agentfs.refused").as_deref(),
                Some("0")
            );
            assert_eq!(
                span_attr(&commits[0], "agentfs.commit.id"),
                Some(head.to_string())
            );
            assert_eq!(span_attr(&commits[0], "agentfs.commit.empty"), None);

            // The same content again leaves the tree unchanged: no commit.
            rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "a", "1")])
                .unwrap();
            let report = com.commit_pending().unwrap();
            assert_eq!((report.records, report.commits), (1, 0));
            assert_eq!(h.sum(names::COMMIT_COUNT, &[]), Some(1));
            let commits = h.spans(spans::COMMIT);
            assert_eq!(commits.len(), 2);
            assert_eq!(
                span_attr(&commits[1], "agentfs.commit.empty").as_deref(),
                Some("true")
            );
            assert_eq!(span_attr(&commits[1], "agentfs.commit.id"), None);
        }

        #[test]
        fn failed_record_counts_and_errors_span() {
            let (dir, state) = setup();
            let h = Harness::new();
            let memory = Arc::new(MemorySink::new());
            let (rec, _, _) = open(
                &state,
                dir.path(),
                context(),
                h.telemetry.sink(memory.clone()),
                &h.telemetry,
            )
            .unwrap();
            let missing = Input::Write {
                path: RepoPath::new("b").unwrap(),
                backing: dir.path().join("missing"),
                executable: false,
            };
            rec.record(Trigger::Close, CALLER, vec![missing])
                .unwrap_err();
            assert!(matches!(&memory.take()[..], [Event::RecordFailed { .. }]));
            assert_eq!(
                h.sum(names::RECORD_FAILURES, &[("trigger", "close")]),
                Some(1)
            );
            assert_eq!(
                h.histogram_count(names::RECORD_DURATION, &[("trigger", "close")]),
                Some(1),
                "failed records are timed too"
            );
            let records = h.spans(spans::RECORD);
            assert_eq!(records.len(), 1);
            assert!(
                matches!(&records[0].status, Status::Error { description } if !description.is_empty()),
                "{:?}",
                records[0].status
            );
            let logs = h.logs();
            let failed = logs
                .iter()
                .find(|r| r.event_name() == Some("agentfs.record_failed"))
                .expect("a record_failed log");
            let trace = failed.trace_context().expect("trace context");
            assert_eq!(trace.trace_id, records[0].span_context.trace_id());
            assert_eq!(trace.span_id, records[0].span_context.span_id());
            assert!(
                failed
                    .body()
                    .map(any_str)
                    .is_some_and(|b| b.starts_with("record failed (close) for b")),
                "{:?}",
                failed.body()
            );
        }

        #[test]
        fn precheck_rejections_by_stage() {
            let (dir, state) = setup();
            {
                let h = Harness::new();
                let (rec, com) = open_traced(&state, dir.path(), &h);
                rec.record(
                    Trigger::Symlink,
                    CALLER,
                    vec![Input::Symlink {
                        path: RepoPath::new(".gitmodules").unwrap(),
                        target: "elsewhere".into(),
                    }],
                )
                .unwrap();
                assert_eq!(
                    h.sum(
                        names::PRECHECK_REJECTED_PATHS,
                        &[("stage", "record"), ("reason", "symlinked_special_file")]
                    ),
                    Some(1)
                );
                rec.record(
                    Trigger::Close,
                    CALLER,
                    vec![write(dir.path(), "big", "0123456789")],
                )
                .unwrap();
                drop(com);
                // Not committed; the remount brings a stricter remote.
            }
            set_remote_limit(&state, "r", 5);
            let h = Harness::new();
            let (_, com) = open_traced(&state, dir.path(), &h);
            assert_eq!(com.commit_pending().unwrap().refused, 1);
            assert_eq!(
                h.sum(
                    names::PRECHECK_REJECTED_PATHS,
                    &[("stage", "commit"), ("reason", "blob_too_large")]
                ),
                Some(1)
            );
            let commits = h.spans(spans::COMMIT);
            assert_eq!(
                span_attr(&commits[0], "agentfs.refused").as_deref(),
                Some("1")
            );
            let logs = h.logs();
            assert_eq!(logs.len(), 1);
            assert_eq!(logs[0].event_name(), Some("agentfs.path_refused"));
            assert_eq!(
                logs[0].trace_context().map(|t| t.span_id),
                Some(commits[0].span_context.span_id()),
                "raised inside the commit span"
            );
        }

        #[test]
        fn state_usage_by_part() {
            let (dir, state) = setup();
            let h = Harness::new();
            let (rec, com) = open_traced(&state, dir.path(), &h);
            let settings = com.history().settings().clone();
            h.telemetry.observe_volume(&state, &settings);
            rec.record(
                Trigger::Close,
                CALLER,
                vec![write(dir.path(), "a", "12345")],
            )
            .unwrap();
            let usage: std::collections::HashMap<_, _> = state.usage().into_iter().collect();
            assert_eq!(usage.len(), 5);
            assert_eq!(usage["pending"], 5);
            assert!(usage["journal"] > 0, "{usage:?}");
            assert!(usage["git"] > 0, "{usage:?}");
            assert_eq!(usage["lfs"], 0, "no lfs directory yet");
            assert_eq!(
                h.gauge_u64(names::STATE_BYTES, &[("part", "pending")]),
                Some(5)
            );
            assert_eq!(h.gauge_u64(names::STATE_BYTES, &[("part", "lfs")]), Some(0));

            com.commit_pending().unwrap();
            let usage: std::collections::HashMap<_, _> = state.usage().into_iter().collect();
            assert_eq!(usage["pending"], 0);
            assert_eq!(
                h.gauge_u64(names::STATE_BYTES, &[("part", "pending")]),
                Some(0)
            );
            // Symlinks are not followed.
            std::os::unix::fs::symlink(dir.path(), state.meta().join("loop")).unwrap();
            let before = state.usage();
            assert_eq!(state.usage(), before);
        }
    }
}
