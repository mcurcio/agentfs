//! `agentfs checkpoint` (design §17.1, §19.2 item 11; bead agentfs-mia.6):
//! one change-line commit per intent ([`agentfs_core::checkpoint`]).
//!
//! The command asks a running daemon first (the control socket's
//! `checkpoint`): its commit loop commits what is recorded and cuts the
//! checkpoint between commits, so the branch keeps one writer
//! ([`ServiceHandle::checkpoint`]). With no daemon it takes the state-area
//! lock (exit 75 while another command holds it), commits what is
//! journaled, then cuts. The author is the caller: the socket peer's ids,
//! or the command's own.
//!
//! Exit 0 for a checkpoint cut or nothing new; 1 when it failed.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;

use agentfs_core::checkpoint::{self, CheckpointOutcome, CheckpointRequest};
use agentfs_core::lock::StateLock;
use agentfs_core::record::Caller;
use agentfs_core::service::{CheckpointNowError, ServiceHandle};
use agentfs_core::volume::StateDir;

use crate::control::{self, ClientError, Peer, Reply, Request};
use crate::{EXIT_LOCKED, Pod, Session, current_gid, current_uid};

/// How long the daemon waits for its commit loop to cut a checkpoint.
pub const TIMEOUT: Duration = Duration::from_secs(120);

/// `agentfs checkpoint [-m MESSAGE]`.
pub fn run(
    state: &StateDir,
    root: Option<&Path>,
    wait_lock: Duration,
    pod: &Pod,
    message: Option<String>,
) -> Result<ExitCode> {
    crate::refuse_finished(state)?;
    if let Some(code) = client(state, message.clone()) {
        return Ok(code);
    }
    let _lock = StateLock::acquire(state, "checkpoint", wait_lock)?;
    let session = Session::start(state, pod)?;
    let (_, mut committer, _) = session.open(state, root.unwrap_or(Path::new(".")), pod)?;
    // Without --root the tree is a guess: no commit-time probes of it.
    committer.set_tree_known(root.is_some());
    let result = committer
        .commit_pending()
        .map_err(anyhow::Error::from)
        .and_then(|_| {
            let request = CheckpointRequest {
                message,
                caller: Caller {
                    uid: current_uid(),
                    gid: current_gid(),
                    pid: std::process::id(),
                },
            };
            Ok(checkpoint::cut(&committer, &request)?)
        });
    session.finish();
    Ok(match result? {
        CheckpointOutcome::Cut { change, marker, .. } => {
            print_cut(&change.to_string(), &marker.to_string())
        }
        CheckpointOutcome::NothingNew => print_nothing_new(),
    })
}

fn print_cut(change: &str, marker: &str) -> ExitCode {
    println!("checkpoint {change}, merged into the branch by {marker}");
    ExitCode::SUCCESS
}

fn print_nothing_new() -> ExitCode {
    println!("nothing new since the last checkpoint");
    ExitCode::SUCCESS
}

/// `agentfs checkpoint` through a running daemon: its exit status, or
/// `None` when no daemon answers (no socket, a reply this build cannot
/// read, or a daemon from before `checkpoint`), and the command takes the
/// lock path.
fn client(state: &StateDir, message: Option<String>) -> Option<ExitCode> {
    let reply = match control::call(&state.control_dir(), &Request::checkpoint(message)) {
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
        Reply::Checkpoint {
            change: Some(change),
            marker: Some(marker),
        } => print_cut(&change, &marker),
        Reply::Checkpoint { .. } => print_nothing_new(),
        // A daemon from before `checkpoint`: the lock path, which refuses.
        Reply::Error(message) if message.contains("unknown variant") => return None,
        Reply::Error(message) => {
            eprintln!("checkpoint failed: {message}");
            ExitCode::FAILURE
        }
        Reply::Refused { message, hint, .. } => {
            eprintln!("checkpoint refused: {message}");
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
        | Reply::Pushed(_)
        | Reply::Received { .. }
        | Reply::Attached { .. }
        | Reply::Judged { .. } => {
            eprintln!("checkpoint failed: the daemon answered another request's reply");
            ExitCode::FAILURE
        }
    })
}

/// The control socket's `checkpoint`: the service's commit loop cuts it,
/// authored by the peer.
pub fn serve(service: &ServiceHandle, request: &Request, peer: Peer) -> Reply {
    let request = CheckpointRequest {
        message: request.message.clone(),
        caller: Caller {
            uid: peer.uid,
            gid: peer.gid,
            pid: peer.pid,
        },
    };
    match service.checkpoint(request, TIMEOUT) {
        Ok(CheckpointOutcome::Cut { change, marker, .. }) => Reply::Checkpoint {
            change: Some(change.to_string()),
            marker: Some(marker.to_string()),
        },
        Ok(CheckpointOutcome::NothingNew) => Reply::Checkpoint {
            change: None,
            marker: None,
        },
        Err(e @ (CheckpointNowError::Stopping | CheckpointNowError::TimedOut)) => {
            Reply::Error(e.to_string())
        }
        Err(CheckpointNowError::Failed(message)) => Reply::Error(message),
    }
}
