//! Frozen copies of recorded content, waiting to become git objects
//! (design §3.2 step 1).
//!
//! Freezing must not depend on file size where the filesystem allows it, so
//! it tries a reflink (`FICLONE`, supported by XFS and btrfs) before falling
//! back to a copy.
//!
//! **Slots** (design §2.2, §14). Several processes freeze into one
//! `pending/` when fronts feed a history service, so ids carry the
//! freezing process's slot in their high 16 bits ([`SLOT_SHIFT`]) and each
//! process counts only within its own. Slot 0 is single-process mode and
//! the history service; a front leases a slot of its own
//! ([`crate::history_service::FrontLease`]) for as long as it runs, so ids
//! never collide and the journal format is the same.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

const TMP_PREFIX: &str = "tmp-";

/// Where a pending id's slot starts: its high 16 bits.
pub const SLOT_SHIFT: u32 = 48;

/// The counter part of an id: its low 48 bits.
const COUNTER_MASK: u64 = (1 << SLOT_SHIFT) - 1;

/// Identifies one frozen copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PendingId(pub u64);

impl PendingId {
    /// The `n`th id of `slot`.
    pub fn in_slot(slot: u16, n: u64) -> Self {
        Self((u64::from(slot) << SLOT_SHIFT) | (n & COUNTER_MASK))
    }

    /// The slot of the process that froze it.
    pub fn slot(self) -> u16 {
        (self.0 >> SLOT_SHIFT) as u16
    }

    fn counter(self) -> u64 {
        self.0 & COUNTER_MASK
    }

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
    slot: u16,
    next: AtomicU64,
}

