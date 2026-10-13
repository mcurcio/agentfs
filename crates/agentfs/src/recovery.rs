//! `agentfs restore` and `agentfs revert` (design §15).
//!
//! Both write content from history back into the tree and record the
//! result as one commit with `Agentfs-Trigger: restore`. The planning,
//! checks and writing are `agentfs_core::restore`; this module picks the
//! mode and reports.
//!
//! - **Mount mode**: when `agentfs mount` is running it listens on the
//!   control socket ([`crate::control`]). The command sends it the request;
//!   the daemon ([`serve`]) waits until history has caught up with the
//!   tree, checks against its mount point (refusing files open for
//!   writing unless `--force`), and writes through its own mount from the
//!   connection's thread, registered as the operation's actor: the FUSE
//!   layer records those writes like any workload's, as part of the
//!   operation, so kernel caches, the inode table and dirty detection stay
//!   coherent. `--root` is not needed.
//! - **Service mode**: `agentfs serve-history` listens on the control
//!   socket too, and serves restores for the fronts that mount the volume
//!   ([`ServiceDaemon`]). Every live front must be listening (or the
//!   request is busy, 75); the service has each one settle (record what is
//!   dirty in it), waits until history has caught up, checks the backing
//!   tree with the fronts' answers about files open for writing, writes
//!   the backing tree itself and records what it wrote as one operation,
//!   then has every front drop its kernel's caches of the paths written.
//! - **Direct mode** (nothing listening): no mount, and so no workload
//!   through one, can be running. The command takes the state-area lock,
//!   commits what is journaled, writes the backing tree (`--root`) itself
//!   and records what it wrote, as one operation.
//!
//! Exit status: 0 done (or already matching), 1 refused or failed, 75 the
//! state area is busy (another command holds it) or the daemon is running
//! another restore.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context as _, Result};

use agentfs_core::config::Settings;
use agentfs_core::confined::Dir;
use agentfs_core::history::{History, seq_from_message};
use agentfs_core::history_service::{Fronts, FrontsError};
use agentfs_core::lfs::ObjectCache;
use agentfs_core::lfs::fetch::{CacheProbe, Fetcher, LfsObjects};
use agentfs_core::lock::{LockError, StateLock};
use agentfs_core::paths::RepoPath;
use agentfs_core::record::Caller;
use agentfs_core::restore::{
    self, Liveness, NotLive, Pathspec, Plan, RecordMode, Request, RestoreError,
};
use agentfs_core::volume::{Recorder, StateDir};
use agentfs_fuse::MountView;

use crate::control::{self, ClientError, Op, Outcome, Reply};
use crate::{EXIT_LOCKED, Pod, Session, current_gid, current_uid};

/// Inputs per record when the CLI records what it wrote, as a directory
/// rename does in the FUSE layer.
const RECORD_CHUNK: usize = 4096;

/// How long the daemon waits for history to catch up, at least.
const MIN_HISTORY_WAIT: Duration = Duration::from_secs(10);

/// What the operator asked for, with its flags.
pub struct Ask {
    pub request: Request,
    /// The path as given (restore only), for the control socket.
    pub path: Option<String>,
    pub force: bool,
    pub dry_run: bool,
}

impl Ask {
    pub fn restore(
        path: &str,
        commit: String,
        force: bool,
        dry_run: bool,
    ) -> Result<Self, RestoreError> {
        Ok(Self {
            request: Request::Restore {
                spec: Pathspec::parse(path)?,
                commit,
            },
            path: Some(path.to_owned()),
            force,
            dry_run,
        })
    }

    pub fn revert(commit: String, force: bool, dry_run: bool) -> Self {
        Self {
            request: Request::Revert { commit },
            path: None,
            force,
            dry_run,
        }
    }

