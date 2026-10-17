//! Starting a volume from a remote branch (F8 S1; R2: continuing a
//! volume elsewhere is a new volume started from the old branch).
//!
//! [`start_from`] makes a new worktree whose history begins at a pinned
//! commit of another worktree's branch: it fetches that branch once from
//! the start remote (a push section of the new volume's own profile, with
//! its credentials), checks that the pinned commit is the branch's tip or
//! an ancestor of it, writes the identity (`agentfs.volume`, its own
//! branch, and the three `agentfs.start.*` keys) into the new volume's own
//! config, points the new branch at the commit, and writes the commit's
//! tree into the backing tree through the same confined writer as
//! [`super::create::create`]. LFS objects the tree references are
//! downloaded from the start remote's LFS endpoint. It writes nothing to
//! any remote, and never the start branch, which belongs to another
//! worktree (rule 1). Idempotent per name: a retry (CreateVolume retried)
//! resumes, and a volume that exists keeps the start point it was made
//! with.
//!
//! [`accept`] is the operator's answer to the start-branch alarm
//! ([`crate::push::StartBranchMoved`]): it saves an old-branch tip under
//! this volume's own `refs/agentfs/<volume>/accepted/<tip>`, after which
//! the alarm stops watching every commit that tip reaches. It takes no
//! state lock (it creates one ref and adds objects, both atomic in git),
//! so it runs beside a running history service.

use std::io;
use std::path::{Path, PathBuf};

use git2::Oid;

use crate::config::ConfigError;
use crate::history::HistoryError;
use crate::volume::{StateDir, VolumeError};

use super::identity::StartPoint;

/// What to start.
#[derive(Debug, Clone, Copy)]
pub struct StartRequest<'a> {
    /// The new worktree's state area: new, or this volume's from an
    /// earlier try.
    pub state: &'a StateDir,
    /// The new worktree's backing tree: empty, or this volume's from an
    /// earlier try.
    pub tree: &'a Path,
    /// The new volume's name, unique per PV (`{pvName}`, rule 1).
    pub volume: &'a str,
    /// The new volume's `include.path` entries: its profile's config,
    /// which names `start.remote`'s section and credentials.
    pub includes: &'a [PathBuf],
    pub start: &'a StartPoint,
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// `start.remote` is not a valid push section of the new volume's
    /// profile (CreateVolume: `InvalidArgument`).
    #[error(
        "startFrom names remote {remote:?}, which is not a push remote of the volume's profile"
    )]
    NoSuchRemote { remote: String },
    /// The profile declares no remote (F8 S3): there is nothing to start
    /// from (`InvalidArgument`).
    #[error(
        "the volume's profile is local-only (agentfs.remotes = none): it has no remote to start from"
    )]
    LocalOnly,
    /// The start branch is the new volume's own branch (rule 1;
    /// `InvalidArgument`).
    #[error("startFrom names branch {branch:?}, which is this volume's own branch")]
    OwnBranch { branch: String },
    /// The remote's branch does not reach the pinned commit
    /// (`FailedPrecondition`).
    #[error("commit {commit} is not on {branch} at the start remote")]
    NotReachable { commit: Oid, branch: String },
    /// The start remote could not be fetched (`Unavailable`: the CO
    /// retries).
    #[error("fetching from remote {remote}: {message}")]
    Unreachable { remote: String, message: String },
    /// The start branch does not exist on the remote
    /// (`FailedPrecondition`).
    #[error("remote {remote} has no branch {branch}")]
    NoSuchBranch { remote: String, branch: String },
    /// An LFS object of the pinned tree could not be downloaded
    /// (`Unavailable`).
    #[error("LFS object of {path} could not be downloaded from the start remote: {message}")]
    LfsObjectMissing { path: String, message: String },
    /// The state area belongs to another volume or start point
    /// (`AlreadyExists`).
    #[error("the state area already belongs to {found}; nothing was changed")]
    Exists { found: String },
    #[error("reading the volume's settings: {0}")]
    Settings(ConfigError),
    #[error(transparent)]
    Volume(#[from] VolumeError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Starts the volume `request` names, or returns the start point an
/// earlier call created it with (see the module docs).
pub fn start_from(request: &StartRequest<'_>) -> Result<StartPoint, StartError> {
    let _ = request;
    todo!("F8 S1")
}

#[derive(Debug, thiserror::Error)]
pub enum AcceptError {
    /// The volume has no start point.
    #[error("volume {volume} was not started from another branch: there is nothing to accept")]
    NotStarted { volume: String },
    #[error("the start remote {remote} is not a push remote of this volume")]
    NoSuchRemote { remote: String },
    #[error("fetching {branch} from remote {remote}: {message}")]
    Unreachable {
        remote: String,
        branch: String,
        message: String,
    },
    /// `tip` is not the start branch's tip or an ancestor of it on the
    /// remote: only commits of the start branch are accepted.
    #[error("commit {tip} is not on {branch} at remote {remote}")]
    NotOnStartBranch {
        tip: Oid,
        remote: String,
        branch: String,
    },
    #[error("reading the volume's settings: {0}")]
    Settings(ConfigError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Saves `tip`, a commit of this volume's start branch, as
/// `refs/agentfs/<volume>/accepted/<tip>` (created, never moved; a second
/// accept of the same tip is a no-op), fetching its objects from the
/// start remote first. Returns the ref's name. The next push to the start
/// remote pushes it create-only with the branch and no longer raises the
/// alarm for commits `tip` reaches.
pub fn accept(state: &StateDir, tip: Oid) -> Result<String, AcceptError> {
    let _ = (state, tip);
    todo!("F8 S1")
}
