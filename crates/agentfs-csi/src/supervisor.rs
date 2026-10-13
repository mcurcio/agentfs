//! The node plugin's supervisor (design §14.2): one `agentfs
//! serve-history` per staged volume, one `agentfs mount --history-service`
//! front per published target, and their recovery.
//!
//! - **Stage** initialises the state area under its lock (static PVs are
//!   created here too), installs the profile's rules, records the volume
//!   in `<node-state>/staged/`, starts the service on a node-local socket
//!   and waits for it to write its address (at most `stage_wait`; then
//!   `UNAVAILABLE`, and the kubelet retries while the service recovers its
//!   journal or fetches patches).
//! - **Publish** starts a front that FUSE-mounts the backing tree at its
//!   own source directory, waits until that mount is live, bind-mounts it
//!   onto the kubelet's target and records the target in
//!   `<node-state>/published/`.
//! - **A front that exits unasked** (a crash, `kill -9`, an external
//!   unmount) is restarted after a backoff in the same pending slot, with
//!   the same pod flags; its stale source is detached and the new mount is
//!   bind-stacked over the stale target, without unmounting it first, so
//!   it propagates into containers that mount the volume with
//!   `HostToContainer`. A target whose directory is gone (the pod is gone)
//!   is forgotten instead.
//! - **A service that exits unasked** is restarted after a backoff; it
//!   recovers its journal, and the fronts reconnect by themselves.
//! - **Unpublish** detaches the target, then stops the front (SIGTERM,
//!   SIGKILL after `unpublish_grace`). **Unstage** is refused while
//!   targets are published, then stops the service (SIGTERM, SIGKILL after
//!   `unstage_grace`; the journal makes that safe).
//! - **Reconcile**, at plugin start, registers the node and rebuilds the
//!   above from the records: services for staged volumes, front recovery
//!   for targets whose topmost mount is a stale agentfs mount.
//!
//! Each volume's calls run one at a time (a per-volume async lock); waits
//! for a restarted child happen outside it.

use std::collections::HashMap;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use agentfs_core::history_service::FrontLease;
use agentfs_core::volume::{self as core_volume, StateDir};
use agentfs_fuse::MountState;
use rustix::process::Signal;
use tokio::sync::Mutex as AsyncMutex;

use crate::error::Error;
use crate::launcher::{self, Exit, FrontOptions, Launcher, PluginIdentity, Spawned};
use crate::layout::{Layout, target_key};
use crate::mounter::Mounter;
use crate::params::{PodInfo, VolumeParams};
use crate::records::{self, PublishedRecord, StagedRecord};
use crate::volume;

/// Restart delays: `initial`, doubling up to `max`, back to `initial`
/// once a child has run for `reset_after`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
    pub reset_after: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
            reset_after: Duration::from_secs(60),
        }
    }
}

/// The delay before restarting a child that ran for `uptime`, after the
/// previous delay `prev` (none for a first crash).
pub fn next_backoff(prev: Option<Duration>, uptime: Duration, cfg: &Backoff) -> Duration {
    match prev {
        Some(prev) if uptime < cfg.reset_after => (prev * 2).min(cfg.max).max(cfg.initial),
        _ => cfg.initial,
    }
}

/// The node plugin's settings.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub node_id: String,
    pub layout: Layout,
    /// The plugin pod's own identity, given to the history services.
    pub plugin: PluginIdentity,
    pub front: FrontOptions,
    pub stage_wait: Duration,
    pub publish_wait: Duration,
    pub unpublish_grace: Duration,
    pub unstage_grace: Duration,
    pub backoff: Backoff,
    /// How often readiness is checked.
    pub poll: Duration,
}

impl NodeConfig {
    /// The defaults of `agentfs-csi --mode node`.
    pub fn new(node_id: impl Into<String>, layout: Layout) -> Self {
        Self {
            node_id: node_id.into(),
            layout,
            plugin: PluginIdentity::default(),
            front: FrontOptions {
                passthrough: "auto".into(),
                threads: 4,
                history_wait: Duration::from_secs(30),
            },
            stage_wait: Duration::from_secs(60),
            publish_wait: Duration::from_secs(60),
            unpublish_grace: Duration::from_secs(20),
            unstage_grace: Duration::from_secs(30),
            backoff: Backoff::default(),
            poll: Duration::from_millis(100),
        }
    }
}

/// NodeStage, validated.
#[derive(Debug, Clone)]
pub struct StageRequest {
    pub handle: String,
    pub volume_context: HashMap<String, String>,
}