    /// The request as the control socket carries it.
    fn wire(&self) -> control::Request {
        let (op, commit) = match &self.request {
            Request::Restore { commit, .. } => (Op::Restore, commit),
            Request::Revert { commit } => (Op::Revert, commit),
        };
        control::Request {
            v: control::PROTOCOL_VERSION,
            op,
            path: self.path.clone(),
            commit: commit.clone(),
            force: self.force,
            dry_run: self.dry_run,
            remotes: Vec::new(),
            timeout_ms: None,
        }
    }

    /// The request a daemon received.
    fn from_wire(r: control::Request) -> Result<Self, RestoreError> {
        match r.op {
            // No default: a missing path must not mean the whole tree.
            Op::Restore => match r.path.as_deref() {
                Some(path) => Self::restore(path, r.commit, r.force, r.dry_run),
                None => Err(RestoreError::MissingPath),
            },
            Op::Revert => Ok(Self::revert(r.commit, r.force, r.dry_run)),
            Op::Push => Err(other("a push is not a restore or revert")),
        }
    }

    fn command(&self) -> &'static str {
        match self.request {
            Request::Restore { .. } => "restore",
            Request::Revert { .. } => "revert",
        }
    }
}

/// Where a plan's LFS content comes from (design §7, §15): the cache, else
/// the remotes; a dry run only looks in the cache, so it downloads and
/// writes nothing.
enum Objects {
    Fetch(Box<Fetcher>),
    Probe(CacheProbe),
}

impl Objects {
    fn new(state: &StateDir, settings: &Settings, dry_run: bool) -> std::io::Result<Self> {
        Ok(if dry_run {
            Self::Probe(CacheProbe::new(ObjectCache::at(state.lfs())?))
        } else {
            Self::Fetch(Box::new(Fetcher::new(state, settings)?))
        })
    }

    fn get(&self) -> &dyn LfsObjects {
        match self {
            Self::Fetch(f) => f.as_ref(),
            Self::Probe(p) => p,
        }
    }

    /// Objects a dry run found missing from the cache, and their size.
    fn uncached(&self) -> (usize, u64) {
        match self {
            Self::Fetch(_) => (0, 0),
            Self::Probe(p) => p.uncached(),
        }
    }
}

/// The plan as listed to the operator.
pub fn plan_lines(plan: &Plan) -> Vec<String> {
    plan.steps
        .iter()
        .map(|s| format!("{} {}", s.action(), s.path))
        .collect()
}

/// Prints `done` the same way in every mode.
pub fn report(done: &Outcome) {
    for line in &done.plan {
        println!("  {line}");
    }
    if done.dry_run {
        if done.lfs_uncached > 0 {
            println!(
                "{} LFS object(s), {} bytes, not in the local cache: a real run downloads \
                 them from the remotes (not checked by a dry run)",
                done.lfs_uncached, done.lfs_uncached_bytes
            );
        }
        println!(
            "dry run: {} change(s) planned, nothing written",
            done.plan.len()
        );
        return;
    }
    match &done.commit {
        None if done.plan.is_empty() => println!("already matches: nothing to do"),
        None => println!(
            "restored {} path(s), removed {}: no commit",
            done.written, done.removed
        ),
        Some(commit) => println!(
            "restored {} path(s), removed {}: commit {commit}",
            done.written, done.removed
        ),
    }
}

/// Prints a refusal with its hint; exit status 1.
pub fn refused(command: &str, error: &RestoreError) -> ExitCode {
    print_refusal(command, &error.to_string(), error.hint());
    ExitCode::FAILURE
}

fn print_refusal(command: &str, message: &str, hint: Option<&str>) {
    eprintln!("{command} refused: {message}");
    if let Some(hint) = hint {
        eprintln!("hint: {hint}");
    }
}

/// The process's own ids, as the operator of a direct-mode operation.
fn own_caller() -> Caller {
    Caller {
        uid: current_uid(),
        gid: current_gid(),
        pid: std::process::id(),
    }
}

