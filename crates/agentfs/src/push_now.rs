//! `agentfs push` against a running daemon (design §2.3, §11.2).
//!
//! `mount`, `run` and `serve-history` serve the control socket
//! ([`crate::control`]); [`handler`] answers its requests:
//!
//! - **push**: [`ServiceHandle::push_now`] commits everything recorded
//!   (but a restore still running), wakes each chosen remote's push loop
//!   past its interval or backoff, and waits up to the request's timeout
//!   (clamped to [`MIN_TIMEOUT`]..=[`MAX_TIMEOUT`]). The reply carries the
//!   lines `agentfs push` prints, rendered by the daemon so both modes
//!   print the same. Misconfigured remotes are reported, never pushed.
//! - **restore and revert**: served by a mount, through itself, and by
//!   `serve-history`, into the backing tree with its fronts' help
//!   ([`crate::recovery`]); `run` answers `busy` with the reason, so the
//!   exit status stays 75 as when it held the lock without a socket.
//!
//! - **receive_fork**: a fork of this local-only volume publishes into its
//!   history through the daemon that holds its lock
//!   ([`crate::fork_cmd::serve_receive`], F8 S3).
//! - **apply**: `agentfs patch apply` while a daemon holds the volume
//!   ([`crate::patch_cmd::serve_apply`], bead agentfs-mia.5): a running
//!   daemon always counts as attached, so the queue is judged and the
//!   integrator notified, and nothing lands until the next mount.
//!
//! - **checkpoint**: `agentfs checkpoint` while a daemon holds the volume
//!   ([`crate::checkpoint_cmd::serve`], bead agentfs-mia.6): the service's
//!   commit loop cuts it ([`ServiceHandle::checkpoint`]).
//!
//! [`client`] is the command's side: it asks the daemon first and returns
//! `None` when none answers (no socket, or a daemon from before `push`
//! existed), and the command then pushes directly, taking the lock.
//!
//! `agentfs push --force-with-lease` never comes here: the operator's
//! lease push (§15) is always direct, under the remote's push lock alone,
//! so it runs the same with or without a daemon and the control protocol
//! carries no way to replace a branch.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentfs_core::service::{PushNowError, PushNowReport, RemoteResult, ServiceHandle};
use agentfs_core::volume::StateDir;

use crate::control::{self, ClientError, Op, PushReply, RemoteReply, Reply, Request};
pub use crate::patch_cmd::Applier;
use crate::{EXIT_LOCKED, describe, recovery};

/// The shortest and longest wait a push request may ask for.
pub const MIN_TIMEOUT: Duration = Duration::from_secs(1);
pub const MAX_TIMEOUT: Duration = Duration::from_secs(600);
/// The wait when a request names none.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long `run` and `serve-history` wait for a request in progress when
/// they stop.
pub const CONTROL_GRACE: Duration = Duration::from_secs(5);

