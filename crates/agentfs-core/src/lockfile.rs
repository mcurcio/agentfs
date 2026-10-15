//! Lock files that hold under `flock` and under NFS's emulation of it
//! (design §2.2, §2.3, §14.2).
//!
//! The state lock ([`crate::lock::StateLock`]) and the front slot leases
//! ([`crate::history_service::FrontLease`]) are exclusive locks on files
//! in the state area, which on several nodes lives on a shared filesystem.
//! An NFS client emulates `flock` with POSIX (`fcntl`) byte-range locks,
//! and POSIX locks belong to the *process*, not the open file:
//!
//! - two opens of one file in one process do not conflict, so a process
//!   could lease one slot twice;
//! - closing *any* descriptor of the file drops every lock the process
//!   holds on it, so a probe that opens and closes a lock file the
//!   process itself holds would silently release it.
//!
//! This module keeps a process-wide registry of the lock files the process
//! holds, keyed by device and inode, and serialises every lock-file
//! operation of the process on it: an exclusive lock is refused when the
//! process already holds that file, and a probe reports a held file from
//! the registry without opening it. Between processes the kernel (or the
//! NFS server) decides, as before.
//!
//! Every holder line starts with `pid=<pid> node=<node>`, so a holder is
//! identified across nodes, where pids collide; the node is
//! [`node_name`].
//!
//! Tests can switch the calling thread to POSIX locks
//! ([`use_posix_locks_on_this_thread`]) to exercise the emulated semantics
//! on a local filesystem.

use std::cell::Cell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::FlockOperation;

use crate::push::utc_basic;

/// The lock files this process holds, by (device, inode), with the holder
/// line each carries.
static HELD: Mutex<Option<HashMap<(u64, u64), String>>> = Mutex::new(None);

/// The node name set by the command line ([`set_node`]).
static NODE: OnceLock<String> = OnceLock::new();

thread_local! {
    static POSIX: Cell<bool> = const { Cell::new(false) };
}

fn held() -> MutexGuard<'static, Option<HashMap<(u64, u64), String>>> {
    HELD.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Makes this thread's lock operations POSIX (`fcntl`) locks instead of
/// `flock`, the semantics an NFS client gives `flock`. For tests.
pub fn use_posix_locks_on_this_thread(posix: bool) {
    POSIX.with(|p| p.set(posix));
}

/// Names this process's node in holder lines; the first call wins. Empty
/// names are ignored.
pub fn set_node(node: &str) {
    if !node.is_empty() {
        let _ = NODE.set(node.to_owned());
    }
}

/// This process's node: [`set_node`]'s, else `AGENTFS_NODE`, else the
/// host name.
pub fn node_name() -> String {
    if let Some(node) = NODE.get() {
        return node.clone();
    }
    std::env::var("AGENTFS_NODE")
        .ok()
        .filter(|n| !n.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .map(|h| h.trim().to_owned())
                .filter(|h| !h.is_empty())
        })
        .unwrap_or_else(|| "localhost".into())
}

/// `pid=<pid> node=<node> <description> since=<UTC>`.
pub fn holder_line(description: &str) -> String {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!(
        "pid={} node={} {description} since={}",
        std::process::id(),
        node_name(),
        utc_basic(since)
    )
}

/// The `node=` field of a holder line, if it has one (lines written
/// before nodes were recorded do not).
pub fn holder_node(line: &str) -> Option<&str> {
    line.split_whitespace()
        .find_map(|f| f.strip_prefix("node="))
}

/// The `pid=` field of a holder line.
pub fn holder_pid(line: &str) -> Option<u32> {
    line.split_whitespace()
        .find_map(|f| f.strip_prefix("pid="))
        .and_then(|p| p.parse().ok())
}

/// One lock operation: `true` if taken, `false` if another holder has a
/// conflicting lock.
fn lock_op(file: &File, op: FlockOperation) -> io::Result<bool> {
    let posix = POSIX.with(Cell::get);
    loop {
        let result = if posix {
            rustix::fs::fcntl_lock(file, op)
        } else {
            rustix::fs::flock(file, op)
        };
        match result {
            Ok(()) => return Ok(true),
            Err(rustix::io::Errno::INTR) => continue,
            Err(rustix::io::Errno::WOULDBLOCK) => return Ok(false),
            // POSIX allows EACCES for a conflicting lock.
            Err(rustix::io::Errno::ACCESS) if posix => return Ok(false),
            Err(e) => return Err(e.into()),
        }
    }
}

fn key(meta: &std::fs::Metadata) -> (u64, u64) {
    (meta.dev(), meta.ino())
}