/// `agentfs restore` / `agentfs revert`: through a running mount if one
/// listens, else directly.
pub fn run(
    state: &StateDir,
    root: Option<&Path>,
    wait_lock: Duration,
    pod: &Pod,
    ask: Ask,
) -> Result<ExitCode> {
    let command = ask.command();
    match control::call(&state.control_dir(), &ask.wire()) {
        Ok(reply) => return Ok(print_reply(command, reply)),
        Err(ClientError::NotRunning) => {}
        Err(e) => {
            eprintln!(
                "error: a mount seems to be running, but its control socket in {} cannot be \
                 used: {e}",
                state.control_dir().display()
            );
            eprintln!("hint: run as root or as the user the mount runs as");
            return Ok(ExitCode::FAILURE);
        }
    }
    direct(state, root, wait_lock, pod, ask)
}

/// Reports a daemon's reply; the exit status.
fn print_reply(command: &str, reply: Reply) -> ExitCode {
    match reply {
        Reply::Ok(done) => {
            report(&done);
            ExitCode::SUCCESS
        }
        Reply::Refused { message, hint, .. } => {
            print_refusal(command, &message, hint.as_deref());
            ExitCode::FAILURE
        }
        Reply::Busy(message) => {
            eprintln!("error: {message}");
            ExitCode::from(EXIT_LOCKED)
        }
        Reply::Error(message) => {
            eprintln!("{command} failed: {message}");
            ExitCode::FAILURE
        }
        Reply::Pushed(_) => {
            eprintln!("{command} failed: the daemon answered with a push's reply");
            ExitCode::FAILURE
        }
    }
}

/// Direct mode: no mount runs.
fn direct(
    state: &StateDir,
    root: Option<&Path>,
    wait_lock: Duration,
    pod: &Pod,
    ask: Ask,
) -> Result<ExitCode> {
    let command = ask.command();
    let Some(root) = root else {
        eprintln!(
            "error: no mount is running, so {command} writes the backing tree directly and \
             needs --root (or AGENTFS_ROOT)"
        );
        return Ok(ExitCode::FAILURE);
    };
    let _lock = match StateLock::acquire(state, command, wait_lock) {
        Ok(lock) => lock,
        Err(busy @ LockError::Busy { .. }) => {
            eprintln!("error: {busy}");
            if let LockError::Busy { holder, .. } = &busy
                && holder.contains("command=run")
            {
                eprintln!(
                    "hint: `agentfs run` holds it and serves no restores; stop it, or mount \
                     the volume, then retry"
                );
            }
            return Ok(ExitCode::from(EXIT_LOCKED));
        }
        Err(e) => return Err(e.into()),
    };
    let session = Session::start(state, pod)?;
    let (recorder, committer, _) = session.open(state, root, pod)?;
    // History must hold everything recorded before the tree is compared
    // with it.
    committer
        .commit_pending()
        .context("committing what is already recorded")?;
    let tree = Dir::open(root).with_context(|| format!("opening {}", root.display()))?;
    let history = committer.history();
    // LFS content comes from the cache, else from the remotes (design §7).
    let objects =
        Objects::new(state, history.settings(), ask.dry_run).context("opening the LFS cache")?;
    let checked = restore::plan(history.repo(), &history.settings().branch_ref, &ask.request)
        .and_then(|plan| {
            restore::check(
                plan,
                history.repo(),
                &recorder,
                objects.get(),
                &tree,
                &NotLive,
                ask.force,
            )
        });
    let plan = match checked {
        Ok(plan) => plan,
        Err(e) if e.is_refusal() => {
            session.finish();
            return Ok(refused(command, &e));
        }
        Err(e) => return Err(e.into()),
    };
    let mut done = Outcome {
        plan: plan_lines(&plan),
        dry_run: ask.dry_run,
        lfs_uncached: objects.uncached().0,
        lfs_uncached_bytes: objects.uncached().1,
        ..Outcome::default()
    };
    if ask.dry_run || plan.steps.is_empty() {
        report(&done);
        session.finish();
        return Ok(ExitCode::SUCCESS);
    }

    let me = own_caller();
    let guard = recorder.begin_operation(
        plan.kind,
        plan.source.to_string(),
        plan.summary.clone(),
        me,
        &[me.pid],
    )?;
    let (applied, failure) = match restore::apply(
        &plan,
        history.repo(),
        objects.get(),
        &tree,
        &NotLive,
        ask.force,
    ) {
        Ok(applied) => (applied, None),
        Err(e) => (e.applied.clone(), Some(e)),
    };
    // A path the failure left changed (a write cut short) is recorded as it
    // is, so the tree matches history again and a re-run can finish it.
    let mut to_record = applied.clone();
    if let Some(path) = failure
        .as_ref()
        .and_then(|e| e.left_changed(&plan, history.repo(), &tree))
    {
        to_record.written.push(path);
    }
    let recorded = restore::record_applied(
        &recorder,
        &tree,
        &to_record,
        me,
        RecordMode::Chunked(RECORD_CHUNK),
    );
    guard.finish();
    let report_commit = committer
        .commit_pending()
        .context("committing the restore")?;
    done.written = applied.written.len();
    done.removed = applied.removed.len();
    done.commit = (report_commit.commits > 0)
        .then(|| history.head().ok().flatten().map(|c| c.id().to_string()))
        .flatten();
    let code = match (failure, recorded) {
        (None, Ok(())) => {
            report(&done);
            ExitCode::SUCCESS
        }
        (failure, recorded) => {
            report(&done);
            if let Some(f) = failure {
                eprintln!("{command} failed part way: {f}; what was written is recorded");
            }
            if let Err(e) = recorded {
                eprintln!("{command}: {e}");
            }
            ExitCode::FAILURE
        }
    };
    session.finish();
    Ok(code)
}

