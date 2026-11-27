//! The node plugin's supervisor (docs/csi.md): one `agentfs host` per volume
//! in use on this node, each pod's view opened on it at the kubelet's
//! target, and the hosts' recovery.
//!
//! - **Ensure** runs `agentfs init` for a claim's volume (Volume's ensure:
//!   it creates the tree volume or finds it, and makes the profile's config
//!   its include), then installs the profile's rule files.
//! - **Publish** allows one pod per claim: a claim published at another
//!   target is refused. It starts the volume's host when none runs, then
//!   asks it to open a view at the target with the pod's context, and
//!   records the target in `<node-state>/published/<handle>.json`.
//! - **Unpublish** asks the host to close the view (or detaches the target
//!   when no host runs), removes the record and the target directory, and
//!   stops the host once the volume is idle.
//! - **Starting a host** installs the profile's rule files (rules are read
//!   when a volume opens), spawns the host and reads its hello (another
//!   protocol version is refused, and never retried), then opens every
//!   recorded view of the volume whose target is still there (stacked over
//!   a stale mount a crashed host left) and asks the host to reconcile,
//!   which ends what no view uses any more. Only then do other calls see
//!   the host.
//! - **Stopping a host** closes its control channel, whatever clones of its
//!   client other tasks hold: the host sees EOF. With no view open (every
//!   stop the supervisor asks for) it drains and exits 0 once Volume has
//!   released the volume; with views open (the plugin's shutdown) it ends
//!   its git children, exits 3 and leaves the views stale for the next host.
//! - **A host that exits unasked** is started again after a backoff while
//!   the volume is not idle, with every view replayed: no event says a
//!   crashed host would start now, and the backoff throttles a crash loop.
//!   A start that exits 75 (another process holds the volume's lease, such
//!   as a killed host's git children until they die) is tried again once
//!   the lease is free: the supervisor waits on the lease, not a timer. A
//!   start that fails otherwise is tried again with backoff. These delays
//!   are the only timers, beside the launcher's bounded wait for a child's
//!   last log lines.
//! - **Release** (DeleteVolume) starts the volume's host with no view and
//!   stops it: its exit 0 is Volume's release verdict.
//! - **Forks** (a fork claim's CreateVolume and DeleteVolume) are created
//!   and ended by key on the source volume's host, started for the call
//!   when none runs and stopped after it when the volume is idle. A fork
//!   claim's view is published on that same host.
//! - **Reconcile**, at plugin start, drops the records whose target is
//!   gone, starts a host per volume that still has views, and names the
//!   recording-format volumes it never serves.
//!
//! Locks, each named for what it orders:
//!
//! - **A claim's lock** is held by its publish or unpublish, from the
//!   one-pod check to the record's write. A second NodePublish or
//!   NodeUnpublish of a claim whose call still runs is answered `ABORTED`
//!   at once (the CSI specification's "operation pending for volume"), and
//!   the kubelet retries; calls of other claims go on. Each runs as a task
//!   of its own to the end, so a kubelet that gives up drops only its wait.
//! - **A volume's start lock** orders its host's start and stop, the
//!   replay, every write of the volume's records and every detach of its
//!   targets. It is held across a reply only while a host starts (replay
//!   and reconcile, on a host no other call sees yet), while it stops, and
//!   for a fork's create or end. An open or close of a running host's view
//!   is never under it, so one close that waits on a held file stalls only
//!   its own claim.
//! - **`using`**, a short lock never held across an await on a host,
//!   holds each volume's claims whose open is in flight. A volume is idle
//!   when none is and it has no record; deciding that and taking the host
//!   out of service are one step under it, as are joining it and taking
//!   the host's client, so a publish never gets a host being stopped.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use agentfs_contracts::host::{Refusal, RefusalKind, Reply, Request, TreeRef, ViewRequest};
use agentfs_contracts::name::{ClientLabel, ForkKey};
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{Found, StateArea as _};
use agentfs_contracts::tree::TreeId;
use agentfs_contracts::trees::ForkSettings;
use agentfs_engine::operator_settings::{RuleFiles, install_rules};
use agentfs_engine::state_area::TreeStateArea;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, oneshot, watch};

use crate::error::Error;
use crate::host_client::{HostClient, HostGone, StartError};
use crate::launcher::{self, Exit, Launcher, PluginIdentity, RecentLog, Spawned};
use crate::layout::{Layout, target_key, validate_handle};
use crate::mounter::{Mounter, detach_agentfs};
use crate::params::VolumeParams;
use crate::records::{self, PublishedRecord, RECORD_VERSION};
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
    /// The plugin pod's own identity, given to its children.
    pub plugin: PluginIdentity,
    pub backoff: Backoff,
}

impl NodeConfig {
    /// The defaults of `agentfs-csi`.
    pub fn new(node_id: impl Into<String>, layout: Layout) -> Self {
        Self {
            node_id: node_id.into(),
            layout,
            plugin: PluginIdentity::default(),
            backoff: Backoff::default(),
        }
    }
}

/// One pod's view, as NodePublish asks for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewSpec {
    /// The claim.
    pub handle: String,
    /// The volume directory whose host serves the view.
    pub volume: String,
    pub tree: TreeRef,
    pub target: PathBuf,
    pub read_only: bool,
    pub pod: PodContext,
}

/// What [`Supervisor::reconcile`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Hosts started.
    pub hosts: usize,
    /// Views opened again.
    pub views: usize,
    /// Records dropped: their target was gone, or the host refused them.
    pub dropped: usize,
    /// Names under the published directory that are no readable record.
    pub unreadable: Vec<String>,
}

