//! `agentfs lfs prune` (design §7): deleting from the local LFS cache the
//! objects every remote's LFS servers hold. Never automatic, and never
//! anything on a server.
//!
//! An object is deleted only when it is confirmed on every valid remote's
//! upload endpoint, and on its download endpoint too where that is
//! another endpoint:
//!
//! - batch API, upload: the server asks for no upload of it, and reports
//!   no error;
//! - batch API, download: the server offers a download of it, and reports
//!   no error;
//! - a local or `file://` remote: the file is there with the object's
//!   size.
//!
//! A per-object `404` means the server lacks it (the object is kept); any
//! other failure aborts the prune. A remote that is misconfigured, or
//! whose endpoint cannot be resolved (an SSH remote without `lfsurl`,
//! a derived `https` endpoint without the `https` feature), refuses the
//! whole prune: nothing can be confirmed there. With no remote nothing is
//! confirmed, so nothing is deleted.
//!
//! [`plan`] confirms without any lock: servers never forget an object, so
//! a confirmation stays true. [`apply`] then takes the cache lock
//! exclusive ([`super::lock::CacheLock`]), which restores, reverts, patch
//! applies and `agentfs lfs fetch` hold shared, so no object they use goes
//! between their check and their write; it deletes the confirmed objects
//! still there and the partial downloads nobody holds.

use std::collections::BTreeMap;
use std::io;
use std::time::{Duration, SystemTime};

use super::lock::CacheLock;
use super::{Direction, LfsError, ObjectCache, Pointer, transfer_for};
use crate::config::Settings;
use crate::volume::StateDir;

#[derive(Debug, thiserror::Error)]
pub enum PruneError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(
        "remote {remote} is misconfigured ({error}): nothing can be confirmed on it, so \
         nothing is pruned; fix or remove it"
    )]
    Misconfigured { remote: String, error: String },
    #[error(
        "remote {remote} has no usable LFS {direction} endpoint ({error}): nothing can be \
         confirmed on it, so nothing is pruned"
    )]
    NoEndpoint {
        remote: String,
        direction: &'static str,
        error: String,
    },
    #[error("asking {endpoint} (remote {remote}) which objects it holds: {error}")]
    Lfs {
        remote: String,
        endpoint: String,
        error: LfsError,
    },
}

/// One endpoint asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub remote: String,
    pub direction: Direction,
    /// [`super::endpoint::LfsEndpoint::describe`].
    pub endpoint: String,
    /// Candidates it holds.
    pub confirmed: usize,
}

/// What a prune would delete.
#[derive(Debug, Clone, Default)]
pub struct PrunePlan {
    /// Every cached object, and their bytes.
    pub cached: usize,
    pub cached_bytes: u64,
    /// Objects cached more recently than `--older-than`, kept unasked.
    pub recent: usize,
    /// No remote is configured: nothing can be confirmed.
    pub no_remotes: bool,
    /// Each endpoint asked.
    pub checked: Vec<Checked>,
    /// Objects confirmed everywhere: to delete.
    pub confirmed: Vec<Pointer>,
    /// Objects some endpoint lacks, with the remotes lacking them.
    pub kept: Vec<(Pointer, Vec<String>)>,
}

impl PrunePlan {
    pub fn confirmed_bytes(&self) -> u64 {
        self.confirmed.iter().map(|p| p.size).sum()
    }

    /// For each remote, how many kept objects it lacks.
    pub fn lacking(&self) -> BTreeMap<String, usize> {
        let mut out = BTreeMap::new();
        for (_, remotes) in &self.kept {
            for remote in remotes {
                *out.entry(remote.clone()).or_insert(0) += 1;
            }
        }
        out
    }
}

/// What a prune deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub pruned: usize,
    pub bytes: u64,
    /// Partial downloads nobody held, deleted.
    pub partials: usize,
    pub partial_bytes: u64,
}