/// A refusal as the control socket carries it.
fn refusal_reply(error: &RestoreError) -> Reply {
    Reply::Refused {
        kind: error.kind().to_owned(),
        message: error.to_string(),
        hint: error.hint().map(str::to_owned),
    }
}

/// The daemon side of mount mode: serves restores and reverts that arrive
/// on the control socket, writing through the mount.
pub struct Daemon {
    pub state: StateDir,
    pub recorder: Recorder,
    pub view: MountView,
    pub commit_window: Duration,
}

impl Daemon {
    /// Serves one request, on the connection's own thread.
    pub fn serve(&self, request: control::Request, peer: control::Peer) -> Reply {
        let ask = match Ask::from_wire(request) {
            Ok(ask) => ask,
            Err(e) => return refusal_reply(&e),
        };
        match self.serve_ask(&ask, peer) {
            Ok(done) => Reply::Ok(done),
            Err(e) if e.is_refusal() => refusal_reply(&e),
            Err(e) => Reply::Error(e.to_string()),
        }
    }

    /// How long to wait for history to catch up with the tree.
    fn history_wait(&self) -> Duration {
        (self.commit_window * 20).max(MIN_HISTORY_WAIT)
    }

    fn serve_ask(&self, ask: &Ask, peer: control::Peer) -> Result<Outcome, RestoreError> {
        // Everything recorded before the request must be in history before
        // the tree is compared with it.
        if let Some(seq) = self.recorder.last_seq()
            && !self.recorder.wait_committed(seq, self.history_wait())
        {
            return Err(RestoreError::HistoryBehind);
        }
        let history = History::open(&self.state.git()).map_err(other)?;
        let repo = history.repo();
        let tree = Dir::open(self.view.mountpoint())?;
        let plan = restore::plan(repo, &history.settings().branch_ref, &ask.request)?;
        let live: &dyn Liveness = &self.view;
        let objects = Objects::new(&self.state, history.settings(), ask.dry_run)?;
        let plan = restore::check(
            plan,
            repo,
            &self.recorder,
            objects.get(),
            &tree,
            live,
            ask.force,
        )?;
        let mut done = Outcome {
            plan: plan_lines(&plan),
            dry_run: ask.dry_run,
            lfs_uncached: objects.uncached().0,
            lfs_uncached_bytes: objects.uncached().1,
            ..Outcome::default()
        };
        if ask.dry_run || plan.steps.is_empty() {
            return Ok(done);
        }
        // FUSE reports the calling thread's id as the pid; the process id
        // is registered too in case a kernel reports the thread group.
        let tid = rustix::thread::gettid().as_raw_nonzero().get() as u32;
        let guard = self
            .recorder
            .begin_operation(
                plan.kind,
                plan.source.to_string(),
                plan.summary.clone(),
                Caller {
                    uid: peer.uid,
                    gid: peer.gid,
                    pid: peer.pid,
                },
                &[tid, std::process::id()],
            )
            .map_err(other)?;
        let result = restore::apply(&plan, repo, objects.get(), &tree, live, ask.force);
        // Databases (design §8): each group the operation changed is
        // recorded whole now, as part of it; nothing recorded them before.
        let captured = self.view.capture_operation();
        let last = guard.finish();
        let applied = match &result {
            Ok(applied) => applied.clone(),
            Err(e) => e.applied.clone(),
        };
        done.written = applied.written.len();
        done.removed = applied.removed.len();
        if let Some(last) = last
            && self.recorder.wait_committed(last, self.history_wait())
        {
            done.commit = commit_with_seq(&history, last);
        }
        match result {
            Ok(_) => match captured {
                Ok(_) => Ok(done),
                Err(e) => Err(other(format!(
                    "the {} path(s) were written, but recording a database group failed: {e}; \
                     it is recorded by the next capture",
                    done.written + done.removed
                ))),
            },
            Err(e) => Err(other(format!(
                "failed part way: {e}; the {} path(s) written and {} removed before it are \
                 recorded",
                done.written, done.removed
            ))),
        }
    }
}