/// A running host. Its `client` is the one owner of the control channel:
/// closing it stops the host.
struct Host {
    client: HostClient,
    exited: watch::Receiver<Option<Exit>>,
    recent_log: RecentLog,
    generation: u64,
    started: Instant,
    /// The delay before this host was started, if it is a restart.
    backoff: Option<Duration>,
}

/// A host just started and in service.
struct Started {
    views: usize,
    dropped: usize,
}

/// What a host that did not start waits for before its next start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Next {
    /// Nothing: trying again cannot help (another protocol version).
    Never,
    /// The volume's lease free: the start exited 75, so another process held it.
    Lease,
    /// A backoff delay: a failure no event ends (a crash, a full disk, PID pressure).
    Backoff,
}

impl Next {
    /// What a log line of a failed start says comes next, the backoff
    /// being `delay`.
    fn then(self, delay: Duration) -> String {
        match self {
            Self::Never => String::new(),
            Self::Lease => "; trying again once its state area's lease is free".to_owned(),
            Self::Backoff => format!("; trying again in {delay:?}"),
        }
    }
}

/// A host that did not start; nothing of it runs.
struct NotStarted {
    error: Error,
    next: Next,
}

impl From<Error> for NotStarted {
    fn from(error: Error) -> Self {
        Self {
            error,
            next: Next::Backoff,
        }
    }
}

impl From<NotStarted> for Error {
    fn from(not: NotStarted) -> Self {
        not.error
    }
}

/// Each volume's claims whose open is in flight.
type Using = HashMap<String, BTreeSet<String>>;

/// One async lock per name, made on first use.
type Locks = Mutex<HashMap<String, Arc<AsyncMutex<()>>>>;

pub struct Supervisor {
    cfg: NodeConfig,
    launcher: Arc<dyn Launcher>,
    mounter: Arc<dyn Mounter>,
    /// The hosts in service, by volume. An entry removed is a host asked
    /// to stop: its exit is no crash.
    hosts: Mutex<HashMap<String, Host>>,
    /// Each claim's lock, by handle.
    claims: Locks,
    /// Each volume's start lock.
    starts: Locks,
    using: Mutex<Using>,
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

/// Whether something is at `path`. Only `NotFound` means absent: a stale
/// FUSE mount answers `stat` with `ENOTCONN`, and it is very much there.
fn exists(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(e) => e.kind() != io::ErrorKind::NotFound,
    }
}

/// The child's exit, once it has one.
async fn wait_exit(exited: &mut watch::Receiver<Option<Exit>>) -> Exit {
    let waited = exited.wait_for(Option::is_some).await.ok().and_then(|e| *e);
    waited.or(*exited.borrow()).unwrap_or(Exit::Code(-1))
}