/// Finds the cached objects every remote's endpoints hold (see the module
/// documentation). Objects cached less than `older_than` ago are kept
/// without asking. Takes no lock and deletes nothing.
pub fn plan(
    state: &StateDir,
    settings: &Settings,
    older_than: Option<Duration>,
) -> Result<PrunePlan, PruneError> {
    if let Some(bad) = settings.invalid_remotes.first() {
        return Err(PruneError::Misconfigured {
            remote: bad.name.clone(),
            error: bad.error.clone(),
        });
    }
    let cache = ObjectCache::at(state.lfs())?;
    let now = SystemTime::now();
    let mut plan = PrunePlan::default();
    let mut candidates = Vec::new();
    for (pointer, mtime) in cache.list()? {
        plan.cached += 1;
        plan.cached_bytes += pointer.size;
        let age = now.duration_since(mtime).unwrap_or(Duration::ZERO);
        if older_than.is_some_and(|min| age < min) {
            plan.recent += 1;
        } else {
            candidates.push(pointer);
        }
    }
    if settings.remotes.pushed().is_empty() {
        plan.no_remotes = true;
        return Ok(plan);
    }
    // Resolve every endpoint before asking any server.
    let mut transfers = Vec::new();
    for remote in settings.remotes.pushed() {
        let upload = transfer_for(settings, remote, Direction::Upload).map_err(|e| {
            PruneError::NoEndpoint {
                remote: remote.name.clone(),
                direction: "upload",
                error: e.to_string(),
            }
        })?;
        let download = transfer_for(settings, remote, Direction::Download).map_err(|e| {
            PruneError::NoEndpoint {
                remote: remote.name.clone(),
                direction: "download",
                error: e.to_string(),
            }
        })?;
        transfers.push((remote, Direction::Upload, upload));
        // The same endpoint both ways (the usual case) is asked once.
        if transfers.last().map(|(_, _, t)| &t.endpoint().kind) != Some(&download.endpoint().kind) {
            transfers.push((remote, Direction::Download, download));
        }
    }
    let mut lacking: Vec<Vec<String>> = vec![Vec::new(); candidates.len()];
    if !candidates.is_empty() {
        for (remote, direction, transfer) in &transfers {
            let present = transfer
                .present(&candidates, *direction, &settings.branch_ref)
                .map_err(|error| PruneError::Lfs {
                    remote: remote.name.clone(),
                    endpoint: transfer.endpoint().describe(),
                    error,
                })?;
            let mut confirmed = 0;
            for (i, here) in present.into_iter().enumerate() {
                if here {
                    confirmed += 1;
                } else if !lacking[i].contains(&remote.name) {
                    lacking[i].push(remote.name.clone());
                }
            }
            plan.checked.push(Checked {
                remote: remote.name.clone(),
                direction: *direction,
                endpoint: transfer.endpoint().describe(),
                confirmed,
            });
        }
    }
    for (pointer, lacking) in candidates.into_iter().zip(lacking) {
        if lacking.is_empty() {
            plan.confirmed.push(pointer);
        } else {
            plan.kept.push((pointer, lacking));
        }
    }
    Ok(plan)
}