/// How long the history service waits for every live front to be
/// listening before it refuses a restore as busy.
pub const LISTEN_GRACE: Duration = Duration::from_secs(5);

/// The history service's side of restores and reverts (design §15): it
/// holds the state area, so it writes the backing tree itself, live, with
/// every front's help ([`Fronts`]):
///
/// 1. every slot whose lease is held must have a listening front (else
///    `busy`, exit 75: a front that cannot be settled or told to drop its
///    caches would serve stale content);
/// 2. each front records what is dirty in it (a front that fails to is
///    `busy` too), and history catches up; a dry run settles the same
///    way, so it checks what a real run would;
/// 3. the plan is checked against the backing tree, with the fronts
///    saying which paths are open for writing (refused without `--force`);
/// 4. the service writes the backing tree, asking the fronts again about
///    each path just before writing it, and records what it wrote as the
///    operation, by the operator, then commits;
/// 5. each front drops its kernel's caches of the paths written.
///
/// When the service stops (`stopping`), a restore in progress stops
/// before its next path and records what it wrote.
pub struct ServiceDaemon {
    pub state: StateDir,
    pub recorder: Recorder,
    pub fronts: Fronts,
    pub backing: PathBuf,
    pub commit_window: Duration,
    pub stopping: Arc<AtomicBool>,
}

/// Why the service could not serve a restore.
enum Failure {
    Restore(RestoreError),
    /// A front could not be called: retry later (exit 75).
    Busy(String),
}

impl From<RestoreError> for Failure {
    fn from(e: RestoreError) -> Self {
        Self::Restore(e)
    }
}

impl From<FrontsError> for Failure {
    fn from(e: FrontsError) -> Self {
        match e {
            FrontsError::Call { .. } | FrontsError::NotListening(_) => Self::Busy(format!(
                "{e}; the restore did not start: retry once the front answers"
            )),
            FrontsError::Leases(e) => Self::Restore(RestoreError::Io(e)),
        }
    }
}