/// Returns once no process holds the volume's lease at `state`:
/// `TreeStateArea::wait_for_lease` on a std::thread of its own, answered over
/// a tokio oneshot: never `spawn_blocking`, whose pool the runtime's shutdown
/// waits for. A wait that never ends keeps its thread.
async fn lease_free(state: PathBuf) -> io::Result<()> {
    let (tx, rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("lease-wait".into())
        .spawn(move || {
            // The receiver gone: the restart was dropped, nothing waits.
            let _ = tx.send(TreeStateArea::at(&state).wait_for_lease());
        })?;
    rx.await
        .map_err(|_| io::Error::other("the lease wait ended without an answer"))?
}

/// The error a host's refusal is to the CO.
fn refused(refusal: Refusal) -> Error {
    let Refusal { kind, message } = refusal;
    match kind {
        RefusalKind::ForkTaken
        | RefusalKind::KeyUnknown
        | RefusalKind::KeyEnded
        | RefusalKind::Stale => Error::FailedPrecondition(message),
        RefusalKind::Occupied => Error::AlreadyExists(message),
        RefusalKind::MountFailed => Error::Internal(message),
        RefusalKind::Draining | RefusalKind::Stopped => Error::Unavailable(message),
        RefusalKind::NestedNotBuilt | RefusalKind::ApprovalNotBuilt => {
            Error::InvalidArgument(message)
        }
    }
}

/// The error a host's refusal of a view's open is to the CO: a fork
/// claim's fork that has had its writer or has ended is named by its claim.
fn open_refused(view: &ViewSpec, refusal: Refusal) -> Error {
    let TreeRef::Fork(_) = view.tree else {
        return refused(refusal);
    };
    let handle = &view.handle;
    match refusal.kind {
        RefusalKind::ForkTaken => Error::FailedPrecondition(format!(
            "the fork of claim {handle} has had its writer: a fork claim serves one pod \
             (docs/csi.md)"
        )),
        RefusalKind::KeyUnknown | RefusalKind::KeyEnded => {
            Error::FailedPrecondition(format!("the fork of claim {handle} has ended"))
        }
        _ => refused(refusal),
    }
}

/// A profile file, when the operator gave one.
fn optional(path: PathBuf) -> Option<PathBuf> {
    path.exists().then_some(path)
}

/// The lock of `name` in `locks`, made on first use.
fn lock_of(locks: &Locks, name: &str) -> Arc<AsyncMutex<()>> {
    locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(name.to_owned())
        .or_default()
        .clone()
}

impl Supervisor {
    pub fn new(
        config: NodeConfig,
        launcher: Arc<dyn Launcher>,
        mounter: Arc<dyn Mounter>,
    ) -> Arc<Self> {
        Arc::new(Self {
            cfg: config,
            launcher,
            mounter,
            hosts: Mutex::new(HashMap::new()),
            claims: Mutex::new(HashMap::new()),
            starts: Mutex::new(HashMap::new()),
            using: Mutex::new(HashMap::new()),
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

    fn hosts(&self) -> MutexGuard<'_, HashMap<String, Host>> {
        // A panic while holding it leaves a plain map; keep going.
        self.hosts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The claim's lock, for its publish or unpublish: `ABORTED` while
    /// another call of the claim holds it.
    fn lock_claim(&self, handle: &str) -> Result<OwnedMutexGuard<()>, Error> {
        lock_of(&self.claims, handle).try_lock_owned().map_err(|_| {
            Error::Aborted(format!(
                "a publish or unpublish of claim {handle} is still running on this node; it is \
                 finished first"
            ))
        })
    }

    /// The volume's start lock.
    async fn lock_start(&self, volume: &str) -> OwnedMutexGuard<()> {
        lock_of(&self.starts, volume).lock_owned().await
    }

    /// Each volume's claims whose open is in flight. Taken before `hosts`
    /// when both are, and never held across an await.
    fn using(&self) -> MutexGuard<'_, Using> {
        self.using
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether `volume` is idle: no open of it in flight and no record of
    /// it. Read under `using` (the caller's guard).
    fn idle(&self, using: &Using, volume: &str) -> bool {
        using.get(volume).is_none_or(BTreeSet::is_empty) && self.records_of(volume).is_empty()
    }

    /// Joins `using` with the claim's open of `volume`, and takes the
    /// client of the host in service, if one is: one step, so the host is
    /// not stopped while the open is in flight.
    fn join(&self, volume: &str, handle: &str) -> Option<(HostClient, u64)> {
        let mut using = self.using();
        using
            .entry(volume.to_owned())
            .or_default()
            .insert(handle.to_owned());
        self.hosts()
            .get(volume)
            .map(|h| (h.client.clone(), h.generation))
    }

    /// The claim's open of `volume` is no longer in flight.
    fn leave(&self, volume: &str, handle: &str) {
        let mut using = self.using();
        if let Some(claims) = using.get_mut(volume) {
            claims.remove(handle);
            if claims.is_empty() {
                using.remove(volume);
            }
        }
    }

    fn shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Relaxed)
    }

    /// The client and generation of the host in service.
    fn host_of(&self, volume: &str) -> Option<(HostClient, u64)> {
        self.hosts()
            .get(volume)
            .map(|h| (h.client.clone(), h.generation))
    }

    fn recent(&self, volume: &str) -> String {
        self.hosts()
            .get(volume)
            .map(|h| launcher::recent(&h.recent_log))
            .unwrap_or_default()
    }

    /// The published records of `volume`.
    fn records_of(&self, volume: &str) -> Vec<PublishedRecord> {
        records::load_published(self.layout())
            .0
            .into_iter()
            .filter(|r| r.volume == volume)
            .collect()
    }

    /// What a host gone mid-call is to the CO.
    fn gone(&self, volume: &str) -> Error {
        Error::Unavailable(format!(
            "the host of volume {volume} is gone; it is started again:\n{}",
            self.recent(volume)
        ))
    }

    /// Detaches every agentfs mount at `path`, on the blocking pool:
    /// `statfs` on a hung FUSE mount can block.
    async fn detach(&self, path: &Path) -> Result<(), Error> {
        let mounter = self.mounter.clone();
        let p = path.to_path_buf();
        tokio::task::spawn_blocking(move || detach_agentfs(mounter.as_ref(), &p))
            .await
            .map_err(io::Error::other)
            .and_then(|r| r)
            .map(|_| ())
            .map_err(|e| Error::io(format!("detaching {}", path.display()), e))
    }

    // ----- volumes --------------------------------------------------------

    /// `agentfs init` for the claim's volume, then install_rules from the
    /// profile. Under the volume's start lock.
    pub async fn ensure(&self, handle: &str, params: &VolumeParams) -> Result<(), Error> {
        let _start = self.lock_start(handle).await;
        let layout = self.layout();
        volume::profile_config(layout, &params.profile)?;
        if self.hosts().contains_key(handle) {
            return Err(Error::Unavailable(format!(
                "volume {handle} is served on this node: its settings change only while it is not"
            )));
        }
        for dir in [&layout.base, &layout.volume_dir(handle)] {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
        }
        let spec = launcher::init_spec(layout, handle, params, &self.cfg.plugin);
        let Spawned {
            mut exited,
            recent_log,
            ..
        } = self
            .launcher
            .spawn(spec)
            .map_err(|e| Error::io("starting agentfs init", e))?;
        match wait_exit(&mut exited).await {
            Exit::Code(0) => {}
            Exit::Code(75) => {
                return Err(Error::Unavailable(format!(
                    "volume {handle} is busy: another process holds its state area"
                )));
            }
            exit => {
                return Err(Error::FailedPrecondition(format!(
                    "agentfs init of volume {handle} failed ({exit}):\n{}",
                    launcher::recent(&recent_log)
                )));
            }
        }
        self.install_rules(handle, &params.profile)
    }

    /// The profile's rule files into the volume's state area; an absent one
    /// removes the installed copy.
    fn install_rules(&self, volume: &str, profile: &str) -> Result<(), Error> {
        let layout = self.layout();
        let state = layout.state(volume);
        let exclude = optional(layout.profile_file(profile, "exclude"));
        let attributes = optional(layout.profile_file(profile, "attributes"));
        install_rules(
            &state,
            RuleFiles {
                exclude: exclude.as_deref(),
                attributes: attributes.as_deref(),
            },
        )
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => Error::FailedPrecondition(format!(
                "volume {volume}: installing the rules of profile {profile:?}: {e}"
            )),
            _ => Error::io(
                format!("volume {volume}: installing the rules of profile {profile:?}"),
                e,
            ),
        })
    }

    // ----- hosts ----------------------------------------------------------

    /// Starts the volume's host: its rules installed, its hello read, then
    /// (`replay`) every recorded view of the volume opened again, then a
    /// reconcile; only then is it in service (in `hosts`, watched). A host
    /// that fails before is stopped, its exit awaited. The caller holds the
    /// volume's start lock and has seen no host in service.
    async fn start_host(
        self: &Arc<Self>,
        volume: &str,
        replay: bool,
        backoff: Option<Duration>,
    ) -> Result<Started, NotStarted> {
        let layout = self.layout().clone();
        let Some((name, profile)) = records::load_volume_claim(&layout, volume)? else {
            return Err(Error::NotFound(format!(
                "no claim of volume {volume} under {}",
                layout.base.display()
            ))
            .into());
        };
        self.install_rules(volume, &profile)?;
        let spec = launcher::host_spec(&layout, volume, &name, &self.cfg.plugin);
        let Spawned {
            mut exited,
            recent_log,
            control,
            ..
        } = self
            .launcher
            .spawn(spec)
            .map_err(|e| Error::io("starting agentfs host", e))?;
        let Some(control) = control else {
            return Err(
                Error::Internal("the launcher gave the host no control channel".into()).into(),
            );
        };
        let (client, hello) = match HostClient::start(control).await {
            Ok(started) => started,
            Err(StartError::Version { host, plugin }) => {
                // Its stream is closed: with no view it drains.
                wait_exit(&mut exited).await;
                return Err(NotStarted {
                    error: Error::FailedPrecondition(format!(
                        "the host of volume {volume} speaks host protocol v{host}, this plugin \
                         v{plugin}: the node plugin's image and its agentfs binary must be of \
                         one release; not tried again"
                    )),
                    next: Next::Never,
                });
            }
            Err(StartError::Gone(HostGone)) => {
                let exit = wait_exit(&mut exited).await;
                let log = launcher::recent(&recent_log);
                return Err(match exit {
                    Exit::Code(75) => NotStarted {
                        error: Error::Unavailable(format!(
                            "volume {volume} is busy: another process holds its state area"
                        )),
                        next: Next::Lease,
                    },
                    Exit::Code(1 | 2) => Error::FailedPrecondition(format!(
                        "the host of volume {volume} refused to start ({exit}):\n{log}"
                    ))
                    .into(),
                    _ => Error::Unavailable(format!(
                        "the host of volume {volume} exited before it served ({exit}):\n{log}"
                    ))
                    .into(),
                });
            }
        };
        let served = if hello.volume == name {
            self.serve(volume, &client, replay, &recent_log).await
        } else {
            Err(Error::Internal(format!(
                "the host of volume {volume} serves agentfs volume {}, not {name}",
                hello.volume
            )))
        };
        let (views, dropped) = match served {
            Ok(counts) => counts,
            Err(error) => {
                client.close();
                wait_exit(&mut exited).await;
                return Err(error.into());
            }
        };
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        self.hosts().insert(
            volume.to_owned(),
            Host {
                client,
                exited: exited.clone(),
                recent_log,
                generation,
                started: Instant::now(),
                backoff,
            },
        );
        self.watch(volume.to_owned(), generation, exited);
        Ok(Started { views, dropped })
    }

    /// A new host's first calls: (`replay`) the recorded views, then the
    /// reconcile.
    async fn serve(
        &self,
        volume: &str,
        client: &HostClient,
        replay: bool,
        recent_log: &RecentLog,
    ) -> Result<(usize, usize), Error> {
        let gone = || {
            Error::Unavailable(format!(
                "the host of volume {volume} is gone; it is started again:\n{}",
                launcher::recent(recent_log)
            ))
        };
        let counts = if replay {
            self.replay(volume, client).await.map_err(|e| match e {
                Replay::Gone => gone(),
                Replay::Record(e) => e,
            })?
        } else {
            (0, 0)
        };
        match client.call(Request::Reconcile).await {
            Ok(Reply::Reconciled { ended }) if ended > 0 => {
                eprintln!("volume {volume}: {ended} client(s) no view used were ended");
            }
            Ok(Reply::Reconciled { .. }) => {}
            Ok(other) => {
                return Err(Error::Internal(format!(
                    "the host of volume {volume} answered a reconcile with {other:?}"
                )));
            }
            Err(HostGone) => return Err(gone()),
        }
        Ok(counts)
    }

    /// Opens every recorded view of `volume` again: its target still there
    /// (a stale mount of it included), else its record dropped. A view the
    /// host refuses is logged and its record dropped. The caller holds the
    /// volume's start lock.
    async fn replay(&self, volume: &str, client: &HostClient) -> Result<(usize, usize), Replay> {
        let mut views = 0;
        let mut dropped = 0;
        for record in self.records_of(volume) {
            let why = if exists(&record.target) {
                match client.call(Request::Open(view_request(&record))).await {
                    Ok(Reply::Opened { .. }) => {
                        views += 1;
                        continue;
                    }
                    Ok(Reply::Refused(refusal)) => refusal.message,
                    Ok(other) => format!("the host answered {other:?}"),
                    Err(HostGone) => return Err(Replay::Gone),
                }
            } else {
                "its target is gone".to_owned()
            };
            eprintln!(
                "volume {volume}: dropped the view of {} at {}: {why}",
                record.handle,
                record.target.display()
            );
            records::remove_published(self.layout(), &record.handle).map_err(Replay::Record)?;
            dropped += 1;
        }
        Ok((views, dropped))
    }

    /// Watches a host's exit: an exit nobody asked for is a crash.
    fn watch(
        self: &Arc<Self>,
        volume: String,
        generation: u64,
        mut exited: watch::Receiver<Option<Exit>>,
    ) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let exit = wait_exit(&mut exited).await;
            this.exited(volume, generation, exit).await;
        });
    }

    async fn exited(self: Arc<Self>, volume: String, generation: u64, exit: Exit) {
        let delay = {
            let _start = self.lock_start(&volume).await;
            let host = {
                let mut hosts = self.hosts();
                if hosts
                    .get(&volume)
                    .is_none_or(|h| h.generation != generation)
                {
                    // Asked to stop, or replaced.
                    return;
                }
                hosts.remove(&volume)
            };
            let Some(host) = host else { return };
            if self.shutting_down() {
                return;
            }
            let delay = next_backoff(host.backoff, host.started.elapsed(), &self.cfg.backoff);
            eprintln!("[host {volume}] exited: {exit}; restarting in {delay:?}");
            delay
        };
        self.restart(volume, Next::Backoff, delay).await;
    }

    /// Starts the volume's host again once `next` allows (after `delay`, or
    /// once its lease is free) while the volume is not idle, and keeps
    /// trying while that fails, as each failed start says, unless trying
    /// again cannot help. The lease is awaited outside the start lock: an
    /// unpublish never waits on another process.
    async fn restart(self: Arc<Self>, volume: String, mut next: Next, mut delay: Duration) {
        loop {
            match next {
                Next::Never => return,
                Next::Backoff => tokio::time::sleep(delay).await,
                Next::Lease => {
                    if let Err(e) = lease_free(self.layout().state(&volume)).await {
                        eprintln!(
                            "volume {volume}: waiting for its state area's lease: {e}; trying \
                             again in {delay:?}"
                        );
                        tokio::time::sleep(delay).await;
                    }
                }
            }
            let _start = self.lock_start(&volume).await;
            let idle = {
                let using = self.using();
                self.idle(&using, &volume)
            };
            if self.shutting_down() || self.hosts().contains_key(&volume) || idle {
                return;
            }
            match self.start_host(&volume, true, Some(delay)).await {
                Ok(_) => {
                    self.stop_if_idle(&volume).await;
                    return;
                }
                Err(not) => {
                    next = not.next;
                    if next == Next::Backoff {
                        delay = next_backoff(Some(delay), Duration::ZERO, &self.cfg.backoff);
                    }
                    eprintln!(
                        "starting the host of volume {volume} again: {}{}",
                        not.error,
                        next.then(delay)
                    );
                }
            }
        }
    }

    /// Stops a host out of service: its channel closed (EOF: with no view
    /// open it drains), its exit awaited.
    async fn stop(host: Host) -> (Exit, RecentLog) {
        let Host {
            client,
            mut exited,
            recent_log,
            ..
        } = host;
        client.close();
        (wait_exit(&mut exited).await, recent_log)
    }

    /// Stops the volume's host. The caller holds the volume's start lock.
    async fn stop_host(&self, volume: &str) -> Option<(Exit, RecentLog)> {
        let host = self.hosts().remove(volume)?;
        Some(Self::stop(host).await)
    }

    /// Stops the volume's host when the volume is idle: the decision and
    /// taking the host out of service are one step under `using`. The
    /// caller holds the volume's start lock.
    async fn stop_if_idle(&self, volume: &str) {
        let host = {
            let using = self.using();
            if !self.idle(&using, volume) {
                return;
            }
            self.hosts().remove(volume)
        };
        if let Some(host) = host {
            Self::stop(host).await;
        }
    }

    // ----- views ----------------------------------------------------------

    /// NodePublish: one pod per claim; the view opened at the target on the
    /// volume's host, started when none runs. `ABORTED` while another call
    /// of the claim runs. Runs to the end as a task of its own: a caller
    /// that gives up drops only its wait.
    pub async fn publish(self: &Arc<Self>, view: ViewSpec) -> Result<(), Error> {
        let claim = self.lock_claim(&view.handle)?;
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let _claim = claim;
            this.publish_claimed(&view).await
        })
        .await
        .map_err(|e| Error::Internal(format!("publishing: {e}")))?
    }

    /// Publish, under the claim's lock.
    async fn publish_claimed(self: &Arc<Self>, view: &ViewSpec) -> Result<(), Error> {
        let handle = view.handle.as_str();
        let published = records::load_published(self.layout())
            .0
            .into_iter()
            .find(|r| r.handle == handle);
        if let Some(record) = published {
            if record.target != view.target {
                return Err(Error::FailedPrecondition(format!(
                    "volume {handle} is published at {} on this node: one pod per volume; \
                     several pods on one volume are not built (docs/csi.md)",
                    record.target.display()
                )));
            }
            if record.read_only == view.read_only {
                return Ok(());
            }
            return Err(Error::AlreadyExists(format!(
                "volume {handle} is published at {} with read_only={}, not read_only={}",
                record.target.display(),
                record.read_only,
                view.read_only
            )));
        }
        let result = self.open(view).await;
        if result.is_err() {
            let _start = self.lock_start(&view.volume).await;
            self.leave(&view.volume, handle);
            self.stop_if_idle(&view.volume).await;
        }
        result
    }

    /// The host in service for the claim's open, which joins `using`; one
    /// is started when none is.
    async fn attach(self: &Arc<Self>, view: &ViewSpec) -> Result<(HostClient, u64), Error> {
        loop {
            if let Some(host) = self.join(&view.volume, &view.handle) {
                return Ok(host);
            }
            let _start = self.lock_start(&view.volume).await;
            if !self.hosts().contains_key(&view.volume) {
                self.start_host(&view.volume, true, None).await?;
            }
        }
    }

    /// Opens the view and records it; the claim leaves `using` once its
    /// record is written. On an error the caller leaves.
    async fn open(self: &Arc<Self>, view: &ViewSpec) -> Result<(), Error> {
        let (client, generation) = self.attach(view).await?;
        let target = &view.target;
        fs::create_dir_all(target).or_else(|e| {
            // A stale mount there: `mkdir` fails with EEXIST, and
            // create_dir_all then cannot stat it (ENOTCONN, or ECONNABORTED
            // while the connection is being aborted).
            if e.kind() == io::ErrorKind::AlreadyExists
                || agentfs_fuse::stale::is_disconnected_io(&e)
            {
                Ok(())
            } else {
                Err(Error::io(format!("creating {}", target.display()), e))
            }
        })?;
        let label = ClientLabel::parse(&format!("csi-{}", target_key(target)))
            .map_err(|e| Error::Internal(e.to_string()))?;
        let record = PublishedRecord {
            v: RECORD_VERSION,
            handle: view.handle.clone(),
            volume: view.volume.clone(),
            tree: view.tree.clone(),
            target: target.clone(),
            read_only: view.read_only,
            pod: view.pod.clone(),
            label,
        };
        match client.call(Request::Open(view_request(&record))).await {
            Ok(Reply::Opened { stacked, .. }) => {
                self.record(&record, &client, generation).await?;
                eprintln!(
                    "published volume {} at {}{}",
                    view.handle,
                    target.display(),
                    if stacked { ", over a stale mount" } else { "" }
                );
                Ok(())
            }
            Ok(Reply::Refused(refusal)) => Err(open_refused(view, refusal)),
            Ok(other) => Err(Error::Internal(format!(
                "the host of volume {} answered an open with {other:?}",
                view.volume
            ))),
            Err(HostGone) => Err(self.gone(&view.volume)),
        }
    }

    /// Writes the record of a view the host of `generation` opened, under
    /// the volume's start lock, and leaves `using`. A host started since
    /// then replayed the records before this one was written: the view is
    /// opened on it too. Any later start replays the record. Should that
    /// host refuse it (its reconcile ended the client of the open in
    /// flight, and a fork's writer outlives its client), the record goes
    /// and the publish fails with the refusal: no host serves the view, and
    /// a replay would be refused the same way.
    async fn record(
        &self,
        record: &PublishedRecord,
        client: &HostClient,
        generation: u64,
    ) -> Result<(), Error> {
        let current = {
            let start = self.lock_start(&record.volume).await;
            if let Err(e) = records::store_published(self.layout(), record) {
                drop(start);
                let _ = client
                    .call(Request::Close {
                        at: record.target.clone(),
                    })
                    .await;
                return Err(e);
            }
            self.leave(&record.volume, &record.handle);
            self.host_of(&record.volume)
        };
        if let Some((current, newer)) = current
            && newer != generation
        {
            match current.call(Request::Open(view_request(record))).await {
                Ok(Reply::Opened { .. }) => {}
                Ok(Reply::Refused(refusal)) => {
                    let start = self.lock_start(&record.volume).await;
                    let removed = records::remove_published(self.layout(), &record.handle);
                    drop(start);
                    removed?;
                    return Err(refused(refusal));
                }
                other => eprintln!(
                    "volume {}: opening the view of {} at {} on the restarted host: {other:?}; \
                     the next start replays it",
                    record.volume,
                    record.handle,
                    record.target.display()
                ),
            }
        }
        Ok(())
    }

    /// NodeUnpublish: the view closed (or the target detached when no host
    /// runs), the record and the target directory removed, and the host
    /// stopped once the volume is idle. `ABORTED` while another call of
    /// the claim runs. Runs to the end as a task of its own: a caller that
    /// gives up drops only its wait.
    pub async fn unpublish(self: &Arc<Self>, handle: &str, target: &Path) -> Result<(), Error> {
        let claim = self.lock_claim(handle)?;
        let this = Arc::clone(self);
        let (handle, target) = (handle.to_owned(), target.to_path_buf());
        tokio::spawn(async move {
            let _claim = claim;
            this.unpublish_claimed(&handle, &target).await
        })
        .await
        .map_err(|e| Error::Internal(format!("unpublishing: {e}")))?
    }

    /// Unpublish, under the claim's lock. The close is asked of the host in
    /// service without the start lock; the record's removal, any detach
    /// and the stop are under it. A host started since the close was asked
    /// (its replay reopened the view) is asked again.
    async fn unpublish_claimed(&self, handle: &str, target: &Path) -> Result<(), Error> {
        let found = records::load_published(self.layout())
            .0
            .into_iter()
            .find(|r| r.handle == handle && r.target == target);
        let Some(record) = found else {
            // Nothing of ours is published there: done already, but an
            // agentfs mount left there goes, and so does the directory.
            let _start = self.lock_start(handle).await;
            self.detach(target).await?;
            return remove_target(target);
        };
        let volume = record.volume;
        loop {
            let tried = self.host_of(&volume);
            let closed = match &tried {
                Some((client, _)) => {
                    match client.call(Request::Close { at: target.into() }).await {
                        Ok(Reply::Closed) => true,
                        Ok(Reply::Refused(refusal)) => return Err(refused(refusal)),
                        Ok(other) => {
                            return Err(Error::Internal(format!(
                                "the host of volume {volume} answered a close with {other:?}"
                            )));
                        }
                        // The plugin stops: the view is left for the next.
                        Err(HostGone) if self.shutting_down() => return Err(self.gone(&volume)),
                        Err(HostGone) => false,
                    }
                }
                None => false,
            };
            let _start = self.lock_start(&volume).await;
            let now = self.hosts().get(&volume).map(|h| h.generation);
            if now.is_some() && now != tried.map(|(_, generation)| generation) {
                continue;
            }
            if !closed {
                self.detach(target).await?;
            }
            records::remove_published(self.layout(), handle)?;
            remove_target(target)?;
            eprintln!("unpublished volume {handle} at {}", target.display());
            self.stop_if_idle(&volume).await;
            return Ok(());
        }
    }

    /// The first published record using `volume`, if any.
    pub async fn in_use(&self, volume: &str) -> Option<PublishedRecord> {
        let _start = self.lock_start(volume).await;
        self.records_of(volume).into_iter().next()
    }

    /// The published record of the claim `handle`, whose view is on
    /// `volume`'s host, if any.
    pub async fn claim_in_use(&self, volume: &str, handle: &str) -> Option<PublishedRecord> {
        let _start = self.lock_start(volume).await;
        self.records_of(volume)
            .into_iter()
            .find(|r| r.handle == handle)
    }

    /// One call on the volume's host, started for it when none runs (its
    /// recorded views replayed) and stopped after it when the volume is
    /// idle. The whole call is under the volume's start lock.
    async fn call_on_host(
        self: &Arc<Self>,
        volume: &str,
        request: Request,
    ) -> Result<Reply, Error> {
        let _start = self.lock_start(volume).await;
        let client = match self.host_of(volume) {
            Some((client, _)) => Ok(client),
            None => match self.start_host(volume, true, None).await {
                Ok(_) => self
                    .host_of(volume)
                    .map(|(client, _)| client)
                    .ok_or_else(|| self.gone(volume)),
                Err(not) => Err(not.into()),
            },
        };
        let reply = match client {
            Ok(client) => client
                .call(request)
                .await
                .map_err(|HostGone| self.gone(volume)),
            Err(e) => Err(e),
        };
        self.stop_if_idle(volume).await;
        reply
    }

    /// On the volume's host, started for the call if none runs and stopped
    /// after it if the volume is idle: the open fork under `key`, or a new
    /// fork of `upstream` under it.
    pub async fn create_fork(
        self: &Arc<Self>,
        volume: &str,
        upstream: TreeRef,
        key: ForkKey,
        settings: ForkSettings,
    ) -> Result<TreeId, Error> {
        let request = Request::ForkCreate {
            upstream,
            key,
            settings,
        };
        match self.call_on_host(volume, request).await? {
            Reply::Forked { tree } => Ok(tree),
            Reply::Refused(Refusal {
                kind: RefusalKind::KeyEnded,
                message,
            }) => Err(Error::AlreadyExists(message)),
            Reply::Refused(refusal) => Err(refused(refusal)),
            other => Err(Error::Internal(format!(
                "the host of volume {volume} answered a fork create with {other:?}"
            ))),
        }
    }

    /// Ends the fork under `key` on the volume's host, started for the call
    /// if none runs and stopped after it if the volume is idle. Ended, or
    /// no fork under the key, are both Ok (idempotent DeleteVolume).
    pub async fn end_fork(self: &Arc<Self>, volume: &str, key: ForkKey) -> Result<(), Error> {
        match self.call_on_host(volume, Request::ForkEnd { key }).await? {
            Reply::Ended
            | Reply::Refused(Refusal {
                kind: RefusalKind::KeyUnknown | RefusalKind::KeyEnded,
                ..
            }) => Ok(()),
            Reply::Refused(refusal) => Err(refused(refusal)),
            other => Err(Error::Internal(format!(
                "the host of volume {volume} answered a fork end with {other:?}"
            ))),
        }
    }

    /// Starts the volume's host and stops it: Ok once it exits 0 (Volume
    /// released). Refused while a host serves the volume or it is not
    /// idle.
    pub async fn release(self: &Arc<Self>, volume: &str) -> Result<(), Error> {
        let _start = self.lock_start(volume).await;
        let busy = {
            let using = self.using();
            !self.idle(&using, volume)
        };
        if busy || self.hosts().contains_key(volume) {
            return Err(Error::FailedPrecondition(format!(
                "volume {volume} is in use on this node"
            )));
        }
        self.start_host(volume, false, None).await?;
        match self.stop_host(volume).await {
            Some((Exit::Code(0), _)) => Ok(()),
            Some((exit, log)) => Err(Error::Unavailable(format!(
                "the host of volume {volume} did not release it ({exit}):\n{}",
                launcher::recent(&log)
            ))),
            None => Err(Error::Internal(format!(
                "the host of volume {volume} was stopped meanwhile"
            ))),
        }
    }

    /// At plugin start: records whose target is gone dropped, one host per
    /// volume that still has views, unreadable records and recording-format
    /// volumes named and left alone.
    pub async fn reconcile(self: &Arc<Self>) -> ReconcileReport {
        let layout = self.layout().clone();
        let (found, unreadable) = records::load_published(&layout);
        for bad in &unreadable {
            eprintln!("node record {bad} is not this driver's: left as it is");
        }
        let mut report = ReconcileReport {
            unreadable,
            ..ReconcileReport::default()
        };
        let mut volumes = BTreeSet::new();
        for record in found {
            if exists(&record.target) {
                volumes.insert(record.volume);
                continue;
            }
            let _start = self.lock_start(&record.volume).await;
            eprintln!(
                "volume {}: dropped the view at {}: its target is gone",
                record.handle,
                record.target.display()
            );
            if let Err(e) = records::remove_published(&layout, &record.handle) {
                eprintln!("{e}");
            }
            report.dropped += 1;
        }
        for volume in volumes {
            let _start = self.lock_start(&volume).await;
            if self.hosts().contains_key(&volume) {
                continue;
            }
            match self.start_host(&volume, true, None).await {
                Ok(started) => {
                    report.hosts += 1;
                    report.views += started.views;
                    report.dropped += started.dropped;
                    self.stop_if_idle(&volume).await;
                }
                Err(NotStarted { error, next }) => {
                    let delay = self.cfg.backoff.initial;
                    eprintln!(
                        "starting the host of volume {volume}: {error}{}",
                        next.then(delay)
                    );
                    tokio::spawn(Arc::clone(self).restart(volume.clone(), next, delay));
                }
            }
        }
        for (handle, dir) in recording_volumes(&layout) {
            eprintln!(
                "recording-format volume {handle} at {} is not served by this driver: move it \
                 with docs/runbooks/moving.md",
                dir.display()
            );
        }
        report
    }

    /// Stops every host: each one's channel is closed, whatever clones the
    /// tasks still running hold (EOF: with views open it ends its git
    /// children and exits 3), and its exit awaited.
    pub async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Relaxed);
        let hosts: Vec<(String, Host)> = self.hosts().drain().collect();
        let mut exits = Vec::new();
        for (volume, host) in hosts {
            let Host { client, exited, .. } = host;
            client.close();
            exits.push((volume, exited));
        }
        for (volume, mut exited) in exits {
            let exit = wait_exit(&mut exited).await;
            eprintln!("[host {volume}] stopped: {exit}");
        }
    }
}

