//! One history service per volume, fed by a front per pod (design §14).
//!
//! In single-process mode (the default) every mount is its own writer of
//! the branch and holds the state-area lock. With a history service, one
//! process (`agentfs serve-history`) holds the lock and is the only writer
//! of the branch and journal; each pod's mount is a *front* that freezes
//! into the shared `pending/` in a slot it leases ([`FrontLease`]) and sends
//! the record to the service ([`protocol`]), which journals it and replies
//! once it is durable. The front fails the boundary with `EIO` if no
//! durable reply comes (design §3.5). Each front also keeps a listening
//! connection ([`HistoryClient::listen`]) on which the service calls it,
//! so the service can serve restores live ([`Fronts`], design §15).
//!
//! Fronts reach the service over a Unix socket on its node, or over TLS
//! from another node ([`transport`], feature `history-tls`); the address
//! `state` finds the active service through its endpoint file
//! ([`endpoint`], design §14.2).

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

mod client;
pub mod endpoint;
mod fronts;
mod lease;
mod listener;
pub mod protocol;
mod server;
#[cfg(feature = "history-tls")]
pub mod tls;
pub mod transport;

pub use client::{ClientConfig, ClientError, HistoryClient};
pub use endpoint::{Endpoint, EndpointCache};
pub use fronts::{Fronts, FrontsError, MIN_CALL_TIMEOUT};
pub use lease::{FrontLease, LeaseError, MAX_SLOT};
pub use listener::{FrontHooks, ListenerHandle, NoHooks, RECONNECT_MAX, RECONNECT_MIN};
pub use server::{HELLO_TIMEOUT, HistoryServer, IDLE_TIMEOUT, MAX_CONNECTIONS};

/// Whether this build carries the TLS transport (feature `history-tls`).
pub const TLS_BUILT: bool = cfg!(feature = "history-tls");

/// Where a history service listens, or how a front finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    /// A Unix socket, on the node's own filesystem.
    Unix(PathBuf),
    /// TLS over TCP, authenticated both ways with the volume's certificate
    /// (design §14.1). Needs the `history-tls` feature to serve or reach.
    Tls(SocketAddr),
    /// Whatever the active service of the volume says in its endpoint
    /// file ([`endpoint`]), read again at every new connection: its Unix
    /// socket on its own node, its TLS address from any other. A front
    /// only.
    State,
}

impl Address {
    /// The socket's directory and file name, for [`crate::local_socket`].
    pub fn unix_parts(&self) -> Option<(&Path, &str)> {
        match self {
            Self::Unix(path) => Some((path.parent()?, path.file_name()?.to_str()?)),
            Self::Tls(_) | Self::State => None,
        }
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "unix:{}", path.display()),
            Self::Tls(addr) => write!(f, "tls:{addr}"),
            Self::State => f.write_str("state"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AddressError {
    #[error(
        "history service address {0:?} is not supported (design §14.1): use unix:<path>, \
         tls:<ip>:<port> or state"
    )]
    Unsupported(String),
    #[error("history service address {0:?} must be an absolute path or unix:<absolute path>")]
    NotAbsolute(String),
    #[error(
        "history service address {0:?}: tls: takes an IP address and port, as tls:10.0.0.7:4100"
    )]
    BadTls(String),
}

impl FromStr for Address {
    type Err = AddressError;

    /// `unix:<path>` or an absolute path, `tls:<ip>:<port>`, or `state`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "state" {
            return Ok(Self::State);
        }
        if let Some(addr) = s.strip_prefix("tls:") {
            return addr
                .parse()
                .map(Self::Tls)
                .map_err(|_| AddressError::BadTls(s.to_owned()));
        }
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
            assert!(err.to_string().contains("not supported (design §14.1)"));
        }
        let tls: Address = "tls:10.1.2.3:4100".parse().unwrap();
        assert_eq!(tls, Address::Tls("10.1.2.3:4100".parse().unwrap()));
        assert_eq!(tls.to_string(), "tls:10.1.2.3:4100");
        assert_eq!(tls.unix_parts(), None);
        assert_eq!(
            "tls:[::1]:7".parse::<Address>().unwrap().to_string(),
            "tls:[::1]:7"
        );
        for bad in ["tls:host:7", "tls:1.2.3.4", "tls:"] {
            assert!(
                matches!(bad.parse::<Address>(), Err(AddressError::BadTls(_))),
                "{bad}"
            );
        }
        assert_eq!("state".parse::<Address>().unwrap(), Address::State);
        assert_eq!(Address::State.to_string(), "state");
        for bad in ["relative.sock", "unix:relative", "unix:", "/"] {
            assert!(
                matches!(bad.parse::<Address>(), Err(AddressError::NotAbsolute(_))),
                "{bad}"
            );
        }
    }
}