impl ServiceDaemon {
    /// Serves one request, on the connection's own thread.
    pub fn serve(&self, request: control::Request, peer: control::Peer) -> Reply {
        let ask = match Ask::from_wire(request) {
            Ok(ask) => ask,
            Err(e) => return refusal_reply(&e),
        };
        if let Err(e) = self.fronts.require_all(LISTEN_GRACE) {
            return match Failure::from(e) {
                Failure::Busy(message) => Reply::Busy(message),
                Failure::Restore(e) => Reply::Error(e.to_string()),
            };
        }
        match self.serve_ask(&ask, peer) {
            Ok(done) => Reply::Ok(done),
            Err(Failure::Busy(message)) => Reply::Busy(message),
            Err(Failure::Restore(e)) if e.is_refusal() => refusal_reply(&e),
            Err(Failure::Restore(e)) => Reply::Error(e.to_string()),
        }
    }

    fn history_wait(&self) -> Duration {
        (self.commit_window * 20).max(MIN_HISTORY_WAIT)
    }

    fn serve_ask(&self, ask: &Ask, peer: control::Peer) -> Result<Outcome, Failure> {
        // What the fronts have not recorded yet, then everything recorded,
        // must be in history before the tree is compared with it.
        let waiting = self.fronts.settle()?;
        if waiting > 0 {
            eprintln!(
                "{}: {waiting} path(s) still wait for a front's repair; a plan that touches \
                 them is refused as not recorded",
                ask.command()
            );
        }
        if let Some(seq) = self.recorder.last_seq()
            && !self.recorder.wait_committed(seq, self.history_wait())
        {
            return Err(RestoreError::HistoryBehind.into());
        }
        let history = History::open(&self.state.git()).map_err(other)?;
        let repo = history.repo();
        let tree = Dir::open(&self.backing).map_err(RestoreError::from)?;
        let plan = restore::plan(repo, &history.settings().branch_ref, &ask.request)?;
        let asked = FrontsLiveness::asked(&self.fronts, &self.stopping);
        asked.ask(plan.steps.iter().map(|s| &s.path))?;
        let objects = Objects::new(&self.state, history.settings(), ask.dry_run)
            .map_err(RestoreError::from)?;
        let plan = restore::check(
            plan,
            repo,
            &self.recorder,
            objects.get(),
            &tree,
            &asked,
            ask.force,
        )?;
        let mut done = Outcome {
            plan: plan_lines(&plan),
            dry_run: ask.dry_run,
            lfs_uncached: objects.uncached().0,
            lfs_uncached_bytes: objects.uncached().1,
            ..Outcome::default()
        };
        if ask.dry_run || plan.steps.is_empty() {
            return Ok(done);
        }
        // From here on the fronts are asked about each path just before
        // it is written: a file opened since the check is refused by the
        // recheck of its step.
        let live = FrontsLiveness::per_step(&self.fronts, &self.stopping);
        let operator = Caller {
            uid: peer.uid,
            gid: peer.gid,
            pid: peer.pid,
        };
        // The service records what it writes, as the operation's actor.
        let me = Caller {
            pid: std::process::id(),
            ..operator
        };
        let guard = self
            .recorder
            .begin_operation(
                plan.kind,
                plan.source.to_string(),
                plan.summary.clone(),
                operator,
                &[me.pid],
            )
            .map_err(other)?;
        let (applied, failure) =
            match restore::apply(&plan, repo, objects.get(), &tree, &live, ask.force) {
                Ok(applied) => (applied, None),
                Err(e) => (e.applied.clone(), Some(e)),
            };
        let mut to_record = applied.clone();
        if let Some(path) = failure
            .as_ref()
            .and_then(|e| e.left_changed(&plan, repo, &tree))
        {
            to_record.written.push(path);
        }
        let recorded = restore::record_applied(
            &self.recorder,
            &tree,
            &to_record,
            me,
            RecordMode::Chunked(RECORD_CHUNK),
        );
        let last = guard.finish();
        // The fronts' kernels drop what they cached of the paths written,
        // committed or not: the tree has changed either way.
        let changed: Vec<RepoPath> = to_record
            .written
            .iter()
            .chain(&to_record.removed)
            .cloned()
            .collect();
        if let Err(e) = self.fronts.invalidate(&changed) {
            eprintln!(
                "{}: a front could not drop its caches of the paths written ({e}); it may \
                 show their old content until its kernel's cache times out",
                ask.command()
            );
        }
        done.written = applied.written.len();
        done.removed = applied.removed.len();
        if let Some(last) = last
            && self.recorder.wait_committed(last, self.history_wait())
        {
            done.commit = commit_with_seq(&history, last);
        }
        match (failure, recorded) {
            (None, Ok(())) => Ok(done),
            (Some(e), _) => Err(other(format!(
                "failed part way: {e}; the {} path(s) written and {} removed before it are \
                 recorded",
                done.written, done.removed
            ))
            .into()),
            (None, Err(e)) => Err(other(format!(
                "the {} path(s) were written, but recording them failed: {e}",
                done.written + done.removed
            ))
            .into()),
        }
    }
}