/// NodePublish, validated.
#[derive(Debug, Clone)]
pub struct PublishRequest {
    pub handle: String,
    pub target: PathBuf,
    pub readonly: bool,
    /// SINGLE_NODE_SINGLE_WRITER (ReadWriteOncePod).
    pub single_writer: bool,
    pub pod: Option<PodInfo>,
}

/// What [`Supervisor::reconcile`] found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Services started for staged volumes.
    pub services: usize,
    /// Targets whose fronts are being recovered.
    pub recovering: usize,
    /// Records of targets that were gone or no longer mounted.
    pub dropped: usize,
    /// Records that could not be read.
    pub unreadable: Vec<String>,
}

/// A supervised child.
#[derive(Debug)]
struct Child {
    spawned: Spawned,
    generation: u64,
    started: Instant,
    /// Asked to stop: its exit is not a crash.
    stopping: bool,
    /// The delay before this child was started, if it is a restart.
    backoff: Option<Duration>,
}

#[derive(Debug)]
struct ServiceEntry {
    child: Child,
}

#[derive(Debug)]
struct FrontEntry {
    child: Child,
    record: PublishedRecord,
}

#[derive(Debug, Default)]
struct Children {
    services: HashMap<String, ServiceEntry>,
    fronts: HashMap<PathBuf, FrontEntry>,
}

pub struct Supervisor {
    cfg: NodeConfig,
    launcher: Arc<dyn Launcher>,
    mounter: Arc<dyn Mounter>,
    children: Mutex<Children>,
    volume_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    generation: AtomicU64,
    shutting_down: AtomicBool,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor")
            .field("node_id", &self.cfg.node_id)
            .finish_non_exhaustive()
    }
}

fn io_error(what: impl std::fmt::Display) -> impl FnOnce(io::Error) -> Error {
    move |e| Error::io(what, e)
}

/// Whether something is at `path`. Only `NotFound` means absent: a stale
/// FUSE mount answers `stat` with `ENOTCONN`, and it is very much there.
pub(crate) fn exists(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(e) => e.kind() != io::ErrorKind::NotFound,
    }
}

