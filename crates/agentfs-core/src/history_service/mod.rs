//! One history service per volume, fed by a front per pod (design §14).
//!
//! In single-process mode (the default) every mount is its own writer of
//! the branch and holds the state-area lock. With a history service, one
//! process (`agentfs serve-history`) holds the lock and is the only writer
//! of the branch and journal; each pod's mount is a *front* that freezes
//! into the shared `pending/` in a slot it leases ([`FrontLease`]) and sends
//! the record to the service, which journals it and replies once it is
//! durable. The front fails the boundary with `EIO` if no durable reply
//! comes (design §3.5). Each front also keeps a listening stream
//! ([`HistoryClient::listen`]) on which the service calls it, so the
//! service can serve restores live ([`Fronts`], design §15).
//!
//! Fronts and service speak gRPC, `agentfs.history.v2` ([`wire`]), over
//! the node-local Unix socket: two HTTP/2 connections per front, each
//! starting with `Hello`, one for its records, its questions and the
//! `grpc.health.v1` probe ([`HistoryClient::ping`]), one for its
//! listening stream.
//! Nothing is ever resent: a request that may have reached the service
//! fails ([`ClientError::maybe_journaled`]).

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

mod bridge;
mod client;
mod fronts;
mod lease;
mod listener;
mod server;
pub mod wire;

pub use client::{ClientConfig, ClientError, HistoryClient};
pub use fronts::{Fronts, FrontsError, MIN_CALL_TIMEOUT};
pub use lease::{FrontLease, LeaseError, MAX_SLOT};
pub use listener::{FrontHooks, ListenerHandle, NoHooks, RECONNECT_MAX, RECONNECT_MIN, answer};
pub use server::{HistoryServer, MAX_CONNECTIONS};

use crate::nested::View;
use crate::paths::RepoPath;
use crate::record::Origin;
use crate::stamps::FreezeStamp;

/// 3 = `draft.entry` required (design §9), so a front of another version
/// is refused at `Hello`, never at its first write; 2 = gRPC, package
/// `agentfs.history.v2`; 1 was length-prefixed JSON frames.
pub const PROTOCOL_VERSION: u32 = 3;

/// The largest message either side accepts: the journal's bound on one
/// record. Overrides tonic's 4 MiB default on both ends.
pub const MAX_MESSAGE: usize = 64 << 20;

/// What this build's service supports, in `Welcome.features`: `stamps`
/// (freeze stamps on records, design §14.1), `repair_origin` (a repair's
/// own origin on a sweep record, design §14.1), `listen` (the reversed
/// channel, design §15), `still_last` (design §3.1) and `move` (a record may
/// hold a [`crate::record::Change::Move`], design §3.1).
pub const FEATURES: &[&str] = &["stamps", "repair_origin", "listen", "still_last", "move"];

/// The service's name in `grpc.health.v1` ([`HistoryClient::ping`]).
pub const SERVICE_NAME: &str = "agentfs.history.v2.History";

/// Who a front is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontInfo {
    /// Its pod's metadata, journaled with each of its records (design §9).
    pub origin: Origin,
    /// The pending slot it leases ([`FrontLease`]).
    pub slot: u16,
    /// Random per front process, so the service can count fronts.
    pub instance: u64,
    pub pid: u32,
    /// The front's agentfs version, for the service's log.
    pub version: String,
    /// Where its workloads see the tree (design §5): absolute `gitdir:`
    /// targets under these map exactly.
    pub views: Vec<View>,
}

/// A write's freeze stamp: the stamp of its backing file, taken just
/// before its freeze (design §14.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathStamp {
    pub path: RepoPath,
    pub stamp: FreezeStamp,
}

/// A call from the service to a listening front (design §15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// Record now what changed through the front and is not recorded yet
    /// (its sweep: the repair queue, then every inode open for writing
    /// that changed), and answer once that is journaled.
    Settle,
    /// Whether each path has a handle open for writing through the front.
    OpenForWriting {
        paths: Vec<RepoPath>,
    },
    /// The service changed these paths in the backing tree: drop the
    /// kernel's cached entries, attributes and pages of each.
    Invalidate {
        paths: Vec<RepoPath>,
    },
    Ping,
}

/// A front's answer to a [`Call`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The sweep is done: `recorded` inodes recorded, `failed` paths still
    /// waiting for a repair (their records failed).
    Settled {
        recorded: usize,
        failed: usize,
    },
    Open {
        open: Vec<bool>,
    },
    Invalidated,
    Pong,
    Error {
        message: String,
    },
}

/// Where a history service listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    /// A Unix socket, on the node's own filesystem.
    Unix(PathBuf),
}

impl Address {
    /// The socket's directory and file name, for [`crate::local_socket`].
    pub fn unix_parts(&self) -> Option<(&Path, &str)> {
        match self {
            Self::Unix(path) => Some((path.parent()?, path.file_name()?.to_str()?)),
        }
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "unix:{}", path.display()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AddressError {
    #[error(
        "history service address {0:?} is not supported: a front reaches its history service only over the Unix socket on its own node (unix:<path>, design §14.1)"
    )]
    Unsupported(String),
    #[error("history service address {0:?} must be an absolute path or unix:<absolute path>")]
    NotAbsolute(String),
}

impl FromStr for Address {
    type Err = AddressError;

    /// `unix:<path>` or an absolute path. Other schemes (`tcp:`, …) are
    /// refused: a volume is used from one node at a time.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let path = match s.strip_prefix("unix:") {
            Some(path) => path,
            None if s.starts_with('/') => s,
            None if s.contains(':') => return Err(AddressError::Unsupported(s.to_owned())),
            None => return Err(AddressError::NotAbsolute(s.to_owned())),
        };
        let path = PathBuf::from(path);
        if !path.is_absolute() || path.file_name().is_none() {
            return Err(AddressError::NotAbsolute(s.to_owned()));
        }
        Ok(Self::Unix(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_parse() {
        let unix = Address::Unix("/s/meta/history/history.sock".into());
        assert_eq!(
            "unix:/s/meta/history/history.sock".parse(),
            Ok(unix.clone())
        );
        assert_eq!("/s/meta/history/history.sock".parse(), Ok(unix.clone()));
        assert_eq!(unix.to_string(), "unix:/s/meta/history/history.sock");
        assert_eq!(
            unix.unix_parts(),
            Some((Path::new("/s/meta/history"), "history.sock"))
        );
        for bad in ["tcp:host:7000", "grpc://h"] {
            let err = bad.parse::<Address>().unwrap_err();
            assert!(matches!(err, AddressError::Unsupported(_)), "{bad}");
            assert!(err.to_string().contains("not supported: a front reaches"));
        }
        for bad in ["relative.sock", "unix:relative", "unix:", "/"] {
            assert!(
                matches!(bad.parse::<Address>(), Err(AddressError::NotAbsolute(_))),
                "{bad}"
            );
        }
    }
}
