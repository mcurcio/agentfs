//! LFS objects for restores, reverts and `agentfs lfs fetch` (design §7,
//! §15): from the local cache, else downloaded from the remotes, in name
//! order, into it. Every user holds the cache lock shared
//! ([`super::lock::CacheLock`]) while it uses the cache, so `agentfs lfs
//! prune` never deletes an object between a restore's check and its
//! write.
//!
//! A restore makes sure every LFS object it writes is available before it
//! writes anything, so a missing object refuses the restore instead of
//! leaving it half done. It first hands every object it will need to
//! [`LfsObjects::prefetch`], which downloads those missing from the cache
//! concurrently (`lfs.concurrenttransfers`, design §7), each remote in
//! name order taking what the ones before it could not provide; then it
//! asks for each object with [`LfsObjects::ensure`], whose errors name the
//! path.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use super::lock::CacheLock;
use super::{Direction, LfsError, LfsOid, ObjectCache, Pointer, Transfer, transfer_for};
use crate::config::{RemoteSettings, Settings};
use crate::volume::StateDir;

/// Where a restore gets LFS content.
pub trait LfsObjects {
    /// The path of the cached object, downloading it first if needed.
    fn ensure(&self, pointer: &Pointer) -> Result<PathBuf, LfsError>;

    /// A hint that each of `pointers` will be asked for next: a source
    /// that downloads may fetch them now, concurrently. Errors are not
    /// reported here; [`LfsObjects::ensure`] reports them per object.
    fn prefetch(&self, pointers: &[Pointer]) {
        let _ = pointers;
    }
}

/// The cache alone: nothing is downloaded.
pub struct CacheOnly {
    cache: ObjectCache,
    _lock: CacheLock,
}

impl CacheOnly {
    /// The cache, with its lock held shared for as long as this lives.
    pub fn new(cache: ObjectCache) -> io::Result<Self> {
        let lock = CacheLock::shared(cache.dir())?;
        Ok(Self { cache, _lock: lock })
    }

    pub fn cache(&self) -> &ObjectCache {
        &self.cache
    }
}