/// Which paths the fronts hold open for writing ([`Fronts::open_for_writing`]).
trait AskFronts {
    fn open_for_writing(&self, paths: &[RepoPath]) -> Result<Vec<bool>, FrontsError>;
}

impl AskFronts for Fronts {
    fn open_for_writing(&self, paths: &[RepoPath]) -> Result<Vec<bool>, FrontsError> {
        Fronts::open_for_writing(self, paths)
    }
}

/// Which paths the fronts hold open for writing, for [`restore::check`]
/// and [`restore::apply`].
///
/// - [`FrontsLiveness::asked`], for the check: every front is asked about
///   the plan's paths in batches ([`FrontsLiveness::ask`]), and answers
///   come from that.
/// - [`FrontsLiveness::per_step`], for the apply: every front is asked
///   about each path when the apply rechecks it, just before writing it,
///   so a file opened during a long apply is seen (one round trip per
///   front and path).
///
/// A path not asked about, or asked when a front failed to answer, counts
/// as open: the safe answer, refused without `--force`.
struct FrontsLiveness<'a> {
    fronts: &'a dyn AskFronts,
    /// The batch answers; `None`: ask at each call.
    asked: Option<Mutex<HashMap<RepoPath, bool>>>,
    stopping: &'a AtomicBool,
}

impl<'a> FrontsLiveness<'a> {
    fn asked(fronts: &'a dyn AskFronts, stopping: &'a AtomicBool) -> Self {
        Self {
            fronts,
            asked: Some(Mutex::new(HashMap::new())),
            stopping,
        }
    }

    fn per_step(fronts: &'a dyn AskFronts, stopping: &'a AtomicBool) -> Self {
        Self {
            fronts,
            asked: None,
            stopping,
        }
    }

    fn ask<'p>(&self, paths: impl Iterator<Item = &'p RepoPath>) -> Result<(), FrontsError> {
        let Some(asked) = &self.asked else {
            return Ok(());
        };
        let paths: Vec<RepoPath> = paths.cloned().collect();
        let answers = self.fronts.open_for_writing(&paths)?;
        let mut asked = asked.lock().unwrap_or_else(PoisonError::into_inner);
        asked.extend(paths.into_iter().zip(answers));
        Ok(())
    }
}

impl Liveness for FrontsLiveness<'_> {
    fn open_for_writing(&self, path: &RepoPath) -> bool {
        match &self.asked {
            Some(asked) => asked
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(path)
                .copied()
                .unwrap_or(true),
            None => match self.fronts.open_for_writing(std::slice::from_ref(path)) {
                Ok(open) => open.first().copied().unwrap_or(true),
                Err(e) => {
                    eprintln!("asking the fronts whether {path} is open for writing: {e}");
                    true
                }
            },
        }
    }

    /// The service writes the backing tree directly and records itself.
    fn records_its_writes(&self) -> bool {
        false
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }
}