/// What a daemon does with restores and reverts.
pub enum Restores {
    /// A mount writes them through itself.
    Serve(recovery::Daemon),
    /// The history service writes the backing tree, calling its fronts.
    ServeFronts(recovery::ServiceDaemon),
    /// Answered `busy` with this message.
    Refuse(&'static str),
}

/// `agentfs run`'s answer to a restore.
pub const RUN_SERVES_NO_RESTORES: &str = "`agentfs run` holds the state area and serves no \
     restores; stop it (or mount the volume instead), then retry";

/// The control socket's handler for a daemon of the state area `state`
/// whose service is `service`. `misconfigured` lists the remotes the
/// settings refused, with why; `applier` is what an apply is judged with.
pub fn handler(
    state: StateDir,
    service: ServiceHandle,
    misconfigured: Vec<(String, String)>,
    restores: Restores,
    applier: Applier,
) -> control::Handler {
    Arc::new(move |request: Request, peer| match request.op {
        Op::Push => serve_push(&service, &misconfigured, &request),
        Op::ReceiveFork => crate::fork_cmd::serve_receive(&state, &request),
        Op::Apply => crate::patch_cmd::serve_apply(&state, &applier, &request),
        Op::Checkpoint => crate::checkpoint_cmd::serve(&service, &request, peer),
        Op::Restore | Op::Revert => match &restores {
            Restores::Serve(daemon) => daemon.serve(request, peer),
            Restores::ServeFronts(daemon) => daemon.serve(request, peer),
            Restores::Refuse(message) => Reply::Busy((*message).to_owned()),
        },
    })
}

fn serve_push(
    service: &ServiceHandle,
    misconfigured: &[(String, String)],
    request: &Request,
) -> Reply {
    let timeout = request
        .timeout_ms
        .map_or(DEFAULT_TIMEOUT, Duration::from_millis)
        .clamp(MIN_TIMEOUT, MAX_TIMEOUT);
    let chosen =
        |name: &str| request.remotes.is_empty() || request.remotes.iter().any(|r| r == name);
    let bad: Vec<String> = misconfigured
        .iter()
        .filter(|(name, _)| chosen(name))
        .map(|(name, error)| format!("{name}: misconfigured, not pushed: {error}"))
        .collect();
    let valid: Vec<String> = request
        .remotes
        .iter()
        .filter(|r| !misconfigured.iter().any(|(name, _)| name == *r))
        .cloned()
        .collect();
    let report = if !request.remotes.is_empty() && valid.is_empty() {
        // Only misconfigured remotes were named: nothing to push.
        PushNowReport {
            committed: Ok(Default::default()),
            remotes: Vec::new(),
            still_running: Vec::new(),
        }
    } else {
        match service.push_now(&valid, timeout) {
            Ok(report) => report,
            Err(e @ PushNowError::UnknownRemote(_)) => {
                return Reply::Refused {
                    kind: "unknown_remote".into(),
                    message: e.to_string(),
                    hint: Some(format!(
                        "configured remotes: {}",
                        crate::list(service.remotes())
                    )),
                };
            }
            Err(e @ PushNowError::Stopping) => return Reply::Error(e.to_string()),
        }
    };
    let report = follow_steps(service, report, timeout);
    Reply::Pushed(render(report, bad))
}

/// Pushes again, at once, every remote whose attempt in `report` sent
/// only part of its backlog because of its push size limit (design
/// §11.3), until each is up to date, fails, or `timeout` (from now) runs
/// out. Each remote's steps are kept in its result, in order.
fn follow_steps(
    service: &ServiceHandle,
    mut report: PushNowReport,
    timeout: Duration,
) -> PushNowReport {
    let deadline = Instant::now() + timeout;
    loop {
        let limited: Vec<String> = report
            .remotes
            .iter()
            .filter(|r| r.outcome.is_limited())
            .map(|r| r.name.clone())
            .collect();
        let left = deadline.saturating_duration_since(Instant::now());
        if limited.is_empty() || left.is_zero() {
            return report;
        }
        let Ok(next) = service.push_now(&limited, left.max(MIN_TIMEOUT)) else {
            return report;
        };
        for result in next.remotes {
            if let Some(at) = report.remotes.iter_mut().find(|r| r.name == result.name) {
                let mut steps = std::mem::take(&mut at.steps);
                steps.push(at.outcome.clone());
                *at = RemoteResult { steps, ..result };
            }
        }
        for name in next.still_running {
            report.remotes.retain(|r| r.name != name);
            if !report.still_running.contains(&name) {
                report.still_running.push(name);
            }
        }
    }
}

/// The reply for `report`.
fn render(report: PushNowReport, misconfigured: Vec<String>) -> PushReply {
    let (records, commits, commit_error) = match report.committed {
        Ok(c) => (c.records as u64, c.commits as u64, None),
        Err(e) => (0, 0, Some(e)),
    };
    PushReply {
        pid: std::process::id(),
        records,
        commits,
        commit_error,
        remotes: report.remotes.iter().map(remote_reply).collect(),
        misconfigured,
        still_running: report.still_running,
    }
}

fn remote_reply(result: &RemoteResult) -> RemoteReply {
    let mut lines: Vec<String> = result
        .steps
        .iter()
        .chain(std::iter::once(&result.outcome))
        .map(|outcome| format!("{}: {}", result.name, describe(outcome)))
        .collect();
    let mut ok = result.outcome.is_success();
    // Still limited: the timeout ran out between steps (design §11.3). The
    // remote lacks commits, so this is not success; the push loop carries
    // on with the next step at once.
    if let agentfs_core::push::PushOutcome::Pushed {
        limited: Some(left),
        ..
    } = &result.outcome
    {
        ok = false;
        lines.push(format!(
            "{}: timed out with {left} commit(s) left; the next steps continue in the background",
            result.name
        ));
    }
    if let Some(queue) = &result.queue {
        ok &= queue.error.is_none();
        lines.push(format!("{}: queue: {}", result.name, queue.summary()));
    }
    RemoteReply {
        name: result.name.clone(),
        ok,
        lines,
        unpushed: result.unpushed.map(|n| n as u64),
    }
}

/// `agentfs push` through a running daemon: its exit status, or `None`
/// when no daemon answers and the command should push directly.
pub fn client(state: &StateDir, remotes: &[String], timeout: Duration) -> Option<ExitCode> {
    let request = Request::push(remotes.to_vec(), timeout);
    let reply = match control::call(&state.control_dir(), &request) {
        Ok(reply) => reply,
        Err(ClientError::NotRunning | ClientError::Protocol(_)) => return None,
        Err(ClientError::Io(e)) => {
            eprintln!(
                "error: a daemon seems to be running, but its control socket in {} cannot be \
                 used: {e}",
                state.control_dir().display()
            );
            eprintln!("hint: run as root or as the user the daemon runs as");
            return Some(ExitCode::FAILURE);
        }
    };
    Some(match reply {
        Reply::Pushed(pushed) => print_pushed(&pushed, timeout),
        // A daemon from before `push` existed: push directly (the lock
        // refuses it while that daemon runs).
        Reply::Error(message) if message.contains("unknown variant") => return None,
        Reply::Error(message) => {
            eprintln!("push failed: {message}");
            ExitCode::FAILURE
        }
        Reply::Refused { message, hint, .. } => {
            eprintln!("push refused: {message}");
            if let Some(hint) = hint {
                eprintln!("hint: {hint}");
            }
            ExitCode::FAILURE
        }
        Reply::Busy(message) => {
            eprintln!("error: {message}");
            ExitCode::from(EXIT_LOCKED)
        }
        Reply::Ok(_)
        | Reply::Received { .. }
        | Reply::Attached { .. }
        | Reply::Checkpoint { .. } => {
            eprintln!("push failed: the daemon answered another request's reply");
            ExitCode::FAILURE
        }
    })
}

/// Prints a daemon's push reply as direct mode prints its pushes; exit 0
/// only when every chosen remote succeeded in time.
fn print_pushed(pushed: &PushReply, timeout: Duration) -> ExitCode {
    let mut ok = true;
    match &pushed.commit_error {
        None => println!(
            "through the running daemon (pid {}): {} record(s) into {} commit(s)",
            pushed.pid, pushed.records, pushed.commits
        ),
        Some(error) => {
            ok = false;
            println!(
                "through the running daemon (pid {}): commit failed: {error}; pushing what is \
                 already committed",
                pushed.pid
            );
        }
    }
    for line in &pushed.misconfigured {
        ok = false;
        eprintln!("{line}");
    }
    for remote in &pushed.remotes {
        ok &= remote.ok;
        for line in &remote.lines {
            println!("{line}");
        }
    }
    for name in &pushed.still_running {
        ok = false;
        println!("{name}: still pushing after {timeout:?}; it continues in the background");
    }
    if pushed.remotes.is_empty()
        && pushed.still_running.is_empty()
        && pushed.misconfigured.is_empty()
    {
        println!("no remotes configured");
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_render_like_direct_mode() {
        let result = RemoteResult {
            name: "origin".into(),
            outcome: agentfs_core::push::PushOutcome::NothingToPush,
            queue: None,
            unpushed: None,
            steps: Vec::new(),
        };
        let reply = remote_reply(&result);
        assert!(reply.ok);
        assert_eq!(reply.lines, ["origin: nothing to push"]);
        let failed = RemoteResult {
            outcome: agentfs_core::push::PushOutcome::Busy,
            ..result
        };
        assert!(!remote_reply(&failed).ok);
        // A stepped push cut short by the timeout is not up to date.
        let limited = RemoteResult {
            outcome: agentfs_core::push::PushOutcome::Pushed {
                from: None,
                to: git2::Oid::ZERO_SHA1,
                limited: Some(3),
            },
            ..failed
        };
        let reply = remote_reply(&limited);
        assert!(!reply.ok);
        assert!(
            reply
                .lines
                .last()
                .unwrap()
                .contains("timed out with 3 commit(s) left"),
            "{:?}",
            reply.lines
        );
    }
}
