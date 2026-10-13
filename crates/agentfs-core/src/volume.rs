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
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use opentelemetry::KeyValue;

use crate::checks::{Checks, Limits, Refusal};
use crate::claims::PathClaims;
use crate::config::{ConfigError, validate_name};
use crate::events::{CheckStage, Event, Events};
use crate::fsync_class::{self, FsyncClass, GroupMembers};
use crate::history::{Committed, History, HistoryError};
use crate::history_service::{ClientError, FrontLease, HistoryClient};
use crate::identity::{Context, IdentityRenderer};
use crate::journal::{Journal, OpenReport};
use crate::lfs::{ObjectCache, Store, StorePolicy};
use crate::nested;
use crate::operations::{ActorBusy, OperationGuard, Operations};
use crate::paths::{PathError, RepoPath};
use crate::pending::{PendingId, PendingStore};
use crate::record::{
    Caller, Change, Draft, Operation, OperationKind, Origin, PatchAuthor, Record, Trigger,
};
use crate::rules::{Decision, Rules, RulesError, SkipReason};
use crate::telemetry::{Telemetry, spans};
use crate::tracked::Tracked;

/// The control socket's name in `meta/control`.
pub const CONTROL_SOCKET: &str = "agentfs.sock";

/// The history service's default socket name in `meta/history`.
pub const HISTORY_SOCKET: &str = "history.sock";

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

    /// The patch queue's cached state (design §12): `meta/patch.json`.
    pub fn patch_state(&self) -> PathBuf {
        self.meta().join("patch.json")
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

    /// Leases and repair lists of the fronts feeding a history service
    /// (design §2.2, §14).
    pub fn fronts_dir(&self) -> PathBuf {
        self.meta().join("fronts")
    }

    /// The lease a front holds on pending slot `slot`
    /// ([`crate::history_service::FrontLease`]).
    pub fn front_lease(&self, slot: u16) -> PathBuf {
        self.fronts_dir().join(format!("slot-{slot}.lock"))
    }

    /// The paths a front in `slot` could not record, kept for the next
    /// front in that slot to repair (design §3.5); the counterpart of
    /// [`StateDir::repairs`].
    pub fn front_repairs(&self, slot: u16) -> PathBuf {
        self.fronts_dir().join(format!("slot-{slot}.repairs"))
    }

    /// Where the history service listens by default, readable only by the
    /// daemon's user (design §14).
    pub fn history_dir(&self) -> PathBuf {
        self.meta().join("history")
    }

    /// The history service's default socket in [`StateDir::history_dir`].
    pub fn history_socket(&self) -> PathBuf {
        self.history_dir().join(HISTORY_SOCKET)
    }

    /// The state-area lock (design §2.3, [`crate::lock`]).
    pub fn lock_file(&self) -> PathBuf {
        self.meta().join("lock")
    }

    /// Large-file objects (design §7).
    pub fn lfs(&self) -> PathBuf {
        self.root.join("lfs")
    }

    /// Cached LFS objects, `aa/bb/<oid>` as in git-lfs
    /// ([`crate::lfs::ObjectCache`]).
    pub fn lfs_objects(&self) -> PathBuf {
        self.lfs().join("objects")
    }

    /// LFS objects being written.
    pub fn lfs_tmp(&self) -> PathBuf {
        self.lfs().join("tmp")
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
    /// A front could not get its record durable (design §3.5, §14).
    #[error(transparent)]
    HistoryService(#[from] ClientError),
    /// The history service will not journal a front's record as sent.
    #[error("refused a front's record: {0}")]
    FrontRecord(String),
    #[error(
        "this state area belongs to volume {found:?}, not {wanted:?}; nothing was changed \
         (check which volume or PVC is mounted)"
    )]
    OtherVolume { found: String, wanted: String },
    #[error(
        "this state area's branch is {found:?}, not {wanted:?}; nothing was changed (the \
         branch cannot be changed by init)"
    )]
    OtherBranch { found: String, wanted: String },
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

/// What [`init`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Init {
    /// A new state area was created.
    Created,
    /// The state area already belonged to this volume (and branch); missing
    /// directories were re-created and nothing else changed, apart from
    /// the includes when they were given.
    AlreadyInitialised,
}

/// Creates a state area for `volume`, or accepts one that already belongs
/// to it ([`init_with`] without includes).
pub fn init(state: &StateDir, volume: &str, branch: Option<&str>) -> Result<Init, VolumeError> {
    init_with(state, volume, branch, None)
}

/// Creates a state area for `volume`, or accepts an existing one, so it
/// can run at every start of a pod (design §2.3, §17.1).
///
/// - A state area whose config already names `volume` (and `branch`, when
///   given) is [`Init::AlreadyInitialised`]: missing directories are
///   re-created, nothing else is touched.
/// - One that names another volume or branch is refused
///   ([`VolumeError::OtherVolume`], [`VolumeError::OtherBranch`]) and left
///   unchanged: a PVC or ConfigMap mix-up must not repurpose history.
/// - A git directory without `agentfs.volume` (one a container runtime
///   created to mount `info/exclude` into, say) counts as new; files
///   already in `info/` are kept.
///
/// `includes`, when given, become exactly the config's `include.path`
/// entries ([`History::set_includes`]). The result is opened once, so a bad
/// fragment fails here rather than at the first mount. On an existing
/// state area the caller holds the state-area lock.
pub fn init_with(
    state: &StateDir,
    volume: &str,
    branch: Option<&str>,
    includes: Option<&[PathBuf]>,
) -> Result<Init, VolumeError> {
    validate_name("agentfs.volume", volume).map_err(config_error)?;
    let git = state.git();
    let existing = existing_identity(&git)?;
    let outcome = match existing {
        Some((found, _)) if found != volume => {
            return Err(VolumeError::OtherVolume {
                found,
                wanted: volume.to_owned(),
            });
        }
        Some((_, found_branch)) => {
            if let Some(wanted) = branch
                && found_branch != wanted
            {
                return Err(VolumeError::OtherBranch {
                    found: found_branch,
                    wanted: wanted.to_owned(),
                });
            }
            for name in ["exclude", "attributes"] {
                let path = git.join("info").join(name);
                if !path.exists() {
                    std::fs::create_dir_all(git.join("info"))?;
                    std::fs::write(path, "")?;
                }
            }
            Init::AlreadyInitialised
        }
        None => {
            History::init(&git, volume, branch)?;
            Init::Created
        }
    };
    std::fs::create_dir_all(state.journal().parent().expect("journal has a parent"))?;
    // Not `PendingStore::open`: a running front's half-written copies
    // must survive an `init` beside it.
    std::fs::create_dir_all(state.pending())?;
    std::fs::create_dir_all(state.fronts_dir())?;
    std::fs::create_dir_all(state.push_dir())?;
    if let Some(includes) = includes {
        crate::history::write_includes(&git, includes)?;
    }
    History::open(&git)?;
    Ok(outcome)
}

/// The volume and branch an existing state area's own config names, or
/// `None` if it has no config or no `agentfs.volume`. Read without
/// checking includes, so `init --include` can repair a bad one.
fn existing_identity(git: &Path) -> Result<Option<(String, String)>, VolumeError> {
    let path = git.join("config");
    if !path.exists() {
        return Ok(None);
    }
    crate::history::init_libgit2();
    let config = git2::Config::open(&path).map_err(HistoryError::from)?;
    let get =
        |key: &str| crate::config::string(&config, key).map_err(|e| VolumeError::History(e.into()));
    let Some(volume) = get("agentfs.volume")? else {
        return Ok(None);
    };
    let branch = get("agentfs.branch")?.unwrap_or_else(|| format!("agentfs/{volume}"));
    Ok(Some((volume, branch)))
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

    // Fronts feeding a history service write into the same `pending/`;
    // their copies are theirs to clean up while they live (design §14).
    let live_fronts = FrontLease::live(state)?;
    let pending = Arc::new(PendingStore::open_slot(state.pending(), 0, |slot| {
        !live_fronts.contains(&slot)
    })?);
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
    let pruned_pending = pending.prune_except(&keep, &live_fronts)?;
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
        backend: Backend::Local {
            journal: journal.clone(),
            tracked: Arc::new(tracked),
        },
        operations: operations.clone(),
        pending: pending.clone(),
        rules: rules.clone(),
        claims: Arc::new(PathClaims::default()),
        checks: checks.clone(),
        store_policy: StorePolicy {
            threshold: history.settings().lfs.threshold,
        },
        events: events.clone(),
        telemetry: telemetry.clone(),
        reclaim: Arc::default(),
    };
    let committer = Committer {
        history,
        journal,
        operations,
        pending,
        lfs: Arc::new(ObjectCache::open(state.lfs())?),
        identities,
        context,
        origins: Mutex::new(HashMap::new()),
        checks,
        rules,
        tree_known: true,
        events,
        telemetry: telemetry.clone(),
    };
    Ok((recorder, committer, recovery))
}