impl PendingStore {
    /// Opens (creating if needed) the store at `dir` for slot 0, removing
    /// copies a crash left half-written in every slot. Only for a process
    /// no front runs beside; otherwise see [`PendingStore::open_slot`].
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        Self::open_slot(dir, 0, |_| true)
    }

    /// Opens (creating if needed) the store at `dir`, freezing new copies
    /// in `slot` with ids above the highest that slot already holds.
    /// Half-written copies (`tmp-*`) are removed only in slots
    /// `tmp_is_ours` accepts: a live front's copy being written must
    /// survive another process opening the store.
    pub fn open_slot(
        dir: impl Into<PathBuf>,
        slot: u16,
        tmp_is_ours: impl Fn(u16) -> bool,
    ) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let mut max = None;
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(tmp) = name.strip_prefix(TMP_PREFIX) {
                // An unparseable temporary name is no process's own.
                let owner = PendingId::parse(tmp).map(PendingId::slot);
                if owner.is_none_or(&tmp_is_ours) {
                    match fs::remove_file(entry.path()) {
                        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                        _ => {}
                    }
                }
            } else if let Some(id) = PendingId::parse(&name)
                && id.slot() == slot
            {
                max = max.max(Some(id.counter()));
            }
        }
        Ok(Self {
            dir,
            slot,
            next: AtomicU64::new(max.map_or(0, |m| m + 1)),
        })
    }

    /// The slot this store freezes into.
    pub fn slot(&self) -> u16 {
        self.slot
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
        let id = PendingId::in_slot(self.slot, self.next.fetch_add(1, Ordering::Relaxed));
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

    /// Removes every copy not in `keep` and not frozen in one of
    /// `spare_slots`: copies frozen for records that never became durable,
    /// or already committed before a crash. A live front's copies are
    /// spared, since its record may still be on its way (design §14).
    pub fn prune_except(
        &self,
        keep: &HashSet<PendingId>,
        spare_slots: &HashSet<u16>,
    ) -> io::Result<usize> {
        let mut removed = 0;
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if let Some(id) = PendingId::parse(&entry.file_name().to_string_lossy())
                && !keep.contains(&id)
                && !spare_slots.contains(&id.slot())
            {
                match fs::remove_file(entry.path()) {
                    Ok(()) => removed += 1,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(removed)
    }

    /// Removes the copies in `slot` made before `before` and not in
    /// `keep`: a dead front's orphans, once another front leased the slot
    /// at `before` (design §14). The new holder's own copies are newer.
    pub fn reclaim_slot(
        &self,
        slot: u16,
        before: SystemTime,
        keep: &HashSet<PendingId>,
    ) -> io::Result<usize> {
        let mut removed = 0;
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let Some(id) = PendingId::parse(&entry.file_name().to_string_lossy()) else {
                continue;
            };
            if id.slot() != slot || keep.contains(&id) {
                continue;
            }
            let made = match entry.metadata().and_then(|m| m.modified()) {
                Ok(made) => made,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            if made >= before {
                continue;
            }
            match fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(removed)
    }
}

/// Creates `dst` (which must not exist, mode 0600) as a reflink of `src` or,
/// where the filesystem cannot, a copy; its data is durable on return.
pub(crate) fn copy_into(src: &Path, dst: &Path) -> io::Result<FreezeMethod> {
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

        let removed = store
            .prune_except(&HashSet::from([b]), &HashSet::new())
            .unwrap();

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

    #[test]
    fn ids_carry_their_slot() {
        let id = PendingId::in_slot(7, 42);
        assert_eq!(id.slot(), 7);
        assert_eq!(id.counter(), 42);
        assert_eq!(PendingId(5).slot(), 0, "single-process ids are slot 0");
    }

    #[test]
    fn slots_count_independently_and_never_collide() {
        let dir = tempfile::tempdir().unwrap();
        let pending = dir.path().join("pending");
        let src = dir.path().join("f");
        fs::write(&src, b"x").unwrap();
        let service = PendingStore::open_slot(&pending, 0, |s| s == 0).unwrap();
        let front = PendingStore::open_slot(&pending, 3, |s| s == 3).unwrap();
        let a = service.freeze(&src).unwrap().0;
        let b = front.freeze(&src).unwrap().0;
        let c = front.freeze(&src).unwrap().0;

        assert_eq!((a.slot(), b.slot(), c.slot()), (0, 3, 3));
        assert_ne!(a, b);
        let reopened = PendingStore::open_slot(&pending, 3, |s| s == 3).unwrap();
        let d = reopened.freeze(&src).unwrap().0;
        assert_eq!(d, PendingId::in_slot(3, 2));
    }

    #[test]
    fn opening_spares_another_live_slots_partial_copies() {
        let dir = tempfile::tempdir().unwrap();
        let pending = dir.path().join("pending");
        fs::create_dir_all(&pending).unwrap();
        let theirs = pending.join(format!("tmp-{}", PendingId::in_slot(4, 1).file_name()));
        let ours = pending.join(format!("tmp-{}", PendingId::in_slot(0, 1).file_name()));
        let junk = pending.join("tmp-junk");
        for f in [&theirs, &ours, &junk] {
            fs::write(f, b"partial").unwrap();
        }

        PendingStore::open_slot(&pending, 0, |s| s != 4).unwrap();

        assert!(theirs.exists());
        assert!(!ours.exists());
        assert!(!junk.exists());
    }

    #[test]
    fn prune_spares_live_slots() {
        let dir = tempfile::tempdir().unwrap();
        let pending = dir.path().join("pending");
        let src = dir.path().join("f");
        fs::write(&src, b"x").unwrap();
        let own = PendingStore::open_slot(&pending, 0, |_| false).unwrap();
        let live = PendingStore::open_slot(&pending, 2, |_| false).unwrap();
        let dead = PendingStore::open_slot(&pending, 5, |_| false).unwrap();
        let a = own.freeze(&src).unwrap().0;
        let b = live.freeze(&src).unwrap().0;
        let c = dead.freeze(&src).unwrap().0;

        let removed = own
            .prune_except(&HashSet::new(), &HashSet::from([2]))
            .unwrap();

        assert_eq!(removed, 2);
        assert!(!own.path(a).exists());
        assert!(own.path(b).exists());
        assert!(!own.path(c).exists());
    }
}