impl Supervisor {
    pub fn new(
        cfg: NodeConfig,
        launcher: Arc<dyn Launcher>,
        mounter: Arc<dyn Mounter>,
    ) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            launcher,
            mounter,
            children: Mutex::new(Children::default()),
            volume_locks: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(1),
            shutting_down: AtomicBool::new(false),
        })
    }

    pub fn config(&self) -> &NodeConfig {
        &self.cfg
    }

    fn layout(&self) -> &Layout {
        &self.cfg.layout
    }

    fn children(&self) -> MutexGuard<'_, Children> {
        // A panic while holding it leaves plain maps; keep going.
        self.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn lock_volume(&self, handle: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self
            .volume_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(handle.to_owned())
            .or_default()
            .clone();
        lock.lock_owned().await
    }

    /// Runs a mount-table call on the blocking pool: `statfs` on a hung
    /// FUSE mount can block.
    async fn mount_call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn Mounter) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let mounter = self.mounter.clone();
        tokio::task::spawn_blocking(move || f(mounter.as_ref()))
            .await
            .map_err(io::Error::other)?
    }

    async fn mount_state(&self, path: &Path) -> io::Result<MountState> {
        let path = path.to_path_buf();
        self.mount_call(move |m| m.state(&path)).await
    }

    async fn detach_all(&self, path: &Path) -> Result<usize, Error> {
        let p = path.to_path_buf();
        self.mount_call(move |m| m.detach_all(&p))
            .await
            .map_err(io_error(format!("detaching {}", path.display())))
    }

    /// The pid of the volume's history service, while it runs.
    pub fn service_pid(&self, handle: &str) -> Option<u32> {
        let children = self.children();
        let child = &children.services.get(handle)?.child;
        child.spawned.exit().is_none().then_some(child.spawned.pid)
    }

    /// The pid of the front serving `target`, while it runs.
    pub fn front_pid(&self, target: &Path) -> Option<u32> {
        let children = self.children();
        let child = &children.fronts.get(target)?.child;
        child.spawned.exit().is_none().then_some(child.spawned.pid)
    }

    /// The pending slot of the front serving `target`, once known.
    pub fn front_slot(&self, target: &Path) -> Option<u16> {
        self.children().fronts.get(target)?.record.slot
    }

    fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::Relaxed)
    }

    // ----- services -------------------------------------------------------

    /// NodeStage.
    pub async fn stage(self: &Arc<Self>, req: StageRequest) -> Result<(), Error> {
        let _lock = self.lock_volume(&req.handle).await;
        let handle = req.handle.as_str();
        if !self.children().services.contains_key(handle) {
            let layout = self.layout().clone();
            let h = handle.to_owned();
            let context = req.volume_context.clone();
            let params = tokio::task::spawn_blocking(move || prepare(&layout, &h, &context))
                .await
                .map_err(|e| Error::Internal(format!("staging task failed: {e}")))??;
            self.start_service(handle, None)?;
            eprintln!(
                "staging volume {handle} (agentfs volume {}): history service started",
                params.volume
            );
        }
        self.wait_service(handle).await
    }

    /// Starts the volume's service and its monitor. The caller holds the
    /// volume's lock.
    fn start_service(
        self: &Arc<Self>,
        handle: &str,
        backoff: Option<Duration>,
    ) -> Result<(), Error> {
        let layout = self.layout();
        let run = layout.run_dir(handle);
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&run)
            .map_err(io_error(format!("creating {}", run.display())))?;
        let address = address_file(layout, handle);
        records::remove(&address).map_err(io_error(format!("removing {}", address.display())))?;
        let spec = launcher::service_spec(layout, handle, &self.cfg.plugin);
        let spawned = self
            .launcher
            .spawn(spec)
            .map_err(io_error("starting agentfs serve-history"))?;
        let generation = self.next_generation();
        let mut exited = spawned.exited.clone();
        self.children().services.insert(
            handle.to_owned(),
            ServiceEntry {
                child: Child {
                    spawned,
                    generation,
                    started: Instant::now(),
                    stopping: false,
                    backoff,
                },
            },
        );
        let this = Arc::clone(self);
        let handle = handle.to_owned();
        tokio::spawn(async move {
            let exit = exited.wait_for(Option::is_some).await.ok().and_then(|e| *e);
            this.service_exited(handle, generation, exit).await;
        });
        Ok(())
    }

    /// Waits until the service has written its address. The caller holds
    /// the volume's lock.
    async fn wait_service(&self, handle: &str) -> Result<(), Error> {
        let expected = format!("unix:{}", self.layout().socket(handle).display());
        let address = address_file(self.layout(), handle);
        let deadline = Instant::now() + self.cfg.stage_wait;
        loop {
            let exited = {
                let children = self.children();
                let Some(entry) = children.services.get(handle) else {
                    return Err(Error::Internal(format!(
                        "volume {handle}'s history service is not supervised"
                    )));
                };
                entry
                    .child
                    .spawned
                    .exit()
                    .map(|e| (e, entry.child.spawned.recent()))
            };
            match exited {
                None => {
                    let found = fs::read_to_string(&address).unwrap_or_default();
                    if found.trim() == expected {
                        return Ok(());
                    }
                }
                Some((exit, log)) => {
                    if self.has_fronts(handle) {
                        return Err(Error::Unavailable(format!(
                            "volume {handle}'s history service exited ({exit}) and is being \
                             restarted"
                        )));
                    }
                    // Not a restart: give up, and let the kubelet retry
                    // from the start.
                    if let Some(entry) = self.children().services.remove(handle) {
                        drop(entry);
                    }
                    let _ = records::remove(&self.layout().staged_record(handle));
                    return Err(Error::FailedPrecondition(format!(
                        "volume {handle}'s history service exited ({exit}):\n{log}"
                    )));
                }
            }
            if Instant::now() >= deadline {
                return Err(Error::Unavailable(format!(
                    "volume {handle}'s history service is starting (journal recovery or patch \
                     fetch); still waiting after {:?}",
                    self.cfg.stage_wait
                )));
            }
            tokio::time::sleep(self.cfg.poll).await;
        }
    }

    fn has_fronts(&self, handle: &str) -> bool {
        self.children()
            .fronts
            .values()
            .any(|f| f.record.handle == handle)
    }

    async fn service_exited(self: Arc<Self>, handle: String, generation: u64, exit: Option<Exit>) {
        let (uptime, prev) = {
            let children = self.children();
            match children.services.get(&handle) {
                Some(e) if e.child.generation == generation && !e.child.stopping => {
                    (e.child.started.elapsed(), e.child.backoff)
                }
                _ => return,
            }
        };
        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let delay = next_backoff(prev, uptime, &self.cfg.backoff);
        eprintln!(
            "service for {handle} exited ({}); restarting in {delay:?}",
            describe(exit)
        );
        tokio::time::sleep(delay).await;
        let _lock = self.lock_volume(&handle).await;
        let current = matches!(
            self.children().services.get(&handle),
            Some(e) if e.child.generation == generation && !e.child.stopping
        );
        if !current || self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        if let Err(e) = self.start_service(&handle, Some(delay)) {
            eprintln!("restarting the service for {handle}: {e}");
            // Try again later: a monitor needs a child to watch.
            self.children().services.remove(&handle);
        }
    }

    /// NodeUnstage.
    pub async fn unstage(&self, handle: &str) -> Result<(), Error> {
        let _lock = self.lock_volume(handle).await;
        let published = self.published_targets(handle)?;
        if !published.is_empty() {
            return Err(Error::FailedPrecondition(format!(
                "volume {handle} is still published at {}",
                published
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let child = self.children().services.get_mut(handle).map(|e| {
            e.child.stopping = true;
            e.child.spawned.clone()
        });
        if let Some(spawned) = child {
            self.stop_child(
                &spawned,
                self.cfg.unstage_grace,
                &format!("service for {handle}"),
            )
            .await;
            self.children().services.remove(handle);
            eprintln!("unstaged volume {handle}: history service stopped");
        }
        let layout = self.layout();
        records::remove(&layout.staged_record(handle))
            .map_err(io_error("removing the staged record"))?;
        match fs::remove_dir_all(layout.run_dir(handle)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(Error::io("removing the service's socket directory", e));
            }
            _ => {}
        }
        Ok(())
    }

    /// Targets published for `handle`: supervised fronts and records.
    fn published_targets(&self, handle: &str) -> Result<Vec<PathBuf>, Error> {
        let mut targets: Vec<PathBuf> = self
            .children()
            .fronts
            .iter()
            .filter(|(_, f)| f.record.handle == handle)
            .map(|(t, _)| t.clone())
            .collect();
        let dir = self.layout().published_dir(handle);
        let (found, _) = records::load_all::<PublishedRecord>(&dir)
            .map_err(io_error(format!("reading {}", dir.display())))?;
        for rec in found {
            if !targets.contains(&rec.target) {
                targets.push(rec.target);
            }
        }
        targets.sort();
        Ok(targets)
    }

    /// SIGTERM, then SIGKILL after `grace`; waits for the exit.
    async fn stop_child(&self, spawned: &Spawned, grace: Duration, what: &str) {
        let mut exited = spawned.exited.clone();
        if spawned.exit().is_some() {
            return;
        }
        let _ = self.launcher.signal(spawned.pid, Signal::TERM);
        if tokio::time::timeout(grace, exited.wait_for(Option::is_some))
            .await
            .is_err()
        {
            eprintln!("{what} did not stop within {grace:?}; killing it");
            let _ = self.launcher.signal(spawned.pid, Signal::KILL);
            let _ = tokio::time::timeout(Duration::from_secs(10), exited.wait_for(Option::is_some))
                .await;
        }
    }

    // ----- fronts ---------------------------------------------------------

    /// NodePublish.
    pub async fn publish(self: &Arc<Self>, req: PublishRequest) -> Result<(), Error> {
        let _lock = self.lock_volume(&req.handle).await;
        let handle = req.handle.as_str();
        let target = req.target.as_path();
        if !self.children().services.contains_key(handle) {
            return Err(Error::FailedPrecondition(format!(
                "volume {handle} is not staged on this node"
            )));
        }
        {
            let children = self.children();
            if let Some(f) = children.fronts.get(target)
                && f.record.handle != handle
            {
                return Err(Error::FailedPrecondition(format!(
                    "{} is already published for volume {}",
                    target.display(),
                    f.record.handle
                )));
            }
            let other = children
                .fronts
                .iter()
                .find(|(t, f)| f.record.handle == handle && t.as_path() != target);
            if let Some((other, f)) = other
                && (req.single_writer || f.record.single_writer)
            {
                return Err(Error::FailedPrecondition(format!(
                    "volume {handle} is single-writer (ReadWriteOncePod) and already published \
                     at {}",
                    other.display()
                )));
            }
        }
        let key = target_key(target);
        let source = self.layout().front_source(handle, &key);
        if !self.children().fronts.contains_key(target) {
            fs::create_dir_all(target)
                .map_err(io_error(format!("creating {}", target.display())))?;
            let record = PublishedRecord {
                handle: handle.to_owned(),
                target: target.to_path_buf(),
                readonly: req.readonly,
                single_writer: req.single_writer,
                pod: req.pod.clone(),
                slot: None,
                front_pid: None,
            };
            self.start_front(record, None).await?;
        }
        match self
            .wait_front(target, &source, self.cfg.publish_wait)
            .await
        {
            FrontWait::Live => {}
            FrontWait::Exited(exit, log) => {
                if let Some(entry) = self.children().fronts.remove(target) {
                    drop(entry);
                }
                let _ = self.detach_all(&source).await;
                return Err(Error::FailedPrecondition(format!(
                    "the front for {} exited ({exit}):\n{log}",
                    target.display()
                )));
            }
            FrontWait::Gone => {
                return Err(Error::Aborted(format!(
                    "{} was unpublished meanwhile",
                    target.display()
                )));
            }
            FrontWait::Timeout => {
                return Err(Error::Unavailable(format!(
                    "the front for {} is not mounted yet after {:?} (waiting for the history \
                     service?)",
                    target.display(),
                    self.cfg.publish_wait
                )));
            }
        }
        self.attach_target(target, &source).await
    }

    /// Detaches a leftover source mount, then starts a front for `record`
    /// (preferring its slot) and its monitor. The caller holds the
    /// volume's lock.
    async fn start_front(
        self: &Arc<Self>,
        record: PublishedRecord,
        backoff: Option<Duration>,
    ) -> Result<(), Error> {
        let handle = record.handle.clone();
        let target = record.target.clone();
        let key = target_key(&target);
        let source = self.layout().front_source(&handle, &key);
        fs::create_dir_all(&source).or_else(|e| {
            // A stale mount is detached below. `mkdir` on it fails with
            // EEXIST, and create_dir_all then cannot stat it (ENOTCONN).
            if e.kind() == io::ErrorKind::AlreadyExists
                || e.raw_os_error() == Some(rustix::io::Errno::NOTCONN.raw_os_error())
            {
                Ok(())
            } else {
                Err(Error::io(format!("creating {}", source.display()), e))
            }
        })?;
        self.detach_all(&source).await?;
        let spec = launcher::front_spec(
            self.layout(),
            &handle,
            &key,
            record.pod.as_ref(),
            record.slot,
            &self.cfg.node_id,
            &self.cfg.front,
        );
        let spawned = self
            .launcher
            .spawn(spec)
            .map_err(io_error("starting agentfs mount"))?;
        let generation = self.next_generation();
        let mut exited = spawned.exited.clone();
        let mut record = record;
        record.front_pid = Some(spawned.pid);
        self.children().fronts.insert(
            target.clone(),
            FrontEntry {
                child: Child {
                    spawned,
                    generation,
                    started: Instant::now(),
                    stopping: false,
                    backoff,
                },
                record,
            },
        );
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let exit = exited.wait_for(Option::is_some).await.ok().and_then(|e| *e);
            this.front_exited(handle, target, generation, exit).await;
        });
        Ok(())
    }

    /// Waits until the front for `target` has its source mounted.
    async fn wait_front(&self, target: &Path, source: &Path, wait: Duration) -> FrontWait {
        let deadline = Instant::now() + wait;
        let generation = self
            .children()
            .fronts
            .get(target)
            .map(|f| f.child.generation);
        loop {
            let exited = {
                let children = self.children();
                match children.fronts.get(target) {
                    Some(f) if Some(f.child.generation) == generation && !f.child.stopping => f
                        .child
                        .spawned
                        .exit()
                        .map(|e| (e, f.child.spawned.recent())),
                    _ => return FrontWait::Gone,
                }
            };
            if let Some((exit, log)) = exited {
                return FrontWait::Exited(exit, log);
            }
            if matches!(self.mount_state(source).await, Ok(MountState::Live { .. })) {
                return FrontWait::Live;
            }
            if Instant::now() >= deadline {
                return FrontWait::Timeout;
            }
            tokio::time::sleep(self.cfg.poll).await;
        }
    }

    /// With the front's source live: learns its slot, binds it onto the
    /// target unless the target's topmost mount is already live, and
    /// records the target. The caller holds the volume's lock.
    async fn attach_target(&self, target: &Path, source: &Path) -> Result<(), Error> {
        let (handle, pid, known_slot, readonly) = {
            let children = self.children();
            let f = children
                .fronts
                .get(target)
                .ok_or_else(|| Error::Aborted(format!("{} is unpublished", target.display())))?;
            (
                f.record.handle.clone(),
                f.child.spawned.pid,
                f.record.slot,
                f.record.readonly,
            )
        };
        let state = StateDir::new(self.layout().state(&handle));
        let slot = tokio::task::spawn_blocking(move || slot_of(&state, pid))
            .await
            .ok()
            .flatten()
            .or(known_slot);
        let top = self
            .mount_state(target)
            .await
            .map_err(io_error(format!("inspecting {}", target.display())))?;
        let stacked = !matches!(top, MountState::NotMounted);
        if !matches!(top, MountState::Live { .. }) {
            let (s, t) = (source.to_path_buf(), target.to_path_buf());
            self.mount_call(move |m| m.bind(&s, &t, readonly))
                .await
                .map_err(io_error(format!(
                    "bind-mounting {} onto {}",
                    source.display(),
                    target.display()
                )))?;
        }
        let record = {
            let mut children = self.children();
            let f = children
                .fronts
                .get_mut(target)
                .ok_or_else(|| Error::Aborted(format!("{} is unpublished", target.display())))?;
            f.record.slot = slot;
            f.record.front_pid = Some(pid);
            f.record.clone()
        };
        let path = self.layout().published_record(&handle, &target_key(target));
        records::store(&path, &record).map_err(io_error(format!("writing {}", path.display())))?;
        if stacked && !matches!(top, MountState::Live { .. }) {
            eprintln!(
                "front for {} restarted in slot {}; its mount is stacked over the stale one at {}",
                who(&record),
                slot.map_or_else(|| "?".to_owned(), |s| s.to_string()),
                target.display()
            );
        }
        Ok(())
    }

    /// A boxed future: it restarts a front, whose monitor calls it again.
    fn front_exited(
        self: Arc<Self>,
        handle: String,
        target: PathBuf,
        generation: u64,
        exit: Option<Exit>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async move {
            let (uptime, prev, record) = {
                let children = self.children();
                match children.fronts.get(&target) {
                    Some(f) if f.child.generation == generation && !f.child.stopping => {
                        (f.child.started.elapsed(), f.child.backoff, f.record.clone())
                    }
                    _ => return,
                }
            };
            if self.shutting_down.load(Ordering::Relaxed) {
                return;
            }
            let delay = next_backoff(prev, uptime, &self.cfg.backoff);
            eprintln!(
                "front for {} exited ({}); restarting in {delay:?} in slot {}",
                who(&record),
                describe(exit),
                record
                    .slot
                    .map_or_else(|| "?".to_owned(), |s| s.to_string())
            );
            tokio::time::sleep(delay).await;
            let lock = self.lock_volume(&handle).await;
            let current = matches!(
                self.children().fronts.get(&target),
                Some(f) if f.child.generation == generation && !f.child.stopping
            );
            if !current || self.shutting_down.load(Ordering::Relaxed) {
                return;
            }
            // Read the record again: attach_target may have learnt the slot.
            let record = self
                .children()
                .fronts
                .get(&target)
                .map_or(record, |f| f.record.clone());
            self.recover_front(lock, record, Some(delay)).await;
        })
    }

    /// Restarts the front of a published target and stacks its mount over
    /// the target, or forgets the target if its directory is gone. Takes
    /// the volume's lock, and releases it while the front starts.
    async fn recover_front(
        self: &Arc<Self>,
        lock: tokio::sync::OwnedMutexGuard<()>,
        record: PublishedRecord,
        backoff: Option<Duration>,
    ) {
        let handle = record.handle.clone();
        let target = record.target.clone();
        let key = target_key(&target);
        let source = self.layout().front_source(&handle, &key);
        if !exists(&target) {
            eprintln!(
                "{} is gone; forgetting the front for {}",
                target.display(),
                who(&record)
            );
            self.children().fronts.remove(&target);
            let _ = self.detach_all(&source).await;
            let _ = fs::remove_dir(&source);
            let _ = records::remove(&self.layout().published_record(&handle, &key));
            return;
        }
        if let Err(e) = self.start_front(record, backoff).await {
            eprintln!("restarting the front for {}: {e}", target.display());
            self.children().fronts.remove(&target);
            return;
        }
        drop(lock);
        // No timeout: the front waits for the service itself, and its
        // exit restarts it again.
        match self.wait_front(&target, &source, Duration::MAX / 4).await {
            FrontWait::Live => {
                let _lock = self.lock_volume(&handle).await;
                if let Err(e) = self.attach_target(&target, &source).await {
                    eprintln!("recovering {}: {e}", target.display());
                }
            }
            FrontWait::Exited(..) | FrontWait::Gone | FrontWait::Timeout => {}
        }
    }

    /// NodeUnpublish.
    pub async fn unpublish(&self, handle: &str, target: &Path) -> Result<(), Error> {
        let _lock = self.lock_volume(handle).await;
        let key = target_key(target);
        let child = self.children().fronts.get_mut(target).map(|f| {
            f.child.stopping = true;
            f.child.spawned.clone()
        });
        // The target first, so the front's final sweep sees no new opens.
        if exists(target) {
            self.detach_all(target).await?;
        }
        if let Some(spawned) = child {
            self.stop_child(
                &spawned,
                self.cfg.unpublish_grace,
                &format!("front for {}", target.display()),
            )
            .await;
            self.children().fronts.remove(target);
        }
        let source = self.layout().front_source(handle, &key);
        if exists(&source) {
            self.detach_all(&source).await?;
            let _ = fs::remove_dir(&source);
        }
        match fs::remove_dir(target) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(Error::io(format!("removing {}", target.display()), e));
            }
            _ => {}
        }
        records::remove(&self.layout().published_record(handle, &key))
            .map_err(io_error("removing the published record"))?;
        eprintln!("unpublished volume {handle} from {}", target.display());
        Ok(())
    }

    // ----- start and stop -------------------------------------------------

    /// At plugin start: registers the node, starts the services of staged
    /// volumes and recovers published targets (in the background).
    pub async fn reconcile(self: &Arc<Self>) -> Result<ReconcileReport, Error> {
        let layout = self.layout().clone();
        let marker = layout.node_marker(&self.cfg.node_id);
        fs::create_dir_all(layout.nodes_dir())
            .and_then(|()| fs::write(&marker, format!("{}\n", self.cfg.node_id)))
            .map_err(io_error(format!(
                "registering the node at {}",
                marker.display()
            )))?;
        let mut report = ReconcileReport::default();
        let (staged, bad) = records::load_all::<StagedRecord>(&layout.staged_dir())
            .map_err(io_error("reading the staged records"))?;
        report.unreadable.extend(
            bad.into_iter()
                .map(|(p, e)| format!("{}: {e}", p.display())),
        );
        for rec in staged {
            let _lock = self.lock_volume(&rec.handle).await;
            if self.children().services.contains_key(&rec.handle) {
                continue;
            }
            match self.start_service(&rec.handle, None) {
                Ok(()) => {
                    report.services += 1;
                    eprintln!("reconcile: started the history service for {}", rec.handle);
                }
                Err(e) => eprintln!("reconcile: starting the service for {}: {e}", rec.handle),
            }
        }
        let mut published = Vec::new();
        if let Ok(dirs) = fs::read_dir(layout.published_root()) {
            for dir in dirs.flatten() {
                let (found, bad) = records::load_all::<PublishedRecord>(&dir.path())
                    .map_err(io_error("reading the published records"))?;
                published.extend(found);
                report.unreadable.extend(
                    bad.into_iter()
                        .map(|(p, e)| format!("{}: {e}", p.display())),
                );
            }
        }
        for rec in published {
            let key = target_key(&rec.target);
            let path = layout.published_record(&rec.handle, &key);
            let staged = self.children().services.contains_key(&rec.handle);
            let state = if exists(&rec.target) {
                self.mount_state(&rec.target).await.ok()
            } else {
                None
            };
            match state {
                Some(MountState::Stale { .. } | MountState::Live { .. }) if staged => {
                    report.recovering += 1;
                    eprintln!(
                        "reconcile: recovering the front for {} at {}",
                        who(&rec),
                        rec.target.display()
                    );
                    let this = Arc::clone(self);
                    tokio::spawn(async move {
                        let lock = this.lock_volume(&rec.handle).await;
                        if this.children().fronts.contains_key(&rec.target) {
                            return;
                        }
                        this.recover_front(lock, rec, None).await;
                    });
                }
                _ => {
                    // Gone, or no longer mounted (a reboot): the kubelet
                    // publishes it again if the pod still wants it.
                    report.dropped += 1;
                    let _ = records::remove(&path);
                }
            }
        }
        Ok(report)
    }

    /// Stops supervising without stopping the children: no restarts
    /// follow. They die with the plugin container; tests use this to stand
    /// in for a plugin that died.
    pub fn forget(&self) {
        self.shutting_down.store(true, Ordering::Relaxed);
        let mut children = self.children();
        children.fronts.clear();
        children.services.clear();
    }

    /// Plugin shutdown: stops the fronts (their final sweep), then the
    /// services (their final commit). Records are kept for the next
    /// plugin's reconcile.
    pub async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Relaxed);
        let fronts: Vec<Spawned> = self
            .children()
            .fronts
            .values_mut()
            .map(|f| {
                f.child.stopping = true;
                f.child.spawned.clone()
            })
            .collect();
        let grace = self.cfg.unpublish_grace;
        futures_join(fronts.iter().map(|s| self.stop_child(s, grace, "front"))).await;
        let services: Vec<Spawned> = self
            .children()
            .services
            .values_mut()
            .map(|s| {
                s.child.stopping = true;
                s.child.spawned.clone()
            })
            .collect();
        let grace = self.cfg.unstage_grace;
        futures_join(
            services
                .iter()
                .map(|s| self.stop_child(s, grace, "service")),
        )
        .await;
        let mut children = self.children();
        children.fronts.clear();
        children.services.clear();
    }
}