fn other(e: impl ToString) -> RestoreError {
    RestoreError::Io(std::io::Error::other(e.to_string()))
}

/// The commit on the branch whose `Agentfs-Seq` is `seq`, searched back
/// from the head.
fn commit_with_seq(history: &History, seq: u64) -> Option<String> {
    let mut commit = history.head().ok()??;
    for _ in 0..10_000 {
        match seq_from_message(commit.message_raw().unwrap_or_default()) {
            Some(s) if s == seq => return Some(commit.id().to_string()),
            Some(s) if s < seq => return None,
            _ => {}
        }
        commit = commit.parent(0).ok()?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(op: Op, path: Option<&str>) -> control::Request {
        control::Request {
            v: control::PROTOCOL_VERSION,
            op,
            path: path.map(str::to_owned),
            commit: "HEAD~5".into(),
            force: false,
            dry_run: false,
            remotes: Vec::new(),
            timeout_ms: None,
        }
    }

    /// Fronts whose open files change between calls.
    #[derive(Default)]
    struct FakeFronts {
        open: Mutex<Vec<String>>,
        fail: AtomicBool,
        calls: Mutex<usize>,
    }

    impl AskFronts for FakeFronts {
        fn open_for_writing(&self, paths: &[RepoPath]) -> Result<Vec<bool>, FrontsError> {
            *self.calls.lock().unwrap() += 1;
            if self.fail.load(Ordering::SeqCst) {
                return Err(FrontsError::Call {
                    slot: 1,
                    pod: "pod-a".into(),
                    message: "gone".into(),
                });
            }
            let open = self.open.lock().unwrap();
            Ok(paths
                .iter()
                .map(|p| open.contains(&p.to_string()))
                .collect())
        }
    }

    #[test]
    fn the_apply_asks_the_fronts_at_each_step() {
        let fronts = FakeFronts::default();
        let stopping = AtomicBool::new(false);
        let a = RepoPath::new("a").unwrap();
        let asked = FrontsLiveness::asked(&fronts, &stopping);
        asked.ask([&a].into_iter()).unwrap();
        assert!(!asked.open_for_writing(&a));
        // Not asked about: open.
        assert!(asked.open_for_writing(&RepoPath::new("b").unwrap()));
        // A pod opens `a` after the batch was asked: the check's answers
        // are from before, the apply's are asked anew.
        fronts.open.lock().unwrap().push("a".into());
        assert!(!asked.open_for_writing(&a));
        let live = FrontsLiveness::per_step(&fronts, &stopping);
        let before = *fronts.calls.lock().unwrap();
        assert!(live.open_for_writing(&a));
        assert_eq!(*fronts.calls.lock().unwrap(), before + 1);
        fronts.open.lock().unwrap().clear();
        assert!(!live.open_for_writing(&a));
        // A front that cannot answer: open.
        fronts.fail.store(true, Ordering::SeqCst);
        assert!(live.open_for_writing(&a));
        assert!(matches!(
            Failure::from(asked.ask([&a].into_iter()).unwrap_err()),
            Failure::Busy(_)
        ));
        assert!(!live.stopping());
        stopping.store(true, Ordering::SeqCst);
        assert!(live.stopping());
    }

    #[test]
    fn a_restore_request_must_name_its_path() {
        let err = Ask::from_wire(wire(Op::Restore, None)).err().unwrap();
        assert!(matches!(err, RestoreError::MissingPath), "{err}");
        assert!(err.is_refusal());
        let whole = Ask::from_wire(wire(Op::Restore, Some("."))).unwrap();
        assert!(matches!(
            whole.request,
            Request::Restore {
                spec: Pathspec::Root,
                ..
            }
        ));
        let revert = Ask::from_wire(wire(Op::Revert, None)).unwrap();
        assert!(matches!(revert.request, Request::Revert { .. }));
    }
}
