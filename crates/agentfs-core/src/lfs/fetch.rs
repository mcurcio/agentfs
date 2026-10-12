//! LFS objects for restores and reverts (design §7, §15): from the local
//! cache, else downloaded from the remotes, in name order, into it.
//!
//! A restore makes sure every LFS object it writes is available before it
//! writes anything, so a missing object refuses the restore instead of
//! leaving it half done.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use super::{Direction, LfsError, LfsOid, ObjectCache, Pointer, Transfer, transfer_for};
use crate::config::Settings;
use crate::volume::StateDir;

/// Where a restore gets LFS content.
pub trait LfsObjects {
    /// The path of the cached object, downloading it first if needed.
    fn ensure(&self, pointer: &Pointer) -> Result<PathBuf, LfsError>;
}

/// The cache alone: nothing is downloaded.
pub struct CacheOnly(pub ObjectCache);

impl LfsObjects for CacheOnly {
    fn ensure(&self, pointer: &Pointer) -> Result<PathBuf, LfsError> {
        if self.0.contains(pointer) {
            Ok(self.0.path(&pointer.oid))
        } else {
            Err(LfsError::local(format!(
                "LFS object {} is not in the local cache",
                pointer.oid
            )))
        }
    }
}

/// For a dry run: the cache alone, and nothing downloaded or written. An
/// object missing from the cache does not refuse the plan; it is noted
/// ([`CacheProbe::uncached`]) for the operator, since a real restore
/// downloads it then. Whether a remote has it is not checked.
pub struct CacheProbe {
    cache: ObjectCache,
    uncached: RefCell<BTreeMap<LfsOid, u64>>,
}

impl CacheProbe {
    pub fn new(cache: ObjectCache) -> Self {
        Self {
            cache,
            uncached: RefCell::new(BTreeMap::new()),
        }
    }

    /// The distinct objects not in the cache, and their total size.
    pub fn uncached(&self) -> (usize, u64) {
        let uncached = self.uncached.borrow();
        (uncached.len(), uncached.values().sum())
    }
}

impl LfsObjects for CacheProbe {
    fn ensure(&self, pointer: &Pointer) -> Result<PathBuf, LfsError> {
        if !self.cache.contains(pointer) {
            self.uncached.borrow_mut().insert(pointer.oid, pointer.size);
        }
        Ok(self.cache.path(&pointer.oid))
    }
}

/// The cache, then each remote's download endpoint in name order.
pub struct Fetcher {
    cache: ObjectCache,
    settings: Settings,
    transfers: RefCell<BTreeMap<String, Box<dyn Transfer>>>,
}

impl Fetcher {
    /// A fetcher for the state area's cache and the volume's remotes.
    pub fn new(state: &StateDir, settings: &Settings) -> io::Result<Self> {
        Ok(Self {
            cache: ObjectCache::at(state.lfs())?,
            settings: settings.clone(),
            transfers: RefCell::new(BTreeMap::new()),
        })
    }

    pub fn cache(&self) -> &ObjectCache {
        &self.cache
    }
}

impl LfsObjects for Fetcher {
    fn ensure(&self, pointer: &Pointer) -> Result<PathBuf, LfsError> {
        if self.cache.contains(pointer) {
            return Ok(self.cache.path(&pointer.oid));
        }
        let mut problems = Vec::new();
        for remote in &self.settings.remotes {
            let mut transfers = self.transfers.borrow_mut();
            if !transfers.contains_key(&remote.name) {
                match transfer_for(&self.settings, remote, Direction::Download) {
                    Ok(t) => {
                        transfers.insert(remote.name.clone(), t);
                    }
                    Err(e) => {
                        problems.push(format!("{}: {e}", remote.name));
                        continue;
                    }
                }
            }
            let transfer = &transfers[&remote.name];
            match transfer.download(pointer, &self.cache, &self.settings.branch_ref) {
                Ok(()) => return Ok(self.cache.path(&pointer.oid)),
                Err(e) => problems.push(format!("{}: {e}", remote.name)),
            }
        }
        let tried = if problems.is_empty() {
            "no remote is configured to download it from".to_owned()
        } else {
            format!("no remote could provide it ({})", problems.join("; "))
        };
        Err(LfsError::local(format!(
            "LFS object {} ({} bytes) is not in the local cache, and {tried}",
            pointer.oid, pointer.size
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn the_cache_first_then_the_remotes_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path().join("state"));
        crate::volume::init(&state, "v", None).unwrap();
        // Two local remotes: `a` lacks the object, `b` has it.
        let a = dir.path().join("a.git");
        let b = dir.path().join("b.git");
        git2::Repository::init_bare(&a).unwrap();
        git2::Repository::init_bare(&b).unwrap();
        let content = b"restored from b";
        let pointer = Pointer::of_reader(&content[..]).unwrap();
        let hex = pointer.oid.to_string();
        let object = b.join(format!("lfs/objects/{}/{}/{hex}", &hex[..2], &hex[2..4]));
        fs::create_dir_all(object.parent().unwrap()).unwrap();
        fs::write(&object, content).unwrap();
        {
            let mut config = git2::Config::open(&state.git().join("config")).unwrap();
            config.set_str("remote.a.url", a.to_str().unwrap()).unwrap();
            config.set_str("remote.b.url", b.to_str().unwrap()).unwrap();
        }
        let settings = Settings::load(&state.git()).unwrap();
        let fetcher = Fetcher::new(&state, &settings).unwrap();
        let path = fetcher.ensure(&pointer).unwrap();
        assert_eq!(fs::read(&path).unwrap(), content);
        assert!(fetcher.cache().contains(&pointer));
        // Cached now: no remote needed.
        fs::remove_file(&object).unwrap();
        assert_eq!(fetcher.ensure(&pointer).unwrap(), path);
        // Nowhere at all.
        let absent = Pointer::of_reader(&b"absent"[..]).unwrap();
        let err = fetcher.ensure(&absent).unwrap_err();
        assert!(err.message.contains("a: "), "{err}");
        assert!(err.message.contains("b: "), "{err}");
        let only = CacheOnly(ObjectCache::at(state.lfs()).unwrap());
        assert!(only.ensure(&pointer).is_ok());
        assert!(only.ensure(&absent).is_err());
        // A dry run's probe downloads nothing and notes what is missing.
        let probe = CacheProbe::new(ObjectCache::at(state.lfs()).unwrap());
        assert!(probe.ensure(&pointer).is_ok());
        assert_eq!(probe.uncached(), (0, 0));
        let path = probe.ensure(&absent).unwrap();
        probe.ensure(&absent).unwrap();
        assert!(!path.exists());
        assert_eq!(probe.uncached(), (1, 6));
    }
}