enum FrontWait {
    Live,
    Exited(Exit, String),
    Gone,
    Timeout,
}

/// Runs the futures concurrently.
async fn futures_join<F: std::future::Future<Output = ()>>(futures: impl Iterator<Item = F>) {
    let mut set = Vec::new();
    for f in futures {
        set.push(Box::pin(f));
    }
    for f in set {
        f.await;
    }
}

fn address_file(layout: &Layout, handle: &str) -> PathBuf {
    StateDir::new(layout.state(handle))
        .history_dir()
        .join("address")
}

/// The slot `pid` leases, from the lease holders' lines.
fn slot_of(state: &StateDir, pid: u32) -> Option<u16> {
    let prefix = format!("pid={pid} ");
    FrontLease::holders(state)
        .ok()?
        .into_iter()
        .find(|(_, line)| line.starts_with(&prefix))
        .map(|(slot, _)| slot)
}

fn describe(exit: Option<Exit>) -> String {
    exit.map_or_else(|| "unknown status".to_owned(), |e| e.to_string())
}

fn who(record: &PublishedRecord) -> String {
    match &record.pod {
        Some(p) => format!("{}/{}", p.namespace, p.name),
        None => format!("{} {}", record.handle, target_key(&record.target)),
    }
}

/// Stage's blocking part: the volume (created from a static PV's context
/// if needed), its state area, rules and staged record.
fn prepare(
    layout: &Layout,
    handle: &str,
    context: &HashMap<String, String>,
) -> Result<VolumeParams, Error> {
    let record = match volume::load(layout, handle)? {
        Some(record) => record,
        None if context.contains_key("profile") => {
            let params = VolumeParams::from_context(handle, context)
                .map_err(|e| Error::InvalidArgument(e.to_string()))?;
            volume::create(layout, handle, &params, 0, true)?
        }
        None => {
            return Err(Error::NotFound(format!(
                "volume {handle} does not exist under {}",
                layout.base.display()
            )));
        }
    };
    let params = record.params;
    let config = volume::profile_config(layout, &params.profile)?;
    let state = StateDir::new(layout.state(handle));
    {
        let _lock = volume::lock(&state, "agentfs-csi stage")?;
        core_volume::init_with(
            &state,
            &params.volume,
            params.branch.as_deref(),
            Some(&[config]),
        )
        .map_err(|e| {
            Error::FailedPrecondition(format!(
                "checking {}: {}",
                state.root().display(),
                volume::chain(&e)
            ))
        })?;
    }
    volume::install_rules(layout, handle, &params.profile)?;
    let staged = layout.staged_record(handle);
    records::store(
        &staged,
        &StagedRecord {
            handle: handle.to_owned(),
            params: params.clone(),
        },
    )
    .map_err(io_error(format!("writing {}", staged.display())))?;
    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_the_cap_and_resets_after_uptime() {
        let cfg = Backoff::default();
        let short = Duration::from_secs(1);
        let mut d = next_backoff(None, short, &cfg);
        let mut seen = vec![d];
        for _ in 0..6 {
            d = next_backoff(Some(d), short, &cfg);
            seen.push(d);
        }
        let secs: Vec<u64> = seen.iter().map(Duration::as_secs).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(
            next_backoff(Some(Duration::from_secs(30)), Duration::from_secs(61), &cfg),
            Duration::from_secs(1)
        );
    }
}
