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
//! - **Direct mode** (nothing listening): no mount, and so no workload
//!   through one, can be running. The command takes the state-area lock,
//!   commits what is journaled, writes the backing tree (`--root`) itself
//!   and records what it wrote, as one operation.
//!
//! Exit status: 0 done (or already matching), 1 refused or failed, 75 the
//! state area is busy (another command holds it) or the daemon is running
//! another restore.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context as _, Result};

use agentfs_core::confined::{self, Dir, Kind};
use agentfs_core::history::{History, seq_from_message};
use agentfs_core::lfs::fetch::Fetcher;
use agentfs_core::lock::{LockError, StateLock};
use agentfs_core::record::{Caller, Trigger};
use agentfs_core::restore::{
    self, Applied, Liveness, NotLive, Pathspec, Plan, Request, RestoreError,
};
use agentfs_core::volume::{Input, Recorder, StateDir};
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
        }
    }

    fn command(&self) -> &'static str {
        match self.request {
            Request::Restore { .. } => "restore",
            Request::Revert { .. } => "revert",
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
    let objects = Fetcher::new(state, history.settings()).context("opening the LFS cache")?;
    let checked = restore::plan(history.repo(), &history.settings().branch_ref, &ask.request)
        .and_then(|plan| {
            restore::check(
                plan,
                history.repo(),
                &recorder,
                &objects,
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
    let (applied, failure) =
        match restore::apply(&plan, history.repo(), &objects, &tree, &NotLive, ask.force) {
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
    let recorded = record_applied(&recorder, &tree, &to_record, me);
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

/// Records what [`restore::apply`] changed, from confined descriptors (so
/// no symlink is ever followed), as records of the running operation.
fn record_applied(
    recorder: &Recorder,
    tree: &Dir,
    applied: &Applied,
    caller: Caller,
) -> Result<(), String> {
    let paths: Vec<(&agentfs_core::paths::RepoPath, bool)> = applied
        .written
        .iter()
        .map(|p| (p, true))
        .chain(applied.removed.iter().map(|p| (p, false)))
        .collect();
    let mut problems = Vec::new();
    for chunk in paths.chunks(RECORD_CHUNK) {
        let mut fds = Vec::new();
        let mut inputs = Vec::with_capacity(chunk.len());
        for (path, written) in chunk {
            let path = (*path).clone();
            if !written {
                inputs.push(Input::Remove { path });
                continue;
            }
            match tree.stat_at(path.as_str()) {
                Ok(Some((fd, st))) => match confined::kind_of(&st) {
                    Kind::File => {
                        inputs.push(Input::Write {
                            path,
                            backing: confined::proc_path(&fd),
                            executable: st.st_mode & 0o111 != 0,
                        });
                        fds.push(fd);
                    }
                    Kind::Symlink => match confined::read_link_fd(&fd).map(String::from_utf8) {
                        Ok(Ok(target)) => inputs.push(Input::Symlink { path, target }),
                        _ => problems.push(format!("{path}: unreadable symlink")),
                    },
                    _ => problems.push(format!("{path}: changed while restoring")),
                },
                Ok(None) => inputs.push(Input::Remove { path }),
                Err(e) => problems.push(format!("{path}: {e}")),
            }
        }
        match recorder.record(Trigger::Restore, caller, inputs) {
            Ok(recorded) => {
                for skipped in recorded.skipped {
                    problems.push(format!(
                        "{} was written but not recorded: {}",
                        skipped.path, skipped.reason
                    ));
                }
            }
            Err(e) => problems.push(format!("recording failed: {e}")),
        }
        drop(fds);
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
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
        let objects = Fetcher::new(&self.state, history.settings())?;
        let plan = restore::check(plan, repo, &self.recorder, &objects, &tree, live, ask.force)?;
        let mut done = Outcome {
            plan: plan_lines(&plan),
            dry_run: ask.dry_run,
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
        let result = restore::apply(&plan, repo, &objects, &tree, live, ask.force);
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
            Ok(_) => Ok(done),
            Err(e) => Err(other(format!(
                "failed part way: {e}; the {} path(s) written and {} removed before it are \
                 recorded",
                done.written, done.removed
            ))),
        }
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
        }
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
