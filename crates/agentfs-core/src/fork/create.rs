//! Creating a fork (design §19.1, "Forked volumes"): from a source's state
//! area, read without the source's lock and with no credentials, so the
//! CSI controller can run it inside CreateVolume.
//!
//! [`create`] reads the source's settings and branch head (the base). It
//! refuses a source that is a fork or that has no fork remote. It then
//! makes the fork's state area: the identity in the fork's own config,
//! the source's settings files as its includes, the objects of base's
//! history copied in (no alternates, so a fork outlives its source), the
//! fork's history ref at base, and base's tree written into the fork's
//! backing tree before anything can mount it. It is idempotent per name:
//! a fork that already exists with the same source is returned as it is,
//! with the base it was created with.

use std::path::{Path, PathBuf};

use crate::config::ConfigError;
use crate::history::HistoryError;
use crate::volume::{StateDir, VolumeError};

use super::identity::{ForkIdentity, PushTarget};

/// What to create.
#[derive(Debug, Clone, Copy)]
pub struct CreateRequest<'a> {
    /// The source's state area, only read: no lock, no commit, never
    /// paused.
    pub source: &'a StateDir,
    /// The fork's state area: new, or this fork's from an earlier try.
    pub fork: &'a StateDir,
    /// The fork's backing tree: empty, or this fork's from an earlier try.
    pub tree: &'a Path,
    /// The fork's volume name, unique at create: [`unique_name`] from the
    /// CLI, the class's `volumeName` (which names `{pvName}`) under CSI.
    pub name: &'a str,
    /// The fork's `include.path` entries: the files the source's settings
    /// come from (under CSI, its profile's config), read at every open.
    pub includes: &'a [PathBuf],
    /// Where the fork's history goes.
    pub push_target: PushTarget,
}

#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    #[error("not built yet: forked volumes unit 1 (design §19.1)")]
    NotBuilt,
    #[error("{} is not a volume's state area", path.display())]
    SourceNotFound { path: PathBuf },
    #[error("reading the source's settings: {0}")]
    SourceSettings(ConfigError),
    #[error("reading the source's history: {0}")]
    SourceUnreadable(HistoryError),
    #[error("volume {volume} has no history yet: there is nothing to fork")]
    SourceHasNoHistory { volume: String },
    #[error("volume {volume} is itself a fork; forks of forks are not supported")]
    SourceIsFork { volume: String },
    #[error(
        "volume {volume} has no fork remote: forks push to its patch remote, or to the one \
         remote with agentfsForkRemote = true (design §19.1)"
    )]
    NoForkRemote { volume: String },
    #[error(
        "volume {volume} sets {keys:?} in its own config; a fork reads settings only from \
         included files: move them to a fragment named by include.path"
    )]
    SourceSettingsNotIncluded { volume: String, keys: Vec<String> },
    #[error("{name:?} is not a valid fork name: {why}")]
    BadName { name: String, why: String },
    #[error("the fork's state area already belongs to {found}; nothing was changed")]
    Exists { found: String },
    #[error("LFS object {oid} of {path} is not in the source's cache; nothing to fork it from")]
    LfsObjectMissing { path: String, oid: String },
    #[error(transparent)]
    Volume(#[from] VolumeError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Creates the fork `request` names, or returns the one an earlier call
/// created with the same name and source (see the module docs).
pub fn create(request: &CreateRequest<'_>) -> Result<ForkIdentity, CreateError> {
    let _ = request;
    Err(CreateError::NotBuilt)
}

/// `<prefix>-<8 random hex digits>`: a fork name the CLI makes unique on
/// the remote by construction. The result is a valid volume name, or
/// [`CreateError::BadName`].
pub fn unique_name(prefix: &str) -> Result<String, CreateError> {
    let _ = prefix;
    Err(CreateError::NotBuilt)
}