/// Why a replay stopped.
enum Replay {
    /// The host is gone.
    Gone,
    /// A record could not be removed.
    Record(Error),
}

/// The open request of a recorded view.
fn view_request(record: &PublishedRecord) -> ViewRequest {
    ViewRequest {
        tree: record.tree.clone(),
        at: record.target.clone(),
        label: record.label.clone(),
        read_only: record.read_only,
        pod: record.pod.clone(),
    }
}

/// Removes the target directory; a missing one is fine.
fn remove_target(target: &Path) -> Result<(), Error> {
    match fs::remove_dir(target) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(Error::io(format!("removing {}", target.display()), e))
        }
        _ => Ok(()),
    }
}

/// The volume directories under the base path whose state area holds
/// another format: the recording driver's.
fn recording_volumes(layout: &Layout) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(&layout.base) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let Some(handle) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if validate_handle(&handle).is_err() || !entry.path().is_dir() {
            continue;
        }
        if matches!(
            TreeStateArea::at(&layout.state(&handle)).inspect(),
            Ok(Found::Other)
        ) {
            out.push((handle, entry.path()));
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_its_cap_and_resets_after_a_long_run() {
        let cfg = Backoff::default();
        let first = next_backoff(None, Duration::ZERO, &cfg);
        assert_eq!(first, cfg.initial);
        let second = next_backoff(Some(first), Duration::ZERO, &cfg);
        assert_eq!(second, cfg.initial * 2);
        assert_eq!(
            next_backoff(Some(cfg.max), Duration::ZERO, &cfg),
            cfg.max,
            "capped"
        );
        assert_eq!(
            next_backoff(Some(cfg.max), cfg.reset_after, &cfg),
            cfg.initial,
            "a long run resets it"
        );
    }

    #[test]
    fn a_fork_claims_refused_open_names_its_claim() {
        let view = |tree| ViewSpec {
            handle: "f".into(),
            volume: "v".into(),
            tree,
            target: "/t".into(),
            read_only: false,
            pod: PodContext::default(),
        };
        let refusal = |kind| Refusal {
            kind,
            message: "m".into(),
        };
        let fork = view(TreeRef::Fork(ForkKey::parse("f").unwrap()));
        assert_eq!(
            open_refused(&fork, refusal(RefusalKind::ForkTaken)),
            Error::FailedPrecondition(
                "the fork of claim f has had its writer: a fork claim serves one pod \
                 (docs/csi.md)"
                    .into()
            )
        );
        for kind in [RefusalKind::KeyUnknown, RefusalKind::KeyEnded] {
            assert_eq!(
                open_refused(&fork, refusal(kind)),
                Error::FailedPrecondition("the fork of claim f has ended".into())
            );
        }
        assert_eq!(
            open_refused(&fork, refusal(RefusalKind::Draining)),
            Error::Unavailable("m".into())
        );
        assert_eq!(
            open_refused(&view(TreeRef::Root), refusal(RefusalKind::KeyEnded)),
            Error::FailedPrecondition("m".into())
        );
    }

    #[test]
    fn every_refusal_kind_has_its_code() {
        let code = |kind| {
            format!(
                "{:?}",
                refused(Refusal {
                    kind,
                    message: "m".into()
                })
            )
        };
        for (kind, want) in [
            (RefusalKind::ForkTaken, "FailedPrecondition"),
            (RefusalKind::KeyUnknown, "FailedPrecondition"),
            (RefusalKind::KeyEnded, "FailedPrecondition"),
            (RefusalKind::Stale, "FailedPrecondition"),
            (RefusalKind::Occupied, "AlreadyExists"),
            (RefusalKind::MountFailed, "Internal"),
            (RefusalKind::Draining, "Unavailable"),
            (RefusalKind::Stopped, "Unavailable"),
            (RefusalKind::NestedNotBuilt, "InvalidArgument"),
            (RefusalKind::ApprovalNotBuilt, "InvalidArgument"),
        ] {
            assert!(code(kind).starts_with(want), "{kind:?}: {}", code(kind));
        }
    }
}