impl LfsObjects for CacheOnly {
    fn ensure(&self, pointer: &Pointer) -> Result<PathBuf, LfsError> {
        if self.cache.contains(pointer) {
            Ok(self.cache.path(&pointer.oid))
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

/// A remote's download transfer, or why it has none.
type Resolved = Result<Box<dyn Transfer>, LfsError>;

/// The cache, then each remote's download endpoint in name order.
pub struct Fetcher {
    cache: ObjectCache,
    settings: Settings,
    /// Each remote's download transfer, or why it has none, resolved once.
    transfers: RefCell<BTreeMap<String, Resolved>>,
    /// What every remote said about objects a prefetch could not get, so
    /// `ensure` reports it without asking again.
    failed: RefCell<BTreeMap<LfsOid, Vec<String>>>,
    _lock: CacheLock,
}

impl Fetcher {
    /// A fetcher for the state area's cache and the volume's remotes. It
    /// holds the cache lock shared for as long as it lives, waiting while
    /// a prune holds it.
    pub fn new(state: &StateDir, settings: &Settings) -> io::Result<Self> {
        let cache = ObjectCache::at(state.lfs())?;
        let lock = CacheLock::shared(cache.dir())?;
        Ok(Self {
            cache,
            settings: settings.clone(),
            transfers: RefCell::new(BTreeMap::new()),
            failed: RefCell::new(BTreeMap::new()),
            _lock: lock,
        })
    }

    pub fn cache(&self) -> &ObjectCache {
        &self.cache
    }

    /// Runs `f` with `remote`'s download transfer, resolving it the first
    /// time.
    fn with_transfer<R>(
        &self,
        remote: &RemoteSettings,
        f: impl FnOnce(&dyn Transfer) -> R,
    ) -> Result<R, LfsError> {
        let mut transfers = self.transfers.borrow_mut();
        let transfer = transfers
            .entry(remote.name.clone())
            .or_insert_with(|| transfer_for(&self.settings, remote, Direction::Download));
        match transfer {
            Ok(t) => Ok(f(t.as_ref())),
            Err(e) => Err(e.clone()),
        }
    }

    fn not_found(pointer: &Pointer, problems: &[String]) -> LfsError {
        let tried = if problems.is_empty() {
            "no remote is configured to download it from".to_owned()
        } else {
            format!("no remote could provide it ({})", problems.join("; "))
        };
        LfsError::local(format!(
            "LFS object {} ({} bytes) is not in the local cache, and {tried}",
            pointer.oid, pointer.size
        ))
    }
}

impl LfsObjects for Fetcher {
    fn prefetch(&self, pointers: &[Pointer]) {
        let remotes: Vec<&RemoteSettings> = self.settings.remotes.iter().collect();
        let report = download(&self.cache, &remotes, pointers, |remote, left| {
            self.with_transfer(remote, |t| {
                t.download_many(left, &self.cache, &self.settings.branch_ref)
            })
        });
        let mut failed = self.failed.borrow_mut();
        for (pointer, problems) in report.failures {
            failed.insert(pointer.oid, problems);
        }
    }

    fn ensure(&self, pointer: &Pointer) -> Result<PathBuf, LfsError> {
        if self.cache.contains(pointer) {
            return Ok(self.cache.path(&pointer.oid));
        }
        if let Some(problems) = self.failed.borrow().get(&pointer.oid) {
            return Err(Self::not_found(pointer, problems));
        }
        let mut problems = Vec::new();
        for remote in &self.settings.remotes {
            let result = self.with_transfer(remote, |t| {
                t.download(pointer, &self.cache, &self.settings.branch_ref)
            });
            match result.and_then(|r| r) {
                Ok(()) => return Ok(self.cache.path(&pointer.oid)),
                Err(e) => problems.push(format!("{}: {e}", remote.name)),
            }
        }
        Err(Self::not_found(pointer, &problems))
    }
}

/// What one remote did for a fetch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteFetch {
    pub remote: String,
    /// Objects downloaded from it, and their bytes.
    pub fetched: usize,
    pub bytes: u64,
    /// Objects asked of it that it could not provide.
    pub failed: usize,
    /// Why it could not be asked at all, if it could not (no endpoint).
    pub unavailable: Option<String>,
}

/// What a fetch did ([`fetch`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchReport {
    /// Distinct objects asked for, and their bytes.
    pub objects: usize,
    pub object_bytes: u64,
    /// Of those, already in the cache.
    pub cached: usize,
    /// Each remote asked, in order.
    pub remotes: Vec<RemoteFetch>,
    /// Objects no remote could provide, with what each remote said.
    pub failures: Vec<(Pointer, Vec<String>)>,
}

impl FetchReport {
    /// Objects downloaded, and their bytes.
    pub fn fetched(&self) -> (usize, u64) {
        self.remotes
            .iter()
            .fold((0, 0), |(n, b), r| (n + r.fetched, b + r.bytes))
    }
}

/// The distinct objects of `pointers`, in order.
fn distinct(pointers: &[Pointer]) -> Vec<Pointer> {
    let mut seen = std::collections::BTreeSet::new();
    pointers
        .iter()
        .filter(|p| seen.insert(p.oid))
        .copied()
        .collect()
}

/// Downloads into `cache` each of `pointers` it lacks: from each of
/// `remotes` in order, each taking the objects the ones before it could
/// not provide. `get` downloads from one remote (one result per object),
/// or says why the remote cannot be asked.
fn download(
    cache: &ObjectCache,
    remotes: &[&RemoteSettings],
    pointers: &[Pointer],
    get: impl Fn(&RemoteSettings, &[Pointer]) -> Result<Vec<Result<(), LfsError>>, LfsError>,
) -> FetchReport {
    let mut report = FetchReport::default();
    let mut left: Vec<Pointer> = Vec::new();
    for p in distinct(pointers) {
        report.objects += 1;
        report.object_bytes += p.size;
        if cache.contains(&p) {
            report.cached += 1;
        } else {
            left.push(p);
        }
    }
    let mut problems: BTreeMap<LfsOid, Vec<String>> = BTreeMap::new();
    for remote in remotes {
        if left.is_empty() {
            break;
        }
        let mut done = RemoteFetch {
            remote: remote.name.clone(),
            ..RemoteFetch::default()
        };
        let results = match get(remote, &left) {
            Ok(results) => results,
            Err(e) => {
                done.unavailable = Some(e.to_string());
                left.iter().map(|_| Err(e.clone())).collect()
            }
        };
        let mut still = Vec::new();
        for (pointer, result) in left.iter().zip(results) {
            match result {
                Ok(()) => {
                    done.fetched += 1;
                    done.bytes += pointer.size;
                }
                Err(e) => {
                    if done.unavailable.is_none() {
                        done.failed += 1;
                    }
                    problems
                        .entry(pointer.oid)
                        .or_default()
                        .push(format!("{}: {e}", remote.name));
                    still.push(*pointer);
                }
            }
        }
        report.remotes.push(done);
        left = still;
    }
    report.failures = left
        .into_iter()
        .map(|p| (p, problems.remove(&p.oid).unwrap_or_default()))
        .collect();
    report
}

/// `agentfs lfs fetch` (design §7): downloads into the cache at `state`
/// each of `pointers` it lacks, from `remotes` (names; every valid remote
/// when `None`), in name order. Holds the cache lock shared meanwhile and
/// no state-area lock, so it runs beside a daemon.
pub fn fetch(
    state: &StateDir,
    settings: &Settings,
    pointers: &[Pointer],
    remotes: Option<&[String]>,
) -> io::Result<FetchReport> {
    let cache = ObjectCache::at(state.lfs())?;
    let _lock = CacheLock::shared(cache.dir())?;
    let chosen: Vec<&RemoteSettings> = settings
        .remotes
        .iter()
        .filter(|r| remotes.is_none_or(|names| names.contains(&r.name)))
        .collect();
    Ok(download(&cache, &chosen, pointers, |remote, left| {
        let transfer = transfer_for(settings, remote, Direction::Download)?;
        Ok(transfer.download_many(left, &cache, &settings.branch_ref))
    }))
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
        let only = CacheOnly::new(ObjectCache::at(state.lfs()).unwrap()).unwrap();
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

    #[test]
    fn prefetch_takes_from_each_remote_in_name_order() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path().join("state"));
        crate::volume::init(&state, "v", None).unwrap();
        let put = |repo: &std::path::Path, content: &[u8]| {
            let pointer = Pointer::of_reader(content).unwrap();
            let hex = pointer.oid.to_string();
            let object = repo.join(format!("lfs/objects/{}/{}/{hex}", &hex[..2], &hex[2..4]));
            fs::create_dir_all(object.parent().unwrap()).unwrap();
            fs::write(&object, content).unwrap();
            pointer
        };
        let a = dir.path().join("a.git");
        let b = dir.path().join("b.git");
        git2::Repository::init_bare(&a).unwrap();
        git2::Repository::init_bare(&b).unwrap();
        let on_a = put(&a, b"only on a");
        let on_b = put(&b, b"only on b");
        let both = put(&a, b"on both");
        put(&b, b"on both");
        {
            let mut config = git2::Config::open(&state.git().join("config")).unwrap();
            config.set_str("remote.a.url", a.to_str().unwrap()).unwrap();
            config.set_str("remote.b.url", b.to_str().unwrap()).unwrap();
        }
        let settings = Settings::load(&state.git()).unwrap();
        let fetcher = Fetcher::new(&state, &settings).unwrap();
        let nowhere = Pointer::of_reader(&b"nowhere"[..]).unwrap();
        fetcher.prefetch(&[on_a, on_b, both, nowhere, on_a]);
        for p in [on_a, on_b, both] {
            assert!(fetcher.cache().contains(&p));
        }
        // What every remote said is kept for `ensure`, which does not ask
        // again: even once the object appears on a remote.
        put(&b, b"nowhere");
        let err = fetcher.ensure(&nowhere).unwrap_err();
        assert!(
            err.message.contains("a: ") && err.message.contains("b: "),
            "{err}"
        );
        // Not prefetched: asked for directly.
        let fresh = Fetcher::new(&state, &settings).unwrap();
        assert!(fresh.ensure(&nowhere).is_ok());
    }
}