/// Opens a front's recorder for the backing tree at `tree` (design §14):
/// it records through `client`'s history service, freezing into the slot
/// the client's front leases.
///
/// A front reads the volume's settings (rules, checks, LFS threshold,
/// fsync class) once, here, and runs no recovery: the journal, tracked set
/// and history belong to the service. It holds no state-area lock; the
/// caller holds the slot's [`FrontLease`] for as long as the recorder
/// lives. Half-written copies its slot left are removed.
pub fn open_front(
    state: &StateDir,
    tree: &Path,
    events: Events,
    telemetry: &Telemetry,
    client: Arc<HistoryClient>,
) -> Result<Recorder, VolumeError> {
    let history = History::open(&state.git())?;
    let settings = history.settings();
    let slot = client.front().slot;
    let pending = Arc::new(PendingStore::open_slot(state.pending(), slot, |s| {
        s == slot
    })?);
    Ok(Recorder {
        backend: Backend::Front { client },
        operations: Arc::new(Operations::default()),
        pending,
        rules: Arc::new(Rules::load(&state.git(), tree, settings.into())?),
        claims: Arc::new(PathClaims::default()),
        checks: Arc::new(Checks::new(Limits::from_remotes(&settings.remotes))),
        store_policy: StorePolicy {
            threshold: settings.lfs.threshold,
        },
        events,
        telemetry: telemetry.clone(),
        reclaim: Arc::default(),
    })
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

/// What an operator operation is, before it has an id (see
/// [`Recorder::begin_operation_spec`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationSpec {
    pub kind: OperationKind,
    /// The full id of the source commit: restored from, reverted, or the
    /// patch.
    pub source: String,
    /// The commit subject (for a patch, its whole message).
    pub summary: String,
    /// Who asked for it.
    pub caller: Caller,
    /// A patch's inbox number.
    pub inbox: Option<u64>,
    /// A patch's author.
    pub author: Option<PatchAuthor>,
}

impl OperationSpec {
    pub fn new(kind: OperationKind, source: String, summary: String, caller: Caller) -> Self {
        Self {
            kind,
            source,
            summary,
            caller,
            inbox: None,
            author: None,
        }
    }
}

/// Records change boundaries. Cheap to clone; safe to share across threads.
///
/// A local recorder ([`open`]) journals itself; a front's ([`open_front`])
/// does the same synchronous step up to the freeze and hands the record to
/// the history service (design §14).
#[derive(Clone)]
pub struct Recorder {
    backend: Backend,
    operations: Arc<Operations>,
    pending: Arc<PendingStore>,
    rules: Arc<Rules>,
    claims: Arc<PathClaims>,
    checks: Arc<Checks>,
    store_policy: StorePolicy,
    events: Events,
    telemetry: Telemetry,
    /// Held for reading by [`Recorder::sequence`] and for writing by
    /// [`Recorder::reclaim_slot`], so no copy a front's record is about to
    /// journal is reclaimed meanwhile.
    reclaim: Arc<RwLock<()>>,
}

