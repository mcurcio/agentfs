//! One history service per volume, fed by a front per pod (design §14).
//!
//! In single-process mode (the default) every mount is its own writer of
//! the branch and holds the state-area lock. With a history service, one
//! process (`agentfs serve-history`) holds the lock and is the only writer
//! of the branch and journal; each pod's mount is a *front* that freezes
//! into the shared `pending/` in a slot it leases ([`FrontLease`]) and sends
//! the record to the service ([`protocol`]), which journals it and replies
//! once it is durable. The front fails the boundary with `EIO` if no
//! durable reply comes (design §3.5).

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

mod client;
mod lease;
pub mod protocol;
mod server;

pub use client::{ClientConfig, ClientError, HistoryClient};
pub use lease::{FrontLease, LeaseError, MAX_SLOT};
pub use server::{HELLO_TIMEOUT, HistoryServer, IDLE_TIMEOUT, MAX_CONNECTIONS};

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
    #[error("history service address {0:?} is not supported yet (design §14): use unix:<path>")]
    Unsupported(String),
    #[error("history service address {0:?} must be an absolute path or unix:<absolute path>")]
    NotAbsolute(String),
}

impl FromStr for Address {
    type Err = AddressError;

    /// `unix:<path>` or an absolute path. Other schemes (`tcp:`, …) are
    /// for multi-node transport, which is not built yet.
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
            assert!(err.to_string().contains("not supported yet (design §14)"));
        }
        for bad in ["relative.sock", "unix:relative", "unix:", "/"] {
            assert!(
                matches!(bad.parse::<Address>(), Err(AddressError::NotAbsolute(_))),
                "{bad}"
            );
        }
    }
}
