//! `agentfs restore` and `agentfs revert` (design §15).
//!
//! Both write content from history back into the tree and record the
//! result as one commit with `Agentfs-Trigger: restore`. The planning,
//! checks and writing are `agentfs_core::restore`; this module picks the
//! mode and reports.
//!
//! - **Direct mode** (no process holds the state area, so no mount and no
//!   workload through it): the command takes the state-area lock, commits
//!   what is journaled, writes the backing tree (`--root`) itself and
//!   records what it wrote, as one operation.
//!
//! Exit status: 0 done (or already matching), 1 refused or failed, 75 the
//! state area is busy.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context as _, Result};

use agentfs_core::confined::{self, Dir, Kind};
use agentfs_core::lock::{LockError, StateLock};
use agentfs_core::record::{Caller, Trigger};
use agentfs_core::restore::{self, Applied, NotLive, Pathspec, Plan, Request, RestoreError};
use agentfs_core::volume::{Input, Recorder, StateDir};

use crate::{EXIT_LOCKED, Pod, Session, current_gid, current_uid};

/// Inputs per record when the CLI records what it wrote, as a directory
/// rename does in the FUSE layer.
const RECORD_CHUNK: usize = 4096;

/// What the operator asked for, with its flags.
pub struct Ask {
    pub request: Request,
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
            force,
            dry_run,
        })
    }

    pub fn revert(commit: String, force: bool, dry_run: bool) -> Self {
        Self {
            request: Request::Revert { commit },
            force,
            dry_run,
        }
    }

    fn command(&self) -> &'static str {
        match self.request {
            Request::Restore { .. } => "restore",
            Request::Revert { .. } => "revert",
        }
    }
}

/// What a finished restore or revert did, for the report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Done {
    /// `<action> <path>` per step.
    pub plan: Vec<String>,
    pub written: usize,
    pub removed: usize,
    /// The operation's commit, if anything changed.
    pub commit: Option<String>,
    pub dry_run: bool,
}

/// The plan as listed to the operator.
pub fn plan_lines(plan: &Plan) -> Vec<String> {
    plan.steps
        .iter()
        .map(|s| format!("{} {}", s.action(), s.path))
        .collect()
}

/// Prints `done` the same way in every mode.
pub fn report(done: &Done) {
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
            "restored {} path(s), removed {}: no commit (history already matched)",
            done.written, done.removed
        ),
        Some(commit) => println!(
            "restored {} path(s), removed {}: commit {commit}",
            done.written, done.removed
        ),
    }
}

/// Prints a refusal or failure with its hint; exit status 1.
pub fn refused(command: &str, error: &RestoreError) -> ExitCode {
    eprintln!("{command} refused: {error}");
    if let Some(hint) = error.hint() {
        eprintln!("hint: {hint}");
    }
    ExitCode::FAILURE
}

/// The process's own ids, as the operator of a direct-mode operation.
fn own_caller() -> Caller {
    Caller {
        uid: current_uid(),
        gid: current_gid(),
        pid: std::process::id(),
    }
}

/// `agentfs restore` / `agentfs revert`.
pub fn run(
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
    let checked = restore::plan(history.repo(), &history.settings().branch_ref, &ask.request)
        .and_then(|plan| {
            restore::check(plan, history.repo(), &recorder, &tree, &NotLive, ask.force)
        });
    let plan = match checked {
        Ok(plan) => plan,
        Err(e) if e.is_refusal() => {
            session.finish();
            return Ok(refused(command, &e));
        }
        Err(e) => return Err(e.into()),
    };
    let mut done = Done {
        plan: plan_lines(&plan),
        dry_run: ask.dry_run,
        ..Done::default()
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
    let (applied, failure) = match restore::apply(&plan, history.repo(), &tree) {
        Ok(applied) => (applied, None),
        Err(e) => (e.applied.clone(), Some(e.to_string())),
    };
    let recorded = record_applied(&recorder, &tree, &applied, me);
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
