//! Where the active history service of a volume listens (design §14.1,
//! §14.2): `meta/history/endpoint.json` in the shared state area.
//!
//! The service holding the state lock writes it atomically once it
//! listens, and removes it at a clean stop. A front given the address
//! `state` reads it at each new connection: it uses the Unix socket when
//! the service is on its own node (the same `--node`, and the socket
//! exists), else the TLS address. A service that crashed leaves its file
//! behind; the next active service replaces it, and the `instance` tells
//! one service's file from the next.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::Address;
use crate::volume::StateDir;

/// The file's format version.
pub const ENDPOINT_VERSION: u32 = 1;

/// The file's name in `meta/history/`.
pub const ENDPOINT_FILE: &str = "endpoint.json";

/// What the active service wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub v: u32,
    pub volume: String,
    /// The node it runs on ([`crate::lockfile::node_name`]).
    pub node: String,
    /// Its Unix socket, reachable on `node` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unix: Option<PathBuf>,
    /// Its TLS address, reachable from other nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<SocketAddr>,
    /// Distinct for every service process.
    pub instance: u64,
    /// The service's pid, on `node`.
    pub pid: u32,
    /// When it started serving (UTC, `YYYYMMDDTHHMMSSZ`).
    pub since: String,
}

/// The endpoint file of `state`.
pub fn path(state: &StateDir) -> PathBuf {
    state.history_dir().join(ENDPOINT_FILE)
}