/// Deletes `plan`'s confirmed objects and the partial downloads nobody
/// holds, under the cache lock taken exclusive, waiting up to `wait` for
/// restores and fetches holding it. `None` if they still hold it then:
/// nothing was deleted.
pub fn apply(
    state: &StateDir,
    plan: &PrunePlan,
    wait: Duration,
) -> Result<Option<PruneReport>, PruneError> {
    let cache = ObjectCache::at(state.lfs())?;
    let Some(_lock) = CacheLock::exclusive(cache.dir(), wait)? else {
        return Ok(None);
    };
    let mut report = PruneReport::default();
    for pointer in &plan.confirmed {
        if cache.remove(pointer)? {
            report.pruned += 1;
            report.bytes += pointer.size;
        }
    }
    let (partials, partial_bytes) = cache.remove_free_partials()?;
    report.partials = partials;
    report.partial_bytes = partial_bytes;
    Ok(Some(report))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::events::PushFailureKind;
    use crate::lfs::fake::FakeLfs;

    struct Fx {
        dir: tempfile::TempDir,
        state: StateDir,
    }

    impl Fx {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let state = StateDir::new(dir.path().join("state"));
            crate::volume::init(&state, "v", None).unwrap();
            Self { dir, state }
        }

        fn remote(&self, name: &str) -> PathBuf {
            let path = self.dir.path().join(format!("{name}.git"));
            git2::Repository::init_bare(&path).unwrap();
            self.config(&format!("remote.{name}.url"), path.to_str().unwrap());
            path
        }

        fn config(&self, key: &str, value: &str) {
            let mut config = git2::Config::open(&self.state.git().join("config")).unwrap();
            config.set_str(key, value).unwrap();
        }

        fn settings(&self) -> Settings {
            Settings::load(&self.state.git()).unwrap()
        }

        fn cache(&self) -> ObjectCache {
            ObjectCache::at(self.state.lfs()).unwrap()
        }

        /// Caches `content` and returns its pointer.
        fn cached(&self, content: &[u8]) -> Pointer {
            let src = self.dir.path().join("src");
            fs::write(&src, content).unwrap();
            self.cache().insert_file(&src).unwrap()
        }
    }

    /// Puts `content`'s object on a file remote.
    fn put(remote: &Path, content: &[u8]) -> Pointer {
        let pointer = Pointer::of_reader(content).unwrap();
        let hex = pointer.oid.to_string();
        let object = remote.join(format!("lfs/objects/{}/{}/{hex}", &hex[..2], &hex[2..4]));
        fs::create_dir_all(object.parent().unwrap()).unwrap();
        fs::write(object, content).unwrap();
        pointer
    }

    #[test]
    fn only_objects_on_every_remote_are_pruned() {
        let fx = Fx::new();
        let a = fx.remote("a");
        let b = fx.remote("b");
        let both = fx.cached(b"on both remotes");
        let only_a = fx.cached(b"only on a");
        let neither = fx.cached(b"on neither");
        put(&a, b"on both remotes");
        put(&b, b"on both remotes");
        put(&a, b"only on a");
        let plan = super::plan(&fx.state, &fx.settings(), None).unwrap();
        assert_eq!(plan.cached, 3);
        assert_eq!(plan.confirmed, [both]);
        let mut kept = plan.kept.clone();
        kept.sort();
        let mut want = vec![
            (only_a, vec!["b".to_owned()]),
            (neither, vec!["a".to_owned(), "b".to_owned()]),
        ];
        want.sort();
        assert_eq!(kept, want);
        assert_eq!(plan.lacking()["b"], 2);
        // A file remote's upload and download endpoints are one: asked once.
        assert_eq!(plan.checked.len(), 2);
        // Planning deleted nothing (a dry run stops here).
        assert!(fx.cache().contains(&both));
        let report = apply(&fx.state, &plan, Duration::ZERO).unwrap().unwrap();
        assert_eq!((report.pruned, report.bytes), (1, both.size));
        assert!(!fx.cache().contains(&both));
        assert!(fx.cache().contains(&only_a) && fx.cache().contains(&neither));
        // Its empty directories went with it.
        let hex = both.oid.to_string();
        assert!(!fx.state.lfs_objects().join(&hex[..2]).exists());
    }

    #[test]
    fn misconfigured_or_endpointless_remotes_refuse_the_prune() {
        let fx = Fx::new();
        fx.remote("a");
        fx.cached(b"x");
        fx.config("remote.bad.url", "/x");
        fx.config("remote.bad.agentfsMaxBlobSize", "0");
        let err = super::plan(&fx.state, &fx.settings(), None).unwrap_err();
        assert!(
            matches!(&err, PruneError::Misconfigured { remote, .. } if remote == "bad"),
            "{err}"
        );
        let fx = Fx::new();
        fx.remote("a");
        fx.cached(b"x");
        fx.config("remote.git.url", "git://git.example/r.git");
        let err = super::plan(&fx.state, &fx.settings(), None).unwrap_err();
        assert!(
            matches!(&err, PruneError::NoEndpoint { remote, .. } if remote == "git"),
            "{err}"
        );
        assert!(err.to_string().contains("nothing is pruned"), "{err}");
        // No remote at all: nothing confirmed, nothing deleted.
        let fx = Fx::new();
        let p = fx.cached(b"x");
        let plan = super::plan(&fx.state, &fx.settings(), None).unwrap();
        assert!(plan.no_remotes && plan.confirmed.is_empty());
        apply(&fx.state, &plan, Duration::ZERO).unwrap().unwrap();
        assert!(fx.cache().contains(&p));
    }

    #[test]
    fn recent_objects_are_kept_with_older_than() {
        let fx = Fx::new();
        let a = fx.remote("a");
        let old = fx.cached(b"old object");
        let new = fx.cached(b"new object");
        put(&a, b"old object");
        put(&a, b"new object");
        let path = fx.cache().path(&old.oid);
        let day_ago = SystemTime::now() - Duration::from_secs(86_400);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(day_ago)
            .unwrap();
        let plan = super::plan(&fx.state, &fx.settings(), Some(Duration::from_secs(3600))).unwrap();
        assert_eq!(plan.confirmed, [old]);
        assert_eq!(plan.recent, 1);
        apply(&fx.state, &plan, Duration::ZERO).unwrap().unwrap();
        assert!(fx.cache().contains(&new));
    }

    #[test]
    fn the_lock_keeps_prunes_out_of_restores_and_free_partials_go() {
        let fx = Fx::new();
        let a = fx.remote("a");
        let p = fx.cached(b"on a");
        put(&a, b"on a");
        let plan = super::plan(&fx.state, &fx.settings(), None).unwrap();
        // A restore holds the lock shared: the prune waits, then gives up.
        let held = CacheLock::shared(&fx.state.lfs()).unwrap();
        assert!(
            apply(&fx.state, &plan, Duration::from_millis(200))
                .unwrap()
                .is_none()
        );
        assert!(fx.cache().contains(&p));
        drop(held);
        // A partial download in use is kept, one nobody holds goes.
        let cache = fx.cache();
        let busy = Pointer::of_reader(&b"being downloaded"[..]).unwrap();
        let holder = cache.partial(&busy).unwrap().unwrap();
        fs::write(cache.incomplete_dir().join("0".repeat(64)), b"abandoned").unwrap();
        let report = apply(&fx.state, &plan, Duration::ZERO).unwrap().unwrap();
        assert_eq!(report.pruned, 1);
        assert_eq!((report.partials, report.partial_bytes), (1, 9));
        assert!(cache.incomplete_dir().join(busy.oid.to_string()).exists());
        drop(holder);
    }

    #[test]
    fn http_endpoints_confirm_through_the_batch_api() {
        let fx = Fx::new();
        let server = FakeLfs::start();
        let a = fx.remote("a");
        fx.config("remote.a.lfsurl", &server.url());
        let on_server = fx.cached(b"on the server");
        let not = fx.cached(b"not on the server");
        server.with(|st| {
            st.objects
                .insert(on_server.oid.to_string(), b"on the server".to_vec())
        });
        let plan = super::plan(&fx.state, &fx.settings(), None).unwrap();
        assert_eq!(plan.confirmed, [on_server]);
        assert_eq!(plan.kept, [(not, vec!["a".to_owned()])]);
        let _ = a;
        // Uploads and downloads go to one endpoint: one batch, uploads.
        let batches: Vec<_> = server
            .seen()
            .into_iter()
            .filter(|s| s.path.ends_with("/batch"))
            .collect();
        assert_eq!(batches.len(), 1);
        // A server failure aborts the prune.
        server.with(|st| st.batch_status = Some(500));
        let err = plan_err(&fx);
        assert!(
            matches!(&err, PruneError::Lfs { error, .. } if error.kind == PushFailureKind::Unreachable),
            "{err}"
        );
        // Separate upload and download endpoints: both asked.
        server.with(|st| st.batch_status = None);
        let other = FakeLfs::start();
        fx.config("remote.a.lfspushurl", &other.url());
        other.with(|st| {
            st.objects
                .insert(on_server.oid.to_string(), b"on the server".to_vec())
        });
        let plan = super::plan(&fx.state, &fx.settings(), None).unwrap();
        assert_eq!(plan.checked.len(), 2);
        assert_eq!(plan.confirmed, [on_server]);
        // The download endpoint lacks it: kept.
        server.with(|st| st.objects.clear());
        let plan = plan_ok(&fx);
        assert!(plan.confirmed.is_empty());
    }

    fn plan_err(fx: &Fx) -> PruneError {
        super::plan(&fx.state, &fx.settings(), None).unwrap_err()
    }

    fn plan_ok(fx: &Fx) -> PrunePlan {
        super::plan(&fx.state, &fx.settings(), None).unwrap()
    }
}