/// An exclusive lock this process holds on a lock file. Dropping it
/// releases the lock.
#[derive(Debug)]
pub struct HeldLock {
    file: Option<File>,
    key: (u64, u64),
    path: PathBuf,
}

impl HeldLock {
    /// Takes an exclusive lock on `path` (created with `mode` if missing)
    /// without waiting; `None` if another process holds it, or this
    /// process already does.
    pub fn try_exclusive(path: &Path, mode: u32) -> io::Result<Option<Self>> {
        let mut held = held();
        let map = held.get_or_insert_with(HashMap::new);
        if let Ok(meta) = std::fs::metadata(path)
            && map.contains_key(&key(&meta))
        {
            return Ok(None);
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(mode)
            .open(path)?;
        let k = key(&file.metadata()?);
        if map.contains_key(&k) {
            // The path changed under the check to a file this process
            // holds: closing this descriptor would drop that lock under
            // POSIX semantics, so the descriptor is kept open.
            std::mem::forget(file);
            return Ok(None);
        }
        if !lock_op(&file, FlockOperation::NonBlockingLockExclusive)? {
            return Ok(None);
        }
        map.insert(k, String::new());
        Ok(Some(Self {
            file: Some(file),
            key: k,
            path: path.to_path_buf(),
        }))
    }

    /// Replaces the file's content with `line` (and a newline), the
    /// holder's description other processes read.
    pub fn set_holder(&mut self, line: &str) -> io::Result<()> {
        let mut held = held();
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("released"))?;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(format!("{line}\n").as_bytes())?;
        if let Some(map) = held.as_mut() {
            map.insert(self.key, line.to_owned());
        }
        Ok(())
    }

    /// The locked file, for writes that do not close it.
    pub fn file(&self) -> &File {
        // Only `None` inside `drop`.
        self.file.as_ref().expect("held until dropped")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        let mut held = held();
        // Closed under the registry's lock, so no other thread of this
        // process takes the file in between.
        drop(self.file.take());
        if let Some(map) = held.as_mut() {
            map.remove(&self.key);
        }
    }
}

/// Whether some process holds an exclusive lock on `path`: its holder
/// line if so (from the registry when it is this process), `None` if the
/// file is free or missing. Never opens a file this process holds.
pub fn probe(path: &Path) -> io::Result<Option<String>> {
    let mut held = held();
    let map = held.get_or_insert_with(HashMap::new);
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if let Some(line) = map.get(&key(&meta)) {
        return Ok(Some(line.clone()));
    }
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if map.contains_key(&key(&file.metadata()?)) {
        std::mem::forget(file);
        return Ok(Some(String::new()));
    }
    // A shared probe: it fails only against a holder's exclusive lock.
    if lock_op(&file, FlockOperation::NonBlockingLockShared)? {
        lock_op(&file, FlockOperation::Unlock)?;
        return Ok(None);
    }
    let mut text = String::new();
    let _ = (&mut file).take(512).read_to_string(&mut text);
    Ok(Some(text.trim().to_owned()))
}

/// Whether this process holds the lock file `path`.
pub fn held_here(path: &Path) -> bool {
    let held = held();
    match (held.as_ref(), std::fs::metadata(path)) {
        (Some(map), Ok(meta)) => map.contains_key(&key(&meta)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn both(test: impl Fn()) {
        for posix in [false, true] {
            use_posix_locks_on_this_thread(posix);
            test();
        }
        use_posix_locks_on_this_thread(false);
    }

    #[test]
    fn one_holder_per_file_within_a_process() {
        both(|| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("l");
            let mut a = HeldLock::try_exclusive(&path, 0o600).unwrap().unwrap();
            a.set_holder(&holder_line("command=a")).unwrap();
            assert!(HeldLock::try_exclusive(&path, 0o600).unwrap().is_none());
            let line = probe(&path).unwrap().unwrap();
            assert!(line.contains("command=a"), "{line}");
            assert!(held_here(&path));
            // The probe did not drop the lock (under POSIX, a close would).
            assert!(HeldLock::try_exclusive(&path, 0o600).unwrap().is_none());
            drop(a);
            assert!(!held_here(&path));
            assert_eq!(probe(&path).unwrap(), None);
            assert!(HeldLock::try_exclusive(&path, 0o600).unwrap().is_some());
        });
    }

    #[test]
    fn missing_files_are_free() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(probe(&dir.path().join("none")).unwrap(), None);
    }

    #[test]
    fn holder_lines_carry_pid_and_node() {
        let line = holder_line("command=x");
        assert_eq!(holder_pid(&line), Some(std::process::id()));
        assert_eq!(holder_node(&line), Some(node_name().as_str()));
        assert!(line.contains(" command=x since="), "{line}");
        assert_eq!(holder_node("pid=3 command=mount since=x"), None);
    }
}