impl Endpoint {
    /// An endpoint of this process for `volume`.
    pub fn new(volume: &str, unix: Option<PathBuf>, tls: Option<SocketAddr>) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            v: ENDPOINT_VERSION,
            volume: volume.to_owned(),
            node: crate::lockfile::node_name(),
            unix,
            tls,
            instance: (now.as_nanos() as u64) ^ (u64::from(std::process::id()) << 40),
            pid: std::process::id(),
            since: crate::push::utc_basic(now.as_secs()),
        }
    }

    /// Writes it atomically (temporary file, fsync, rename, fsync of the
    /// directory), mode 0600, in a 0700 `meta/history/`.
    pub fn write(&self, state: &StateDir) -> io::Result<()> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let dir = state.history_dir();
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        crate::fsutil::write_json_atomic(&path(state), self)
    }

    /// The endpoint file, if there is one. A malformed file is an error.
    pub fn read(state: &StateDir) -> io::Result<Option<Self>> {
        match fs::read(path(state)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Removes the endpoint file if it is still this one (a successor's is
    /// left alone).
    pub fn remove(&self, state: &StateDir) -> io::Result<()> {
        match Self::read(state) {
            Ok(Some(current)) if current.instance == self.instance => {
                match fs::remove_file(path(state)) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                    _ => Ok(()),
                }
            }
            _ => Ok(()),
        }
    }

    /// The address a front on `my_node` uses: the Unix socket on the
    /// service's own node when it exists there, else the TLS address.
    pub fn choose(&self, my_node: &str) -> Option<Address> {
        let local = self
            .unix
            .as_ref()
            .filter(|_| self.node == my_node)
            .filter(|p| Path::new(p).exists());
        if let Some(unix) = local {
            return Some(Address::Unix(unix.clone()));
        }
        #[cfg(feature = "history-tls")]
        if let Some(tls) = self.tls {
            return Some(Address::Tls(tls));
        }
        // Without TLS, the socket is the only way, if it is there.
        self.unix
            .as_ref()
            .filter(|p| Path::new(p).exists())
            .map(|p| Address::Unix(p.clone()))
    }

    /// One line for `agentfs status`.
    pub fn describe(&self) -> String {
        let mut ways = Vec::new();
        if let Some(unix) = &self.unix {
            ways.push(format!("unix:{}", unix.display()));
        }
        if let Some(tls) = self.tls {
            ways.push(format!("tls:{tls}"));
        }
        format!(
            "node {} pid {} since {} at {}",
            self.node,
            self.pid,
            self.since,
            if ways.is_empty() {
                "nothing".to_owned()
            } else {
                ways.join(", ")
            }
        )
    }
}

/// Reads the endpoint file only when it changed: a front resolves `state`
/// at every new connection.
#[derive(Debug, Default)]
pub struct EndpointCache {
    last: Mutex<Option<(Stamp, Endpoint)>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    ino: u64,
    size: u64,
    mtime_ns: i64,
}

impl EndpointCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The endpoint file of `state` now.
    pub fn get(&self, state: &StateDir) -> io::Result<Option<Endpoint>> {
        let meta = match fs::metadata(path(state)) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                *self.lock() = None;
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let stamp = Stamp {
            ino: meta.ino(),
            size: meta.size(),
            mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
        };
        if let Some((s, endpoint)) = self.lock().as_ref()
            && *s == stamp
        {
            return Ok(Some(endpoint.clone()));
        }
        let endpoint = Endpoint::read(state)?;
        *self.lock() = endpoint.clone().map(|e| (stamp, e));
        Ok(endpoint)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<(Stamp, Endpoint)>> {
        self.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (tempfile::TempDir, StateDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path().join("state"));
        (dir, state)
    }

    #[test]
    fn written_read_and_removed_only_by_its_writer() {
        let (_dir, state) = state();
        assert_eq!(Endpoint::read(&state).unwrap(), None);
        let first = Endpoint::new("vol", Some("/s.sock".into()), None);
        first.write(&state).unwrap();
        assert_eq!(Endpoint::read(&state).unwrap().as_ref(), Some(&first));
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path(&state)), 0o600);
        assert_eq!(mode(&state.history_dir()), 0o700);
        let mut second = Endpoint::new("vol", Some("/t.sock".into()), None);
        second.instance = first.instance + 1;
        second.write(&state).unwrap();
        first.remove(&state).unwrap();
        assert_eq!(Endpoint::read(&state).unwrap(), Some(second.clone()));
        second.remove(&state).unwrap();
        assert_eq!(Endpoint::read(&state).unwrap(), None);
        second.remove(&state).unwrap();
    }

    #[test]
    fn a_front_on_the_services_node_uses_its_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("h.sock");
        let mut e = Endpoint::new("vol", Some(sock.clone()), None);
        e.node = "node-a".into();
        // No socket yet.
        assert_eq!(e.choose("node-a"), None);
        fs::write(&sock, "").unwrap();
        assert_eq!(e.choose("node-a"), Some(Address::Unix(sock.clone())));
        // Without TLS, another node can only try the socket.
        assert_eq!(e.choose("node-b"), Some(Address::Unix(sock)));
        assert!(e.describe().contains("node node-a"), "{}", e.describe());
    }

    #[cfg(feature = "history-tls")]
    #[test]
    fn a_front_on_another_node_uses_tls() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("h.sock");
        fs::write(&sock, "").unwrap();
        let tls: SocketAddr = "10.0.0.7:4100".parse().unwrap();
        let mut e = Endpoint::new("vol", Some(sock.clone()), Some(tls));
        e.node = "node-a".into();
        assert_eq!(e.choose("node-a"), Some(Address::Unix(sock)));
        assert_eq!(e.choose("node-b"), Some(Address::Tls(tls)));
        assert!(e.describe().contains("tls:10.0.0.7:4100"));
    }

    #[test]
    fn the_cache_rereads_a_changed_file() {
        let (_dir, state) = state();
        let cache = EndpointCache::new();
        assert_eq!(cache.get(&state).unwrap(), None);
        let first = Endpoint::new("vol", Some("/a".into()), None);
        first.write(&state).unwrap();
        assert_eq!(cache.get(&state).unwrap(), Some(first.clone()));
        assert_eq!(cache.get(&state).unwrap(), Some(first));
        // A rename gives a new inode, whatever the clock.
        let mut second = Endpoint::new("vol", Some("/b".into()), None);
        second.instance += 1;
        second.write(&state).unwrap();
        assert_eq!(cache.get(&state).unwrap(), Some(second));
        fs::remove_file(path(&state)).unwrap();
        assert_eq!(cache.get(&state).unwrap(), None);
    }
}