/// Where a [`Recorder`]'s records go.
#[derive(Clone)]
enum Backend {
    /// Single-process mode and the history service: this process journals
    /// and holds the tracked set.
    Local {
        journal: Arc<Journal>,
        tracked: Arc<Tracked>,
    },
    /// A front: the history service journals (design §14).
    Front { client: Arc<HistoryClient> },
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
        self.record_with(trigger, caller, inputs, false)
    }

    /// Records an fsync-class group (design §8) as one all-or-nothing
    /// record: like [`Recorder::record`], except that if any input is
    /// refused, by the checks or by `agentfs.maxSize`, none is recorded and
    /// the others are skipped with [`SkipReason::GroupRefused`]. The record
    /// is journaled [`Draft::atomic`], so a refusal at commit time drops the
    /// whole group too. Rule skips (a `-shm` file, a removal of an untracked
    /// member) do not count as refusals.
    pub fn record_group(
        &self,
        trigger: Trigger,
        caller: Caller,
        inputs: Vec<Input>,
    ) -> Result<Recorded, VolumeError> {
        self.record_with(trigger, caller, inputs, true)
    }

    fn record_with(
        &self,
        trigger: Trigger,
        caller: Caller,
        inputs: Vec<Input>,
        atomic: bool,
    ) -> Result<Recorded, VolumeError> {
        let start = Instant::now();
        // A record by an operation's actor is part of that operation (design
        // §15): trigger `restore`, made by the operator who asked for it.
        let actor = caller.pid;
        let operation = self.operations.of_actor(actor);
        let (trigger, caller) = match &operation {
            Some(op) => (op.kind.trigger(), op.caller),
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
        let result = self.record_inner(trigger, caller, inputs, operation, atomic);
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
        atomic: bool,
    ) -> Result<Recorded, VolumeError> {
        let _claim = self.claims.claim(inputs.iter().map(Input::path));
        let decisions = self.decide_all(&inputs)?;
        let mut kept = Vec::with_capacity(inputs.len());
        let mut skipped = Vec::new();
        for (input, decision) in inputs.into_iter().zip(decisions) {
            match decision {
                Decision::Record => kept.push(input),
                // A group member the rules leave out (other than `-shm`)
                // would be missing from the capture: a database without
                // its WAL. The group is refused instead (design §8).
                Decision::Skip(reason)
                    if atomic
                        && !matches!(input, Input::Remove { .. })
                        && reason != SkipReason::SharedMemory =>
                {
                    self.refuse(
                        &mut skipped,
                        input.path().clone(),
                        Refusal::GroupMemberSkipped {
                            reason: reason.to_string(),
                        },
                    );
                }
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
                    } => match self.prepare_write(path, &backing, executable, &mut frozen)? {
                        Ok(change) => change,
                        Err((path, refusal)) => {
                            self.refuse(&mut skipped, path, refusal);
                            continue;
                        }
                    },
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
            if atomic
                && let Some(member) = skipped
                    .iter()
                    .find(|s| matches!(s.reason, SkipReason::Refused(_)))
                    .map(|s| s.path.clone())
            {
                // All or nothing: the refusal was reported; the rest of the
                // group is skipped with it, and their copies dropped.
                self.pending.remove(std::mem::take(&mut frozen))?;
                for change in changes.drain(..) {
                    skipped.push(Skipped {
                        path: change.path().clone(),
                        reason: SkipReason::GroupRefused {
                            member: member.clone(),
                        },
                    });
                }
            }
            if changes.is_empty() {
                return Ok(Recorded {
                    seq: None,
                    skipped: Vec::new(),
                });
            }
            let draft = Draft {
                trigger,
                caller,
                time_ms: now_ms(),
                changes: changes.clone(),
                operation,
                atomic,
                origin: None,
            };
            match &self.backend {
                Backend::Local { journal, .. } => Ok(Recorded {
                    seq: Some(journal.record(draft)?),
                    skipped: Vec::new(),
                }),
                // The service decides again and journals (design §14); its
                // skips come back with the reply.
                Backend::Front { client } => Ok(client.record(draft)?),
            }
        })();
        match result {
            Ok(Recorded {
                seq,
                skipped: by_service,
            }) => {
                if let Backend::Local { tracked, .. } = &self.backend {
                    for change in &changes {
                        tracked.apply(change);
                    }
                }
                for skip in by_service {
                    if let SkipReason::Refused(refusal) = &skip.reason {
                        self.events.emit(Event::PathRefused {
                            path: skip.path.to_string(),
                            reason: refusal.clone(),
                            stage: CheckStage::Record,
                        });
                    }
                    skipped.push(skip);
                }
                Ok(Recorded { seq, skipped })
            }
            Err(e) => {
                // Copies frozen for a record that never became durable. A
                // front's request that may have reached the service keeps
                // them: the service may have journaled it, and will commit
                // or prune them (design §3.5).
                let maybe_journaled =
                    matches!(&e, VolumeError::HistoryService(c) if c.maybe_journaled());
                if !maybe_journaled {
                    let _ = self.pending.remove(frozen);
                }
                Err(e)
            }
        }
    }

    /// The record-time half of a write (design §3.2, §3.4, §7): the checks
    /// on the path, `agentfs.maxSize` and the blob limit on the size on
    /// disk, the freeze, then `agentfs.maxSize` and the content checks on
    /// the frozen copy, and the store. The copy is pushed to `frozen`
    /// unless refused, in which case it is already removed.
    fn prepare_write(
        &self,
        path: RepoPath,
        backing: &Path,
        executable: bool,
        frozen: &mut Vec<PendingId>,
    ) -> Result<Result<Change, (RepoPath, Refusal)>, VolumeError> {
        if let Some(refusal) = self.checks.check_path(&path) {
            return Ok(Err((path, refusal)));
        }
        // The store decides whether the blob limit applies (design §7):
        // first by the size on disk, so a file far over the limit is never
        // copied, then by the frozen copy's, which is what is journaled.
        let attrs = self.rules.attributes(&path)?;
        let on_disk = std::fs::metadata(backing).map_or(0, |m| m.len());
        // `agentfs.maxSize` the same way: on disk, then frozen.
        let max_size = self.rules.max_size(&path);
        if let Some(refusal) = over_max_size(on_disk, max_size) {
            return Ok(Err((path, refusal)));
        }
        if self.store_policy.decide(&path, on_disk, &attrs) == Store::Blob
            && let Some(refusal) = self.checks.check_backing_size(backing)
        {
            return Ok(Err((path, refusal)));
        }
        let (id, _) = self.pending.freeze(backing)?;
        frozen.push(id);
        let frozen_size = std::fs::metadata(self.pending.path(id))?.len();
        if let Some(refusal) = over_max_size(frozen_size, max_size) {
            frozen.pop();
            self.pending.remove([id])?;
            return Ok(Err((path, refusal)));
        }
        let store = self.store_policy.decide(&path, frozen_size, &attrs);
        if let Some(refusal) = self.checks.check_file_stored(
            &path,
            &self.pending.path(id),
            CheckStage::Record,
            store,
        )? {
            frozen.pop();
            self.pending.remove([id])?;
            return Ok(Err((path, refusal)));
        }
        Ok(Ok(Change::Write {
            path,
            pending: id,
            executable,
            store,
        }))
    }

    /// Journals a front's record (design §14): the history service's side
    /// of [`Recorder::record`], on its local recorder.
    ///
    /// The front decided with the tracked set as it was; this decides each
    /// change again with the paths claimed, as [`Recorder::record`] does: a
    /// removal of nothing tracked and a write the rules now skip are
    /// skipped, and their copies removed; in an atomic group such a write
    /// refuses the whole group (design §8). A write or symlink whose path
    /// is no longer in the backing tree as sent is skipped
    /// ([`SkipReason::Gone`]): another front removed or renamed it after
    /// this front froze it, and that front's boundary, journaled after
    /// this check under the same claim, records it. Every write's copy must exist
    /// and be in the front's `slot`; operations come only from this
    /// process. The record is journaled with `origin`, so it commits with
    /// the front's committer (design §9), and this returns once it is
    /// durable.
    pub fn sequence(
        &self,
        origin: &Origin,
        slot: u16,
        mut draft: Draft,
    ) -> Result<Recorded, VolumeError> {
        let Backend::Local { journal, tracked } = &self.backend else {
            return Err(VolumeError::FrontRecord(
                "a front cannot journal another front's record".into(),
            ));
        };
        if draft.operation.is_some() {
            return Err(VolumeError::FrontRecord(
                "operations are not accepted from fronts".into(),
            ));
        }
        let _reclaim = self.reclaim.read().unwrap_or_else(PoisonError::into_inner);
        let _claim = self.claims.claim(draft.changes.iter().map(Change::path));
        for change in &draft.changes {
            if let Change::Write { path, pending, .. } = change {
                if pending.slot() != slot {
                    return Err(VolumeError::FrontRecord(format!(
                        "{path}: frozen copy {:016x} is not in the front's slot {slot}",
                        pending.0
                    )));
                }
                if !self.pending.path(*pending).is_file() {
                    return Err(VolumeError::FrontRecord(format!(
                        "{path}: frozen copy {:016x} does not exist",
                        pending.0
                    )));
                }
            }
        }
        let mut kept = Vec::with_capacity(draft.changes.len());
        let mut dropped = Vec::new();
        let mut skipped = Vec::new();
        let mut refused_member = None;
        for change in std::mem::take(&mut draft.changes) {
            let decision = match &change {
                Change::Remove { path } => {
                    if tracked.contains_or_under(path) {
                        Decision::Record
                    } else {
                        Decision::Skip(SkipReason::Untracked)
                    }
                }
                Change::Write { path, .. } | Change::Symlink { path, .. }
                    if !draft.atomic
                        && !still_in_backing(
                            self.rules.tree(),
                            path,
                            matches!(change, Change::Symlink { .. }),
                        ) =>
                {
                    Decision::Skip(SkipReason::Gone)
                }
                Change::Write { path, .. } | Change::Symlink { path, .. } => {
                    self.rules.decide(path, tracked.contains(path))?
                }
            };
            match decision {
                Decision::Record => kept.push(change),
                Decision::Skip(reason) => {
                    let is_remove = matches!(change, Change::Remove { .. });
                    let reason = if draft.atomic && !is_remove && reason != SkipReason::SharedMemory
                    {
                        refused_member.get_or_insert_with(|| change.path().clone());
                        SkipReason::Refused(Refusal::GroupMemberSkipped {
                            reason: reason.to_string(),
                        })
                    } else {
                        reason
                    };
                    if let Change::Write { pending, .. } = &change {
                        dropped.push(*pending);
                    }
                    skipped.push(Skipped {
                        path: change.path().clone(),
                        reason,
                    });
                }
            }
        }
        if let Some(member) = refused_member {
            for change in kept.drain(..) {
                if let Change::Write { pending, .. } = &change {
                    dropped.push(*pending);
                }
                skipped.push(Skipped {
                    path: change.path().clone(),
                    reason: SkipReason::GroupRefused {
                        member: member.clone(),
                    },
                });
            }
        }
        if kept.is_empty() {
            self.pending.remove(dropped)?;
            return Ok(Recorded { seq: None, skipped });
        }
        draft.changes = kept;
        draft.origin = Some(origin.clone());
        let ids: Vec<PendingId> = draft
            .changes
            .iter()
            .filter_map(|c| match c {
                Change::Write { pending, .. } => Some(*pending),
                _ => None,
            })
            .collect();
        let paths: Vec<RepoPath> = draft.changes.iter().map(|c| c.path().clone()).collect();
        let changes = draft.changes.clone();
        let trigger = draft.trigger;
        match journal.record(draft) {
            Ok(seq) => {
                for change in &changes {
                    tracked.apply(change);
                }
                let _ = self.pending.remove(dropped);
                Ok(Recorded {
                    seq: Some(seq),
                    skipped,
                })
            }
            Err(e) => {
                // Never durable: the front reports the boundary as failed.
                let _ = self.pending.remove(ids.into_iter().chain(dropped));
                let e = VolumeError::from(e);
                self.events.emit(Event::RecordFailed {
                    trigger,
                    paths,
                    error: format!("{} (from replica {})", e, origin.replica),
                });
                Err(e)
            }
        }
    }

    /// Whether each of `paths` is tracked now: the history service's answer
    /// to a front's `Tracked` query (design §14). A front has no tracked
    /// set and answers `false` for each.
    pub fn tracked_query(&self, paths: &[RepoPath]) -> Vec<bool> {
        match &self.backend {
            Backend::Local { tracked, .. } => paths.iter().map(|p| tracked.contains(p)).collect(),
            Backend::Front { .. } => vec![false; paths.len()],
        }
    }

    /// Removes the frozen copies a dead front left in `slot` (design §14):
    /// copies frozen before `leased_at`, when the slot's current holder
    /// leased it, that no journaled record references. They were frozen
    /// for records that never reached the journal (the front died between
    /// freeze and send, or the service died before journaling what it
    /// read). No front record is being journaled meanwhile, so a copy a
    /// record is about to reference is never taken. Only the history
    /// service calls this, when a front instance first connects. Returns
    /// how many copies were removed.
    pub fn reclaim_slot(&self, slot: u16, leased_at: SystemTime) -> Result<usize, VolumeError> {
        let Backend::Local { journal, .. } = &self.backend else {
            return Ok(0);
        };
        let _reclaim = self.reclaim.write().unwrap_or_else(PoisonError::into_inner);
        let keep: HashSet<PendingId> = journal
            .uncommitted()
            .iter()
            .flat_map(Record::pending_ids)
            .collect();
        Ok(self.pending.reclaim_slot(slot, leased_at, &keep)?)
    }

    /// Whether this recorder is a front of a history service.
    pub fn is_front(&self) -> bool {
        matches!(self.backend, Backend::Front { .. })
    }

    /// A front's connection to its history service.
    pub fn history_client(&self) -> Option<&Arc<HistoryClient>> {
        match &self.backend {
            Backend::Front { client } => Some(client),
            Backend::Local { .. } => None,
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
        self.begin_operation_spec(OperationSpec::new(kind, source, summary, caller), actors)
    }

    /// [`Recorder::begin_operation`] with every field of the operation,
    /// including a patch's inbox number and author (design §12).
    pub fn begin_operation_spec(
        &self,
        spec: OperationSpec,
        actors: &[u32],
    ) -> Result<OperationGuard, VolumeError> {
        let op = Operation {
            id: self.operations.next_id(now_ms()),
            kind: spec.kind,
            source: spec.source,
            summary: spec.summary,
            caller: spec.caller,
            inbox: spec.inbox,
            author: spec.author,
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
        if let Decision::Skip(reason) = self.decide_write(path)? {
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

    /// Where a regular file at `path` holding `size` bytes would be stored
    /// if it were recorded now (design §7): the attributes in force and
    /// `agentfs.lfsThreshold`.
    pub fn store_for(&self, path: &RepoPath, size: u64) -> Result<Store, VolumeError> {
        let attrs = self.rules.attributes(path)?;
        Ok(self.store_policy.decide(path, size, &attrs))
    }

    /// The last sequence number given to a record, journaled or in flight.
    /// `None` for a front, whose records the history service numbers.
    pub fn last_seq(&self) -> Option<u64> {
        match &self.backend {
            Backend::Local { journal, .. } => journal.last_seq(),
            Backend::Front { .. } => None,
        }
    }

    /// Waits up to `timeout` until every record up to `seq` is in history
    /// (or failed and was never journaled), including records still being
    /// written to the journal when it is called. Returns whether they all
    /// are. A front, which commits nothing itself, returns `false`.
    pub fn wait_committed(&self, seq: u64, timeout: Duration) -> bool {
        let Backend::Local { journal, .. } = &self.backend else {
            return false;
        };
        let deadline = Instant::now() + timeout;
        loop {
            if journal.settled_through(seq) {
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

    /// The fsync-class settings for `path` (design §8), if it is in the
    /// class.
    pub fn fsync_class(&self, path: &RepoPath) -> Option<FsyncClass> {
        self.rules.fsync_class(path)
    }

    /// Whether any path can be in the fsync class on this volume.
    pub fn has_fsync_class(&self) -> bool {
        self.rules.has_fsync_class()
    }

    /// The fsync-class group `path` belongs to, if it is in the class and
    /// not a `-shm` file.
    pub fn fsync_group(&self, path: &RepoPath) -> Option<GroupMembers> {
        let class = self.rules.fsync_class(path)?;
        fsync_class::group_members(path, &class, |p| self.rules.fsync_class(p))
    }

    /// Whether process `pid` is an actor of an operator operation now
    /// (design §15).
    pub fn in_operation(&self, pid: u32) -> bool {
        self.operations.of_actor(pid).is_some()
    }

    #[cfg(test)]
    fn journal_for_tests(&self) -> &Journal {
        match &self.backend {
            Backend::Local { journal, .. } => journal,
            Backend::Front { .. } => panic!("a front has no journal"),
        }
    }

    #[cfg(test)]
    fn tracked_for_tests(&self) -> &Tracked {
        match &self.backend {
            Backend::Local { tracked, .. } => tracked,
            Backend::Front { .. } => panic!("a front has no tracked set"),
        }
    }

    /// The rules' decision for each input (design §4). A front asks the
    /// history service whether a path is tracked only when the decision
    /// depends on it (an ignored or nested path), in one query for the
    /// whole record, and leaves removals to the service, which holds the
    /// tracked set (design §14).
    fn decide_all(&self, inputs: &[Input]) -> Result<Vec<Decision>, VolumeError> {
        let Backend::Front { client } = &self.backend else {
            return inputs.iter().map(|i| self.decide(i)).collect();
        };
        let mut decisions = Vec::with_capacity(inputs.len());
        let mut ask = Vec::new();
        for input in inputs {
            let decision = match input {
                Input::Remove { .. } => Decision::Record,
                Input::Write { path, .. } | Input::Symlink { path, .. } => {
                    match self.front_decide(path)? {
                        FrontDecision::Final(decision) => decision,
                        FrontDecision::IfUntracked(skip) => {
                            ask.push((decisions.len(), path.clone(), skip));
                            Decision::Record
                        }
                    }
                }
            };
            decisions.push(decision);
        }
        if !ask.is_empty() {
            let paths: Vec<RepoPath> = ask.iter().map(|(_, path, _)| path.clone()).collect();
            let tracked = client.tracked(&paths)?;
            for ((i, _, skip), tracked) in ask.into_iter().zip(tracked) {
                if !tracked {
                    decisions[i] = skip;
                }
            }
        }
        Ok(decisions)
    }

    /// A front's decision for a write at `path` without the tracked set:
    /// final if being tracked does not change it.
    fn front_decide(&self, path: &RepoPath) -> Result<FrontDecision, VolumeError> {
        Ok(match self.rules.decide(path, true)? {
            skip @ Decision::Skip(_) => FrontDecision::Final(skip),
            Decision::Record => match self.rules.decide(path, false)? {
                Decision::Record => FrontDecision::Final(Decision::Record),
                skip @ Decision::Skip(_) => FrontDecision::IfUntracked(skip),
            },
        })
    }

    /// The rules' decision for a write at `path`, given whether it is
    /// tracked now.
    fn decide_write(&self, path: &RepoPath) -> Result<Decision, VolumeError> {
        match &self.backend {
            Backend::Local { tracked, .. } => {
                Ok(self.rules.decide(path, tracked.contains(path))?)
            }
            Backend::Front { client } => match self.front_decide(path)? {
                FrontDecision::Final(decision) => Ok(decision),
                FrontDecision::IfUntracked(skip) => {
                    let tracked = client.tracked(std::slice::from_ref(path))?;
                    Ok(if tracked.first() == Some(&true) {
                        Decision::Record
                    } else {
                        skip
                    })
                }
            },
        }
    }

    fn decide(&self, input: &Input) -> Result<Decision, VolumeError> {
        Ok(match input {
            Input::Remove { path } => match &self.backend {
                Backend::Local { tracked, .. } if !tracked.contains_or_under(path) => {
                    Decision::Skip(SkipReason::Untracked)
                }
                _ => Decision::Record,
            },
            Input::Write { path, .. } | Input::Symlink { path, .. } => self.decide_write(path)?,
        })
    }
}

/// A front's decision before it knows whether the path is tracked.
enum FrontDecision {
    Final(Decision),
    /// Recorded if tracked, else skipped like this.
    IfUntracked(Decision),
}

/// Turns recorded changes into commits.
pub struct Committer {
    history: History,
    journal: Arc<Journal>,
    operations: Arc<Operations>,
    pending: Arc<PendingStore>,
    lfs: Arc<ObjectCache>,
    identities: IdentityRenderer,
    context: Context,
    /// Each front's identity (design §9, §14), rendered once per origin;
    /// `None` when the origin no longer renders with the templates in
    /// force, so the service's own committer is used instead.
    origins: Mutex<HashMap<Origin, Option<IdentityRenderer>>>,
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

    /// The replica's committer signature, now (design §9).
    pub fn committer_signature(&self) -> Result<git2::Signature<'static>, git2::Error> {
        let c = self.identities.committer();
        git2::Signature::now(&c.name, &c.email)
    }

    /// Commits that the patch of `op` is applied when it changes nothing
    /// ([`History::commit_patch_marker`], design §12).
    pub fn commit_patch_marker(&self, op: &Operation) -> Result<git2::Oid, VolumeError> {
        let commit = self
            .history
            .commit_patch_marker(op, &self.identities, &self.context)?;
        self.telemetry.commit_made();
        Ok(commit)
    }

    /// The local LFS object cache (design §7).
    pub fn lfs_cache(&self) -> &Arc<ObjectCache> {
        &self.lfs
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

    /// The committer identity and trailer context for records from
    /// `origin` (design §9, §14): the process's own without one, else the
    /// front's pod rendered with the volume's templates. A front's origin
    /// that no longer renders (the templates changed across a restart of
    /// the history service) keeps its pod trailers but gets the service's
    /// own identity, with [`Event::IdentityFallback`] once, so the commit
    /// loop never stalls on it.
    fn identity_for(&self, origin: Option<&Origin>) -> (IdentityRenderer, Context) {
        let Some(origin) = origin else {
            return (self.identities.clone(), self.context.clone());
        };
        let context = origin.context(&self.context.volume);
        // Poisoning needs a panic while the lock is held; nothing in the
        // critical section panics.
        let mut origins = self
            .origins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let renderer =
            origins.entry(origin.clone()).or_insert_with(|| {
                match IdentityRenderer::new(self.history.settings().identity.clone(), &context) {
                    Ok(renderer) => Some(renderer),
                    Err(e) => {
                        self.events.emit(Event::IdentityFallback {
                            replica: origin.replica.clone(),
                            error: e.to_string(),
                        });
                        None
                    }
                }
            });
        let identities = renderer.clone().unwrap_or_else(|| self.identities.clone());
        (identities, context)
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
            // A group never mixes origins (`group_by_window`).
            let (identities, context) = self.identity_for(group[0].draft.origin.as_ref());
            match self.history.commit(
                &checked,
                &self.pending,
                &self.lfs,
                &self.checks,
                &identities,
                &context,
            ) {
                Ok(Committed {
                    commit,
                    last_seq,
                    refused,
                    lfs_as_blobs,
                }) => {
                    // The managed .gitattributes block could not be
                    // written as is (design §7): changes dropped, LFS
                    // content stored as blobs.
                    if !refused.is_empty() {
                        report.refused += refused.len();
                        span.set(KeyValue::new(
                            "agentfs.refused.attributes",
                            refused.len() as i64,
                        ));
                    }
                    for (path, reason) in refused {
                        self.events.emit(Event::PathRefused {
                            path: path.to_string(),
                            reason,
                            stage: CheckStage::Commit,
                        });
                    }
                    if lfs_as_blobs > 0 {
                        span.set(KeyValue::new("agentfs.lfs_as_blobs", lfs_as_blobs as i64));
                    }
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
            let mut refused_any = false;
            for change in record.draft.changes {
                let refusal = match &change {
                    Change::Write {
                        path,
                        pending,
                        store,
                        ..
                    } => self.checks.check_file_stored(
                        path,
                        &self.pending.path(*pending),
                        CheckStage::Commit,
                        *store,
                    )?,
                    Change::Symlink { path, target } => self.checks.check_symlink(path, target),
                    Change::Remove { .. } => None,
                };
                if let Some(reason) = refusal {
                    refused_any = true;
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
            // An fsync-class group is only consistent whole (design §8).
            if refused_any && record.draft.atomic {
                kept.clear();
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

/// Whether `path` is still in the backing tree at `tree` as a front
/// recorded it (design §14): a regular file (or, with `symlink`, a
/// symlink) whose every directory is a real directory. Another front
/// that removed or renamed the path since records that itself, so a late
/// write must not bring it back. A failure other than a missing path
/// counts as present.
fn still_in_backing(tree: &Path, path: &RepoPath, symlink: bool) -> bool {
    let mut at = tree.to_path_buf();
    let parts: Vec<&str> = path.as_str().split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        at.push(part);
        let meta = match std::fs::symlink_metadata(&at) {
            Ok(meta) => meta,
            Err(e) => {
                return !matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                );
            }
        };
        let ty = meta.file_type();
        let ok = if i + 1 < parts.len() {
            ty.is_dir()
        } else if symlink {
            ty.is_symlink()
        } else {
            ty.is_file()
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Splits records into runs where each record is within `window_ms` of the
/// run's first record. Records of an operator operation (design §15) form
/// runs of their own, however long the operation takes; workload records
/// interleaved with them (a live restore) split them, since commits
/// follow the journal's order. Records from different fronts of a history
/// service (design §14) never share a run either: a run is one origin's,
/// so its commit carries that pod's committer.
fn group_by_window(records: &[Record], window_ms: u64) -> Vec<&[Record]> {
    let op = |r: &Record| r.draft.operation.as_ref().map(|o| o.id);
    let mut groups = Vec::new();
    let mut start = 0;
    for i in 1..records.len() {
        let first = &records[start];
        let split = if op(&records[i]) != op(first) || records[i].draft.origin != first.draft.origin
        {
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

/// [`Refusal::MaxSize`] if `bytes` is over the path's `agentfs.maxSize`.
fn over_max_size(bytes: u64, limit: Option<u64>) -> Option<Refusal> {
    limit
        .filter(|limit| bytes > *limit)
        .map(|limit| Refusal::MaxSize { bytes, limit })
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
    use crate::lfs::Pointer;
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
    fn open_spares_a_live_fronts_pending_copies() {
        let (dir, state) = setup();
        let lease = FrontLease::acquire(&state, "test").unwrap();
        let dead = PendingId::in_slot(lease.slot() + 1, 0);
        let src = dir.path().join("src");
        fs::write(&src, "x").unwrap();
        let front = PendingStore::open_slot(state.pending(), lease.slot(), |_| false).unwrap();
        let (live, _) = front.freeze(&src).unwrap();
        let tmp = state.pending().join(format!("tmp-{:016x}", live.0 + 1));
        fs::write(&tmp, "half").unwrap();
        fs::write(front.path(dead), "orphan").unwrap();

        let (_, _, recovery) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();

        assert_eq!(recovery.pruned_pending, 1, "only the dead slot's copy");
        assert!(front.path(live).exists());
        assert!(tmp.exists(), "a live front's half-written copy survives");
        assert!(!front.path(dead).exists());
        drop(lease);
    }

    #[test]
    fn init_creates_the_fronts_dir_and_keeps_pending_tmp_files() {
        let (_dir, state) = setup();
        assert!(state.fronts_dir().is_dir());
        let tmp = state.pending().join("tmp-0001000000000000");
        fs::write(&tmp, "half").unwrap();
        init(&state, "vol", None).unwrap();
        assert!(tmp.exists());
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
                .commit(
                    &records,
                    &com.pending,
                    &com.lfs,
                    &com.checks,
                    &com.identities,
                    &com.context,
                )
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

    fn set_lfs_threshold(state: &StateDir, threshold: u64) {
        let mut config = git2::Config::open(&state.git().join("config")).unwrap();
        config
            .set_i64("agentfs.lfsThreshold", threshold as i64)
            .unwrap();
    }

    fn journaled_stores(rec: &Recorder) -> Vec<(String, Store)> {
        rec.journal_for_tests()
            .uncommitted()
            .iter()
            .flat_map(|r| r.draft.changes.clone())
            .filter_map(|c| match c {
                Change::Write { path, store, .. } => Some((path.to_string(), store)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_store_is_decided_at_record_time_and_journaled() {
        let (dir, state) = setup();
        set_lfs_threshold(&state, 10);
        fs::write(
            state.git().join("info/attributes"),
            "*.psd filter=lfs\nkeep.bin agentfs.store=blob\n",
        )
        .unwrap();
        let tree = dir.path().join("tree");
        fs::create_dir_all(&tree).unwrap();
        let (rec, com, _) = open(&state, &tree, context(), null(), &Telemetry::noop()).unwrap();
        rec.record(
            Trigger::Close,
            CALLER,
            vec![
                write(dir.path(), "ten.bin", "0123456789"),
                write(dir.path(), "eleven.bin", "0123456789a"),
                write(dir.path(), "tiny.psd", "x"),
                write(dir.path(), "keep.bin", "0123456789abcdef"),
                write(dir.path(), "empty.psd", ""),
            ],
        )
        .unwrap();
        assert_eq!(
            journaled_stores(&rec),
            [
                ("ten.bin".to_owned(), Store::Blob),
                ("eleven.bin".to_owned(), Store::Lfs),
                ("tiny.psd".to_owned(), Store::Lfs),
                ("keep.bin".to_owned(), Store::Blob),
                ("empty.psd".to_owned(), Store::Blob),
            ]
        );
        assert_eq!(
            rec.store_for(&RepoPath::new("x").unwrap(), 11).unwrap(),
            Store::Lfs
        );
        assert_eq!(
            rec.store_for(&RepoPath::new("x").unwrap(), 10).unwrap(),
            Store::Blob
        );
        com.commit_pending().unwrap();
        assert!(Pointer::parse(&blob(&com, "eleven.bin")).is_some());
        assert!(Pointer::parse(&blob(&com, "tiny.psd")).is_some());
        assert_eq!(blob(&com, "ten.bin"), b"0123456789");
        assert_eq!(blob(&com, "keep.bin"), b"0123456789abcdef");
        let attrs = String::from_utf8(blob(&com, ".gitattributes")).unwrap();
        assert!(attrs.contains("\n/eleven.bin filter=lfs"), "{attrs}");
        assert!(attrs.contains("\n/tiny.psd filter=lfs"), "{attrs}");
        assert!(!attrs.contains("keep.bin"), "{attrs}");
        // History only: the backing tree is never written.
        assert_eq!(fs::read_dir(&tree).unwrap().count(), 0);
        let cached = fs::read_dir(state.lfs_objects()).unwrap().count();
        assert_eq!(cached, 2, "two objects, under their aa/ directories");
        assert_eq!(fs::read_dir(state.lfs_tmp()).unwrap().count(), 0);
    }

    #[test]
    fn the_blob_limit_does_not_apply_to_lfs_content() {
        // The limit still leaves room for the managed .gitattributes, a
        // blob like any other.
        let (dir, state) = setup();
        set_remote_limit(&state, "r", 200);
        set_lfs_threshold(&state, 250);
        fs::write(
            state.git().join("info/attributes"),
            "keep agentfs.store=blob\n",
        )
        .unwrap();
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
                    write(dir.path(), "large", &"l".repeat(300)),
                    write(dir.path(), "keep", &"k".repeat(300)),
                    write(dir.path(), "mid", &"m".repeat(220)),
                ],
            )
            .unwrap();
        // Blobs over the limit are refused, whatever forced them to be blobs.
        let skipped: Vec<&str> = recorded.skipped.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(skipped, ["keep", "mid"]);
        let refused: Vec<String> = sink
            .take()
            .into_iter()
            .map(|e| match e {
                Event::PathRefused { path, .. } => path,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(refused, ["keep", "mid"]);
        com.commit_pending().unwrap();
        assert_eq!(tree_paths(&com), [".gitattributes", "large"]);
    }

    #[test]
    fn the_commit_time_recheck_honours_the_journaled_store() {
        let (dir, state) = setup();
        set_lfs_threshold(&state, 250);
        {
            let (rec, _, _) =
                open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
            rec.record(
                Trigger::Close,
                CALLER,
                vec![
                    write(dir.path(), "large", &"l".repeat(300)),
                    write(dir.path(), "small", &"s".repeat(220)),
                ],
            )
            .unwrap();
            // Crash before committing; the remount brings a strict remote
            // and no threshold.
        }
        set_remote_limit(&state, "r", 200);
        git2::Config::open(&state.git().join("config"))
            .unwrap()
            .remove("agentfs.lfsThreshold")
            .unwrap();
        let sink = Arc::new(MemorySink::new());
        let (_, com, _) = open(
            &state,
            dir.path(),
            context(),
            sink.clone(),
            &Telemetry::noop(),
        )
        .unwrap();
        let report = com.commit_pending().unwrap();
        assert_eq!(report.refused, 1, "only the blob over the limit");
        assert_eq!(tree_paths(&com), [".gitattributes", "large"]);
        assert!(Pointer::parse(&blob(&com, "large")).is_some());
    }

    #[test]
    fn lfs_content_never_lands_without_its_managed_line() {
        let (dir, state) = setup();
        set_lfs_threshold(&state, 8);
        let sink = Arc::new(MemorySink::new());
        let (rec, com, _) = open(
            &state,
            dir.path(),
            context(),
            sink.clone(),
            &Telemetry::noop(),
        )
        .unwrap();
        // A directory named .gitattributes is refused at record time
        // (gitattributesBlob), so the block always has a place.
        let recorded = rec
            .record(
                Trigger::Close,
                CALLER,
                vec![
                    write(dir.path(), ".gitattributes/x", "hi\n"),
                    write(dir.path(), "big.bin", "0123456789abcdef"),
                ],
            )
            .unwrap();
        assert_eq!(
            recorded.skipped,
            [skipped(
                ".gitattributes/x",
                SkipReason::Refused(Refusal::SpecialFileNotBlob {
                    file: crate::paths::SpecialFile::Gitattributes,
                })
            )]
        );
        sink.take();
        com.commit_pending().unwrap();
        assert!(Pointer::parse(&blob(&com, "big.bin")).is_some());
        assert_eq!(tree_paths(&com), [".gitattributes", "big.bin"]);

        // A .gitattributes that fits the limit alone, but not with the
        // block: dropped at commit time with an alarm, and later records
        // still commit.
        set_remote_limit(&state, "r", 200);
        drop((rec, com));
        let (rec, com, _) = open(
            &state,
            dir.path(),
            context(),
            sink.clone(),
            &Telemetry::noop(),
        )
        .unwrap();
        let before = blob(&com, ".gitattributes");
        let user = format!("{}\n", "#".repeat(150));
        rec.record(
            Trigger::Close,
            CALLER,
            vec![
                write(dir.path(), ".gitattributes", &user),
                write(dir.path(), "other.bin", "fedcba9876543210"),
            ],
        )
        .unwrap();
        let report = com.commit_pending().unwrap();
        assert_eq!((report.commits, report.refused), (1, 1));
        let events = sink.take();
        assert!(
            matches!(
                &events[..],
                [Event::PathRefused { path, stage: CheckStage::Commit, reason: Refusal::BlobTooLarge { .. } }]
                    if path == ".gitattributes"
            ),
            "{events:?}"
        );
        assert_eq!(blob(&com, ".gitattributes"), before);
        // Stored as a blob: no pointer without its line.
        assert_eq!(blob(&com, "other.bin"), b"fedcba9876543210");
        rec.record(
            Trigger::Close,
            CALLER,
            vec![write(dir.path(), "small.txt", "ok")],
        )
        .unwrap();
        let report = com.commit_pending().unwrap();
        assert_eq!(report.commits, 1);
        assert_eq!(com.backlog(), 0);
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
    fn max_size_caps_a_path_whatever_the_store() {
        let (dir, state) = setup();
        set_lfs_threshold(&state, 2);
        fs::write(
            state.git().join("info/attributes"),
            "big/* agentfs.maxSize=4\n",
        )
        .unwrap();
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
                    write(dir.path(), "big/a", "0123456789"),
                    write(dir.path(), "big/b", "abc"),
                    write(dir.path(), "free", "0123456789"),
                ],
            )
            .unwrap();
        assert_eq!(
            recorded.skipped,
            [Skipped {
                path: RepoPath::new("big/a").unwrap(),
                reason: SkipReason::Refused(Refusal::MaxSize {
                    bytes: 10,
                    limit: 4
                }),
            }]
        );
        assert!(matches!(
            &sink.take()[..],
            [Event::PathRefused { path, stage: CheckStage::Record, reason: Refusal::MaxSize { .. } }]
                if path == "big/a"
        ));
        com.commit_pending().unwrap();
        // `free` went to LFS, hence the managed .gitattributes.
        assert_eq!(tree_paths(&com), [".gitattributes", "big/b", "free"]);
        assert_eq!(fs::read_dir(state.pending()).unwrap().count(), 0);
    }

    const SQLITE_RULES: &str =
        "*.db* agentfs.commit=fsync agentfs.group=sqlite agentfs.coalesce=2s\n";

    #[test]
    fn the_recorder_knows_fsync_groups() {
        let (dir, state) = setup();
        let (rec, _, _) = open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
        assert!(!rec.has_fsync_class());
        assert_eq!(rec.fsync_group(&RepoPath::new("a.db").unwrap()), None);
        drop(rec);
        fs::write(state.git().join("info/attributes"), SQLITE_RULES).unwrap();
        let (rec, _, _) = open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
        assert!(rec.has_fsync_class());
        let p = |s: &str| RepoPath::new(s).unwrap();
        let group = rec.fsync_group(&p("d/a.db-journal")).unwrap();
        assert_eq!(
            group.members,
            [p("d/a.db"), p("d/a.db-journal"), p("d/a.db-wal")]
        );
        assert_eq!(group.coalesce, Duration::from_secs(2));
        assert_eq!(rec.fsync_group(&p("d/a.db-shm")), None);
        assert_eq!(rec.fsync_group(&p("a.txt")), None);
        assert!(!rec.in_operation(CALLER.pid));
    }

    #[test]
    fn bad_class_values_refuse_to_open() {
        let (dir, state) = setup();
        fs::write(
            state.git().join("info/attributes"),
            "*.db agentfs.commit=fsnc\n",
        )
        .unwrap();
        let err = open(&state, dir.path(), context(), null(), &Telemetry::noop())
            .err()
            .unwrap();
        assert!(
            matches!(err, VolumeError::Rules(RulesError::BadAttribute { .. })),
            "{err}"
        );
    }

    #[test]
    fn a_group_is_recorded_whole_or_not_at_all() {
        let (dir, state) = setup();
        fs::write(
            state.git().join("info/attributes"),
            format!("{SQLITE_RULES}*.db-wal agentfs.maxSize=4\n"),
        )
        .unwrap();
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
        let p = |s: &str| RepoPath::new(s).unwrap();
        let recorded = rec
            .record_group(
                Trigger::Interval,
                CALLER,
                vec![
                    write(dir.path(), "s.db", "db"),
                    write(dir.path(), "s.db-wal", "0123456789"),
                    Input::Remove {
                        path: p("s.db-journal"),
                    },
                ],
            )
            .unwrap();
        assert_eq!(recorded.seq, None);
        assert_eq!(
            recorded.skipped,
            [
                Skipped {
                    path: p("s.db-journal"),
                    reason: SkipReason::Untracked,
                },
                Skipped {
                    path: p("s.db-wal"),
                    reason: SkipReason::Refused(Refusal::MaxSize {
                        bytes: 10,
                        limit: 4
                    }),
                },
                Skipped {
                    path: p("s.db"),
                    reason: SkipReason::GroupRefused {
                        member: p("s.db-wal")
                    },
                },
            ]
        );
        assert_eq!(fs::read_dir(state.pending()).unwrap().count(), 0);
        assert_eq!(com.backlog(), 0);

        let seq = rec
            .record_group(
                Trigger::Interval,
                CALLER,
                vec![
                    write(dir.path(), "s.db", "db"),
                    write(dir.path(), "s.db-wal", "wal"),
                ],
            )
            .unwrap()
            .seq;
        assert_eq!(seq, Some(0));
        let records = com.journal.uncommitted();
        assert!(records[0].draft.atomic);
        assert_eq!(records[0].draft.trigger, Trigger::Interval);
        com.commit_pending().unwrap();
        assert_eq!(tree_paths(&com), ["s.db", "s.db-wal"]);
        // An ordinary record is not atomic.
        rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "x", "1")])
            .unwrap();
        assert!(!com.journal.uncommitted()[0].draft.atomic);
    }

    #[test]
    fn an_atomic_record_refused_at_commit_is_dropped_whole() {
        let (dir, state) = setup();
        fs::write(state.git().join("info/attributes"), SQLITE_RULES).unwrap();
        {
            let (rec, com, _) =
                open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
            rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "keep", "1")])
                .unwrap();
            com.commit_pending().unwrap();
            rec.record_group(
                Trigger::Fsync,
                CALLER,
                vec![
                    write(dir.path(), "s.db", "0123456789"),
                    write(dir.path(), "s.db-wal", "12"),
                ],
            )
            .unwrap();
            // Crash before committing; the remount brings a stricter remote.
        }
        set_remote_limit(&state, "r", 5);
        let sink = Arc::new(MemorySink::new());
        let (_, com, _) = open(
            &state,
            dir.path(),
            context(),
            sink.clone(),
            &Telemetry::noop(),
        )
        .unwrap();
        let report = com.commit_pending().unwrap();
        assert_eq!((report.records, report.refused), (1, 1));
        assert_eq!(tree_paths(&com), ["keep"]);
        assert_eq!(com.backlog(), 0);
        assert_eq!(fs::read_dir(state.pending()).unwrap().count(), 0);
        let events = sink.take();
        assert!(
            matches!(
                &events[..],
                [Event::PathRefused { path, stage: CheckStage::Commit, .. }] if path == "s.db"
            ),
            "{events:?}"
        );
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
            store: Default::default(),
        };
        rec.journal_for_tests()
            .record(Draft {
                trigger: Trigger::Close,
                caller: CALLER,
                time_ms,
                changes: vec![change.clone()],
                operation: None,
                atomic: false,
                origin: None,
            })
            .unwrap();
        rec.tracked_for_tests().apply(&change);
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
            inbox: None,
            author: None,
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
                    atomic: false,
                    origin: None,
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
    fn init_is_idempotent_for_the_same_volume() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path().join("state"));
        assert_eq!(init(&state, "vol", None).unwrap(), Init::Created);
        let config = || std::fs::read(state.git().join("config")).unwrap();
        let before = config();
        std::fs::remove_dir_all(state.push_dir()).unwrap();
        std::fs::remove_file(state.git().join("info/attributes")).unwrap();
        assert_eq!(init(&state, "vol", None).unwrap(), Init::AlreadyInitialised);
        assert_eq!(
            init(&state, "vol", Some("agentfs/vol")).unwrap(),
            Init::AlreadyInitialised
        );
        assert_eq!(config(), before);
        assert!(state.push_dir().is_dir(), "missing directories re-created");
        assert!(state.git().join("info/attributes").is_file());
    }

    #[test]
    fn init_refuses_another_volume_or_branch() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path().join("state"));
        init(&state, "vol", Some("custom")).unwrap();
        let before = std::fs::read(state.git().join("config")).unwrap();
        let err = init(&state, "other", None).unwrap_err();
        assert!(
            matches!(&err, VolumeError::OtherVolume { found, wanted }
                if found == "vol" && wanted == "other"),
            "{err}"
        );
        let err = init(&state, "vol", Some("agentfs/vol")).unwrap_err();
        assert!(
            matches!(&err, VolumeError::OtherBranch { found, wanted }
                if found == "custom" && wanted == "agentfs/vol"),
            "{err}"
        );
        assert_eq!(std::fs::read(state.git().join("config")).unwrap(), before);
        // Without --branch, the stored branch is accepted.
        assert_eq!(init(&state, "vol", None).unwrap(), Init::AlreadyInitialised);
    }

    #[test]
    fn init_keeps_rule_files_a_runtime_created_first() {
        // A container runtime that bind-mounts info/exclude creates the
        // git directory and the file before init runs.
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path().join("state"));
        let info = state.git().join("info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(info.join("exclude"), "*.log\n").unwrap();
        std::fs::write(info.join("attributes"), "secrets/** -agentfs\n").unwrap();
        assert_eq!(init(&state, "vol", None).unwrap(), Init::Created);
        assert_eq!(
            std::fs::read_to_string(info.join("exclude")).unwrap(),
            "*.log\n"
        );
        assert_eq!(
            std::fs::read_to_string(info.join("attributes")).unwrap(),
            "secrets/** -agentfs\n"
        );
    }

    #[test]
    fn init_sets_and_repairs_includes() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path().join("state"));
        let fragment = dir.path().join("fragment");
        std::fs::write(&fragment, "[agentfs]\n\tpushInterval = 9s\n").unwrap();
        let includes = [fragment.clone()];
        assert_eq!(
            init_with(&state, "vol", None, Some(&includes)).unwrap(),
            Init::Created
        );
        let settings = History::open(&state.git()).unwrap().settings().clone();
        assert_eq!(settings.push_interval, Duration::from_secs(9));
        // The fragment disappears: the volume no longer opens, and a
        // re-run with the include given again fails too...
        std::fs::remove_file(&fragment).unwrap();
        assert!(History::open(&state.git()).is_err());
        let err = init_with(&state, "vol", None, Some(&includes)).unwrap_err();
        assert!(err.to_string().contains("cannot be read"), "{err}");
        // ...while one with another include repairs it.
        let other = dir.path().join("other");
        std::fs::write(&other, "[agentfs]\n\tpushInterval = 3s\n").unwrap();
        assert_eq!(
            init_with(&state, "vol", None, Some(&[other])).unwrap(),
            Init::AlreadyInitialised
        );
        let settings = History::open(&state.git()).unwrap().settings().clone();
        assert_eq!(settings.push_interval, Duration::from_secs(3));
        assert_eq!(settings.includes.len(), 1);
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
                atomic: false,
                origin: None,
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

    fn pod(name: &str) -> Origin {
        Origin {
            namespace: "agents".into(),
            pod: name.into(),
            pod_uid: format!("{name}-uid"),
            node: "node-1".into(),
            replica: name.into(),
        }
    }

    #[test]
    fn grouping_never_mixes_origins() {
        let rec = |seq, time_ms, origin: Option<Origin>| Record {
            seq,
            draft: Draft {
                trigger: Trigger::Close,
                caller: CALLER,
                time_ms,
                changes: vec![],
                operation: None,
                atomic: false,
                origin,
            },
        };
        let (a, b) = (Some(pod("a")), Some(pod("b")));
        let records = [
            rec(0, 0, a.clone()),
            rec(1, 1, a.clone()),
            rec(2, 2, b.clone()),
            rec(3, 3, a.clone()),
            rec(4, 4, None),
            rec(5, 5, None),
        ];
        let groups: Vec<Vec<u64>> = group_by_window(&records, 200)
            .into_iter()
            .map(|g| g.iter().map(|r| r.seq).collect())
            .collect();
        assert_eq!(groups, [vec![0, 1], vec![2], vec![3], vec![4, 5]]);
    }

    /// Journals a write of `name` holding `content` as if a front of
    /// `origin` had sent it, the way the history service does.
    fn journal_from(rec: &Recorder, dir: &Path, origin: &Origin, name: &str, content: &str) {
        let Input::Write { path, backing, .. } = write(dir, name, content) else {
            unreachable!()
        };
        let (id, _) = rec.pending.freeze(&backing).unwrap();
        rec.journal_for_tests()
            .record(Draft {
                trigger: Trigger::Close,
                caller: CALLER,
                time_ms: now_ms(),
                changes: vec![Change::Write {
                    path,
                    pending: id,
                    executable: false,
                    store: Store::Blob,
                }],
                operation: None,
                atomic: false,
                origin: Some(origin.clone()),
            })
            .unwrap();
    }

    /// Every commit on the branch, oldest first: committer name and email,
    /// and message.
    fn commits(com: &Committer) -> Vec<(String, String, String)> {
        let repo = com.history().repo();
        let mut walk = repo.revwalk().unwrap();
        walk.push(com.history().head().unwrap().unwrap().id())
            .unwrap();
        walk.set_sorting(git2::Sort::REVERSE | git2::Sort::TOPOLOGICAL)
            .unwrap();
        walk.map(|id| {
            let c = repo.find_commit(id.unwrap()).unwrap();
            let committer = c.committer();
            (
                committer.name().unwrap().to_owned(),
                committer.email().unwrap().to_owned(),
                c.message().unwrap().to_owned(),
            )
        })
        .collect()
    }

    #[test]
    fn records_from_fronts_commit_with_their_pods_committer() {
        let (dir, state) = setup();
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
        journal_from(&rec, dir.path(), &pod("pod-a"), "a1", "1");
        journal_from(&rec, dir.path(), &pod("pod-b"), "b1", "2");
        journal_from(&rec, dir.path(), &pod("pod-a"), "a2", "3");
        rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "own", "4")])
            .unwrap();
        let report = com.commit_pending().unwrap();
        assert_eq!((report.records, report.commits), (4, 4));
        let commits = commits(&com);
        let who: Vec<(&str, &str)> = commits
            .iter()
            .map(|(name, email, _)| (name.as_str(), email.as_str()))
            .collect();
        assert_eq!(
            who,
            [
                ("pod-a", "pod-a-uid@vol.agentfs.invalid"),
                ("pod-b", "pod-b-uid@vol.agentfs.invalid"),
                ("pod-a", "pod-a-uid@vol.agentfs.invalid"),
                ("pod-0", "u@vol.agentfs.invalid"),
            ]
        );
        let message = &commits[1].2;
        for trailer in [
            "Agentfs-Namespace: agents\n",
            "Agentfs-Pod: pod-b\n",
            "Agentfs-Pod-Uid: pod-b-uid\n",
            "Agentfs-Node: node-1\n",
        ] {
            assert!(message.contains(trailer), "{trailer}: {message}");
        }
        assert!(!commits[3].2.contains("Agentfs-Pod:"), "{}", commits[3].2);
        // One renderer per origin, kept.
        assert_eq!(com.origins.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_replay_keeps_each_fronts_committer() {
        let (dir, state) = setup();
        {
            let (rec, _com, _) =
                open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
            journal_from(&rec, dir.path(), &pod("pod-a"), "a", "1");
            journal_from(&rec, dir.path(), &pod("pod-b"), "b", "2");
            // Crash: journaled, never committed.
        }
        let (_rec, com, recovery) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
        assert_eq!(recovery.uncommitted, 2);
        com.commit_pending().unwrap();
        let names: Vec<String> = commits(&com).into_iter().map(|c| c.0).collect();
        assert_eq!(names, ["pod-a", "pod-b"]);
    }

    #[test]
    fn an_origin_that_no_longer_renders_falls_back_to_the_services_committer() {
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
        // A replica name git cannot hold in a committer header.
        let bad = Origin {
            replica: "evil <x>".into(),
            ..pod("pod-c")
        };
        journal_from(&rec, dir.path(), &bad, "c", "1");
        journal_from(&rec, dir.path(), &bad, "d", "2");
        com.commit_pending().unwrap();
        let commits = commits(&com);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].0, "pod-0");
        assert!(
            commits[0].2.contains("Agentfs-Pod: pod-c\n"),
            "{}",
            commits[0].2
        );
        let fallbacks: Vec<Event> = sink
            .take()
            .into_iter()
            .filter(|e| matches!(e, Event::IdentityFallback { .. }))
            .collect();
        assert_eq!(fallbacks.len(), 1, "{fallbacks:?}");
        assert!(fallbacks[0].is_alarm());
        assert!(fallbacks[0].to_string().contains("evil <x>"));
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

    /// Freezes `content` into front slot `slot`, as a front would.
    fn front_write(state: &StateDir, dir: &Path, slot: u16, name: &str, content: &str) -> Change {
        let Input::Write { path, backing, .. } = write(dir, name, content) else {
            unreachable!()
        };
        let store = PendingStore::open_slot(state.pending(), slot, |_| false).unwrap();
        let (id, _) = store.freeze(&backing).unwrap();
        Change::Write {
            path,
            pending: id,
            executable: false,
            store: Store::Blob,
        }
    }

    fn front_draft(changes: Vec<Change>, atomic: bool) -> Draft {
        Draft {
            trigger: Trigger::Close,
            caller: CALLER,
            time_ms: now_ms(),
            changes,
            operation: None,
            atomic,
            origin: None,
        }
    }

    fn pending_of(change: &Change) -> PathBuf {
        let Change::Write { pending, .. } = change else {
            unreachable!()
        };
        PathBuf::from(format!("{:016x}", pending.0))
    }

    #[test]
    fn sequence_journals_with_the_fronts_origin() {
        let (dir, state) = setup();
        let (rec, com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
        let change = front_write(&state, dir.path(), 3, "a", "1");
        let recorded = rec
            .sequence(&pod("pod-a"), 3, front_draft(vec![change], false))
            .unwrap();
        assert_eq!(recorded.seq, Some(0));
        assert!(recorded.skipped.is_empty());
        let records = rec.journal_for_tests().uncommitted();
        assert_eq!(records[0].draft.origin, Some(pod("pod-a")));
        assert!(rec.tracked_query(&[RepoPath::new("a").unwrap()])[0]);
        com.commit_pending().unwrap();
        assert_eq!(commits(&com)[0].0, "pod-a");
    }

    #[test]
    fn sequence_decides_again_with_the_tracked_set() {
        let (dir, state) = setup();
        {
            let (rec, com, _) =
                open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
            rec.record(
                Trigger::Close,
                CALLER,
                vec![write(dir.path(), "kept.log", "0")],
            )
            .unwrap();
            com.commit_pending().unwrap();
        }
        exclude(&state, "*.log\n");
        let (rec, _com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
        let ignored = front_write(&state, dir.path(), 2, "new.log", "1");
        let tracked = front_write(&state, dir.path(), 2, "kept.log", "2");
        let ignored_copy = state.pending().join(pending_of(&ignored));
        let recorded = rec
            .sequence(
                &pod("pod-a"),
                2,
                front_draft(
                    vec![
                        ignored,
                        tracked,
                        Change::Remove {
                            path: RepoPath::new("gone").unwrap(),
                        },
                    ],
                    false,
                ),
            )
            .unwrap();

        assert_eq!(recorded.seq, Some(1));
        assert_eq!(
            recorded.skipped,
            vec![
                skipped(
                    "new.log",
                    SkipReason::Ignored {
                        source: RuleSource::OperatorExclude,
                        pattern: "*.log".into(),
                    }
                ),
                skipped("gone", SkipReason::Untracked),
            ]
        );
        assert!(!ignored_copy.exists(), "a skipped copy is removed");
        let records = rec.journal_for_tests().uncommitted();
        let paths: Vec<_> = records[0]
            .draft
            .changes
            .iter()
            .map(|c| c.path().to_string())
            .collect();
        assert_eq!(paths, ["kept.log"]);
    }

    #[test]
    fn sequence_refuses_copies_that_are_missing_or_not_the_fronts() {
        let (dir, state) = setup();
        let (rec, _com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
        let foreign = front_write(&state, dir.path(), 4, "a", "1");
        let err = rec
            .sequence(&pod("pod-a"), 3, front_draft(vec![foreign], false))
            .unwrap_err();
        assert!(
            err.to_string().contains("not in the front's slot 3"),
            "{err}"
        );

        let missing = front_write(&state, dir.path(), 3, "b", "1");
        fs::remove_file(state.pending().join(pending_of(&missing))).unwrap();
        let err = rec
            .sequence(&pod("pod-a"), 3, front_draft(vec![missing], false))
            .unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");

        let mut with_op = front_draft(vec![], false);
        with_op.operation = Some(Operation {
            id: 1,
            kind: OperationKind::Restore,
            source: "x".into(),
            summary: "s".into(),
            caller: CALLER,
            inbox: None,
            author: None,
        });
        assert!(matches!(
            rec.sequence(&pod("pod-a"), 3, with_op),
            Err(VolumeError::FrontRecord(_))
        ));
        assert_eq!(rec.journal_for_tests().backlog(), 0);
    }

    #[test]
    fn sequence_refuses_an_atomic_group_whole() {
        let (dir, state) = setup();
        exclude(&state, "*-wal\n");
        let (rec, _com, _) =
            open(&state, dir.path(), context(), null(), &Telemetry::noop()).unwrap();
        let db = front_write(&state, dir.path(), 1, "db", "1");
        let wal = front_write(&state, dir.path(), 1, "db-wal", "2");
        let copies = [pending_of(&db), pending_of(&wal)];
        let recorded = rec
            .sequence(&pod("pod-a"), 1, front_draft(vec![db, wal], true))
            .unwrap();
        assert_eq!(recorded.seq, None);
        assert_eq!(recorded.skipped.len(), 2);
        assert!(
            matches!(
                &recorded.skipped[0].reason,
                SkipReason::Refused(Refusal::GroupMemberSkipped { .. })
            ) || matches!(
                &recorded.skipped[1].reason,
                SkipReason::Refused(Refusal::GroupMemberSkipped { .. })
            )
        );
        assert!(recorded.skipped.iter().any(|s| matches!(&s.reason, SkipReason::GroupRefused { member } if member.as_str() == "db-wal")));
        for copy in copies {
            assert!(!state.pending().join(copy).exists());
        }
        assert_eq!(rec.journal_for_tests().backlog(), 0);
    }
}
