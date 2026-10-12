//! Frozen copies of recorded content, waiting to become git objects
//! (design §3.2 step 1).
//!
//! Freezing must not depend on file size where the filesystem allows it, so
//! it tries a reflink (`FICLONE`, supported by XFS and btrfs) before falling
//! back to a copy.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

const TMP_PREFIX: &str = "tmp-";

/// Identifies one frozen copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PendingId(pub u64);

impl PendingId {
    fn file_name(self) -> String {
        format!("{:016x}", self.0)
    }

    fn parse(name: &str) -> Option<Self> {
        (name.len() == 16)
            .then(|| u64::from_str_radix(name, 16).ok())
            .flatten()
            .map(Self)
    }
}

/// How a freeze was done, for telemetry and benchmarks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreezeMethod {
    Reflink,
    Copy,
}

pub struct PendingStore {
    dir: PathBuf,
    next: AtomicU64,
}

impl PendingStore {
    /// Opens (creating if needed) the store at `dir`, removing copies a crash
    /// left half-written.
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let mut max = None;
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(TMP_PREFIX) {
                fs::remove_file(entry.path())?;
            } else if let Some(id) = PendingId::parse(&name) {
                max = max.max(Some(id.0));
            }
        }
        Ok(Self {
            dir,
            next: AtomicU64::new(max.map_or(0, |m| m + 1)),
        })
    }

    pub fn path(&self, id: PendingId) -> PathBuf {
        self.dir.join(id.file_name())
    }

    /// Freezes the current content of the file at `src`.
    ///
    /// The copy's data is durable when this returns; its directory entry
    /// becomes durable at the next [`PendingStore::sync_dir`], which the
    /// journal calls before it makes the referencing record durable.
    pub fn freeze(&self, src: &Path) -> io::Result<(PendingId, FreezeMethod)> {
        let id = PendingId(self.next.fetch_add(1, Ordering::Relaxed));
        let tmp = self.dir.join(format!("{TMP_PREFIX}{}", id.file_name()));
        let method = match copy_into(src, &tmp) {
            Ok(method) => method,
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        };
        fs::rename(&tmp, self.path(id))?;
        Ok((id, method))
    }

    /// Makes new and removed entries durable.
    pub fn sync_dir(&self) -> io::Result<()> {
        File::open(&self.dir)?.sync_all()
    }

    pub fn remove(&self, ids: impl IntoIterator<Item = PendingId>) -> io::Result<()> {
        for id in ids {
            match fs::remove_file(self.path(id)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }

    /// Removes every copy not in `keep`: copies frozen for records that never
    /// became durable, or already committed before a crash.
    pub fn prune_except(&self, keep: &HashSet<PendingId>) -> io::Result<usize> {
        let mut removed = 0;
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if let Some(id) = PendingId::parse(&entry.file_name().to_string_lossy())
                && !keep.contains(&id)
            {
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

fn copy_into(src: &Path, dst: &Path) -> io::Result<FreezeMethod> {
    let from = File::open(src)?;
    let to = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dst)?;
    let method = match rustix::fs::ioctl_ficlone(&to, &from) {
        Ok(()) => FreezeMethod::Reflink,
        Err(_) => {
            io::copy(&mut &from, &mut &to)?;
            FreezeMethod::Copy
        }
    };
    to.sync_all()?;
    Ok(method)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freeze_copies_current_content_and_survives_later_writes() {
        let dir = tempfile::tempdir().unwrap();
        let store = PendingStore::open(dir.path().join("pending")).unwrap();
        let src = dir.path().join("f");
        fs::write(&src, b"one").unwrap();

        let (id, _) = store.freeze(&src).unwrap();
        fs::write(&src, b"two").unwrap();

        assert_eq!(fs::read(store.path(id)).unwrap(), b"one");
    }

    #[test]
    fn reopen_resumes_ids_and_clears_partial_copies() {
        let dir = tempfile::tempdir().unwrap();
        let pending = dir.path().join("pending");
        let src = dir.path().join("f");
        fs::write(&src, b"x").unwrap();
        let first = {
            let store = PendingStore::open(&pending).unwrap();
            store.freeze(&src).unwrap().0
        };
        fs::write(pending.join("tmp-0000000000000009"), b"partial").unwrap();

        let store = PendingStore::open(&pending).unwrap();
        let second = store.freeze(&src).unwrap().0;

        assert!(second > first);
        assert!(!pending.join("tmp-0000000000000009").exists());
    }

    #[test]
    fn prune_keeps_only_referenced_copies() {
        let dir = tempfile::tempdir().unwrap();
        let store = PendingStore::open(dir.path().join("pending")).unwrap();
        let src = dir.path().join("f");
        fs::write(&src, b"x").unwrap();
        let a = store.freeze(&src).unwrap().0;
        let b = store.freeze(&src).unwrap().0;

        let removed = store.prune_except(&HashSet::from([b])).unwrap();

        assert_eq!(removed, 1);
        assert!(!store.path(a).exists());
        assert!(store.path(b).exists());
    }

    #[test]
    fn freeze_of_missing_file_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = PendingStore::open(dir.path().join("pending")).unwrap();
        assert!(store.freeze(&dir.path().join("missing")).is_err());
        assert_eq!(fs::read_dir(dir.path().join("pending")).unwrap().count(), 0);
    }
}
