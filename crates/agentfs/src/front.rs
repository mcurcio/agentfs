//! A front of a history service: `front --history-service`, which only the
//! CSI node plugin starts (docs/csi.md), and `record --history-service`.
//!
//! A front takes no state-area lock. It leases a pending slot
//! ([`FrontLease`]), connects to the service (waiting up to
//! `--history-wait` for it), and records through a front recorder: its
//! own rules, checks and freeze, then a round trip to the service, which
//! journals and commits. A front mount's failed records wait in its slot's
//! repair file (`meta/fronts/slot-N.repairs`) for the next front in that
//! slot. A front serves no control socket: restores and reverts go to
//! the service, which writes the backing tree itself and calls each front
//! on its listening connection: to settle what is dirty, to
//! say what is open for writing, and to drop its kernel's caches of what
//! was written.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use clap::Args;

use agentfs_core::history::History;
use agentfs_core::history_service::{
    Address, ClientConfig, FrontHooks, FrontInfo, FrontLease, HistoryClient, ListenerHandle,
    NoHooks,
};
use agentfs_core::record::Origin;
use agentfs_core::volume::{self, Recorder, StateDir};
use agentfs_fuse::{Mount, MountConfig, MountKind, Passthrough, UnmountReport, preflight};

use crate::daemon::{Daemon, Wake};
use crate::{HistoryArgs, Pod, Session, parse_grace};

/// `agentfs front`'s flags: a FUSE mount of BACKING at MOUNTPOINT whose
/// records go to the history service.
#[derive(Args)]
pub struct FrontArgs {
    /// The backing tree: the real directory whose files the mount shows.
    pub backing: PathBuf,
    /// Where to mount it (created when missing). Must not be inside the
    /// backing tree or the state area.
    pub mountpoint: PathBuf,
    #[command(flatten)]
    pub pod: Pod,
    /// Kernel passthrough of reads and writes: `auto` uses it when the
    /// kernel (6.9+) and privileges (root) allow, `on` requires it, `off`
    /// never uses it.
    #[arg(long, env = "AGENTFS_PASSTHROUGH", default_value = "auto", value_parser = parse_passthrough)]
    pub passthrough: Passthrough,
    /// FUSE threads (default: the number of CPUs, between 2 and 16).
    #[arg(long, env = "AGENTFS_THREADS", default_value_t = 0)]
    pub threads: usize,
    /// Mount without `allow_other`: only the mounting user can use it.
    #[arg(long)]
    pub no_allow_other: bool,
    /// How long shutdown waits for a busy mount (files still open) to be
    /// released after it is detached.
    #[arg(long, default_value = "10s", value_parser = parse_grace)]
    pub unmount_grace: Duration,
    /// Where a workload sees the mount when not at MOUNTPOINT (a container's
    /// `mountPath`): `PATH`, or `PATH=DIR` for the tree's directory DIR (a
    /// `subPath`). Absolute `gitdir:` targets under it, written by `git
    /// worktree add`, then map exactly. Repeatable; the variable takes a
    /// comma-separated list.
    #[arg(
        long = "workload-mount",
        env = "AGENTFS_WORKLOAD_MOUNTS",
        value_delimiter = ',',
        value_parser = agentfs_fuse::views::parse_workload_mount
    )]
    pub workload_mounts: Vec<agentfs_core::nested::View>,
    /// The history service to record through: `unix:<path>` or an absolute
    /// socket path.
    #[arg(long, env = "AGENTFS_HISTORY_SERVICE", value_name = "ADDR")]
    pub history_service: String,
    /// How long a boundary waits for the history service before it fails
    /// with EIO.
    #[arg(long, env = "AGENTFS_HISTORY_TIMEOUT", default_value = "10s", value_parser = parse_grace, value_name = "DURATION")]
    pub history_timeout: Duration,
    /// How long to wait at start for the history service to be up.
    #[arg(long, env = "AGENTFS_HISTORY_WAIT", default_value = "30s", value_parser = parse_grace, value_name = "DURATION")]
    pub history_wait: Duration,
    /// Prefer pending slot N (1..=1023), else the lowest free slot. A front
    /// restarted after a crash passes its predecessor's slot so it drains
    /// that slot's repairs (the CSI driver does this).
    #[arg(long, env = "AGENTFS_HISTORY_SLOT", value_name = "N", value_parser = clap::value_parser!(u16).range(1..=1023))]
    pub history_slot: Option<u16>,
}

fn parse_passthrough(value: &str) -> Result<Passthrough, String> {
    value.parse()
}

/// How a front reaches its history service.
struct Reach {
    timeout: Duration,
    wait: Duration,
    slot: Option<u16>,
}

impl From<&HistoryArgs> for Reach {
    fn from(args: &HistoryArgs) -> Self {
        Self {
            timeout: args.history_timeout,
            wait: args.history_wait,
            slot: args.history_slot,
        }
    }
}

impl From<&FrontArgs> for Reach {
    fn from(args: &FrontArgs) -> Self {
        Self {
            timeout: args.history_timeout,
            wait: args.history_wait,
            slot: args.history_slot,
        }
    }
}

/// A connected front.
struct Front {
    // Held for as long as the front runs.
    lease: FrontLease,
    recorder: Recorder,
    address: Address,
    client: Arc<HistoryClient>,
}

impl Front {
    /// Answers the service's calls with `hooks` until the handle drops.
    fn listen(&self, hooks: Arc<dyn FrontHooks>) -> Result<ListenerHandle> {
        self.client
            .listen(hooks)
            .context("starting the history service listener")
    }
}

fn connect(
    state: &StateDir,
    tree: &Path,
    pod: &Pod,
    address: &str,
    history: &Reach,
    session: &Session,
    command: &str,
) -> Result<Front> {
    let address: Address = address.parse()?;
    let settings = History::open(&state.git())
        .with_context(|| format!("opening {}", state.root().display()))?
        .settings()
        .clone();
    let context = pod.context(&settings.volume);
    let lease = FrontLease::acquire_preferring(
        state,
        &format!(
            "command={command} pod={} replica={}",
            context.pod, context.replica
        ),
        history.slot,
    )?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let front = FrontInfo {
        origin: Origin::from_context(&context),
        slot: lease.slot(),
        instance: nanos ^ (u64::from(std::process::id()) << 32),
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        // Where its workloads see the tree, as this process knows.
        views: agentfs_core::nested::views(),
    };
    let client = HistoryClient::connect(
        ClientConfig {
            address: address.clone(),
            timeout: history.timeout,
            connect_wait: history.wait,
        },
        &settings.volume,
        &settings.branch_ref,
        front,
        session.events.clone(),
        session.telemetry().clone(),
    )
    .with_context(|| {
        format!(
            "connecting to the history service (waited up to {:?}; start `agentfs \
             serve-history` first)",
            history.wait
        )
    })?;
    let client = Arc::new(client);
    let recorder = volume::open_front(
        state,
        tree,
        session.events.clone(),
        session.telemetry(),
        client.clone(),
    )?;
    Ok(Front {
        lease,
        recorder,
        address,
        client,
    })
}

/// `agentfs record --history-service`: records PATHs through the service,
/// which commits them.
pub(crate) fn record_front(
    state: &StateDir,
    root: &Path,
    pod: &Pod,
    address: &str,
    history: &HistoryArgs,
    paths: &[String],
) -> Result<ExitCode> {
    let session = Session::start(state, pod)?;
    let front = connect(
        state,
        root,
        pod,
        address,
        &Reach::from(history),
        &session,
        "record",
    )?;
    // Nothing to settle or invalidate, but a live front that does not
    // listen makes the service refuse restores while it runs.
    let listener = front.listen(Arc::new(NoHooks))?;
    crate::record_paths(&front.recorder, root, paths)?;
    println!(
        "recorded through the history service at {} (slot {}); it commits them",
        front.address,
        front.lease.slot()
    );
    listener.stop();
    drop(front);
    session.finish();
    Ok(ExitCode::SUCCESS)
}

/// `agentfs front`: a FUSE mount whose records go to the history service,
/// until SIGINT, SIGTERM or an unmount. It takes no lock: the history
/// service holds it.
pub fn mount_front(
    daemon: Daemon,
    state: &StateDir,
    root: Option<&Path>,
    args: &FrontArgs,
) -> Result<ExitCode> {
    if let Some(root) = root {
        let same = match (root.canonicalize(), args.backing.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if !same {
            bail!(
                "--root (or AGENTFS_ROOT) {} is not the backing tree {}; drop it or make them agree",
                root.display(),
                args.backing.display()
            );
        }
    }
    let session = Session::start(state, &args.pod)?;
    let settings = History::open(&state.git())
        .with_context(|| format!("opening {}", state.root().display()))?
        .settings()
        .clone();
    crate::mountpoint::create(&args.mountpoint)?;
    let (backing, mountpoint) = preflight(
        state,
        &settings,
        &args.backing,
        &args.mountpoint,
        MountKind::Main,
    )?;
    // Where its workloads see the tree, for this front and the service.
    agentfs_core::nested::add_views(workload_views(&mountpoint, args));
    // Before mounting: a missing service exits without a mount.
    let front = connect(
        state,
        &backing,
        &args.pod,
        &args.history_service,
        &Reach::from(args),
        &session,
        "front",
    )?;
    let slot = front.lease.slot();
    let config = MountConfig {
        volume: settings.volume.clone(),
        backing: backing.clone(),
        mountpoint: mountpoint.clone(),
        passthrough: args.passthrough,
        allow_other: !args.no_allow_other,
        threads: args.threads,
        sweep_interval: settings.sweep_interval,
        repair_store: Some(state.front_repairs(slot)),
        stack_over: None,
        // A finished fork is read-only: the kernel refuses
        // its writes, whatever the binds of this mount say.
        read_only: matches!(
            agentfs_core::volume::refuse_finished(state),
            Err(agentfs_core::volume::VolumeError::ForkFinished { .. })
        ),
        overlay: None,
        unmount_grace: args.unmount_grace,
    };
    let mounted = Mount::start(front.recorder.clone(), config)?;
    add_mounted_views(&mountpoint);
    daemon.watch(mounted.ended(), Wake::Unmounted)?;
    daemon.watch(mounted.failed(), Wake::PassthroughFailed)?;
    // The service calls the mount on this for restores.
    let listener = front.listen(Arc::new(mounted.hooks()))?;
    let startup = mounted.startup_repairs();
    if startup.loaded > 0 {
        eprintln!(
            "{} path(s) a previous front in slot {slot} could not record: {} repaired from the \
             backing tree, {} still failing",
            startup.loaded, startup.repaired, startup.still_queued
        );
    }
    eprintln!(
        "mounted {} at {} (volume {}, front of history service {}, slot {slot}, passthrough: \
         {}, threads {}); stop with SIGINT or SIGTERM",
        backing.display(),
        mountpoint.display(),
        settings.volume,
        front.address,
        mounted.passthrough(),
        mounted.threads(),
    );

    let wake = daemon.wait();
    let failed = wake == Wake::PassthroughFailed;
    if failed {
        eprintln!("passthrough: {}", mounted.passthrough());
    }
    eprintln!(
        "{}: unmounting (waiting up to {:?} for open files; signal again to exit now)",
        crate::describe_wake(wake),
        args.unmount_grace,
    );
    // No more calls: an invalidation must not race the unmount.
    listener.stop();
    let report = mounted.shutdown();
    print_unmount(&report, args.unmount_grace);
    drop(front);
    session.finish();
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// `agentfs status`: the live fronts, and repair files no live front
/// drains.
pub(crate) fn print_status(state: &StateDir) {
    match FrontLease::holders(state) {
        Ok(holders) if holders.is_empty() => println!("fronts:    none"),
        Ok(holders) => {
            for (slot, holder) in &holders {
                println!("fronts:    slot {slot}: {holder}");
            }
        }
        Err(e) => println!("fronts:    unknown ({e})"),
    }
    let live = FrontLease::live(state).unwrap_or_default();
    let Ok(entries) = std::fs::read_dir(state.fronts_dir()) else {
        return;
    };
    let mut waiting: Vec<(u16, u64)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let slot = name
                .to_str()?
                .strip_prefix("slot-")?
                .strip_suffix(".repairs")?
                .parse::<u16>()
                .ok()?;
            let len = entry.metadata().ok()?.len();
            (len > 0 && !live.contains(&slot)).then_some((slot, len))
        })
        .collect();
    waiting.sort_unstable();
    for (slot, _) in waiting {
        println!(
            "repairs:   slot {slot} has paths a front could not record, waiting for the next \
             front in that slot ({})",
            state.front_repairs(slot).display()
        );
    }
}

/// Where workloads see the tree before it is mounted: the mount
/// point and `--workload-mount`.
fn workload_views(mountpoint: &Path, args: &FrontArgs) -> Vec<agentfs_core::nested::View> {
    mountpoint
        .to_str()
        .and_then(|at| agentfs_core::nested::View::new(at, ""))
        .into_iter()
        .chain(args.workload_mounts.iter().cloned())
        .collect()
}

/// Adds the other mounts of the mount at `mountpoint` this namespace shows
/// (bind mounts) to the views.
fn add_mounted_views(mountpoint: &Path) {
    match agentfs_fuse::views::mounted_views(mountpoint) {
        Ok(views) => agentfs_core::nested::add_views(views),
        Err(e) => eprintln!("warning: reading the mount's bind mounts: {e}"),
    }
}

/// Reports what an unmount did and what it could not record.
fn print_unmount(report: &UnmountReport, unmount_grace: Duration) {
    for error in &report.unmount_errors {
        eprintln!("unmount failed: {error}");
    }
    if report.lazy {
        eprintln!("the mount was busy: detached lazily");
    }
    if report.session_abandoned {
        if report.lazy || !report.unmount_errors.is_empty() {
            eprintln!(
                "files were still open after {unmount_grace:?}: their later changes are not \
                 recorded"
            );
        } else {
            // The unmount was not busy, so what holds the session is
            // another mount of this filesystem, not this one.
            eprintln!(
                "the filesystem was still mounted elsewhere after {unmount_grace:?} (a bind of \
                 it, such as a kubelet subPath mount, or a copy in another mount namespace): \
                 the session ends as agentfs exits, and changes through those mounts after \
                 this are not recorded"
            );
        }
    }
    if report.swept > 0 || report.repaired > 0 {
        println!(
            "final sweep: {} open file(s) recorded, {} failed record(s) repaired",
            report.swept, report.repaired
        );
    }
    if report.captured > 0 {
        println!("unmount: {} database group(s) captured", report.captured);
    }
    if let Some(e) = &report.tmp_wipe_error {
        eprintln!(
            "warning: the sandbox overlay's tmp layer was not wiped: {e} (the next start wipes it)"
        );
    }
    if report.unrepaired > 0 {
        match &report.repairs_saved {
            Some(store) => eprintln!(
                "{} path(s) could not be recorded (see the alarms); they are saved in {} and \
                 the next mount records them from the backing tree before it serves anything: {}",
                report.unrepaired,
                store.display(),
                report.unrepaired_paths.join(", ")
            ),
            None => eprintln!(
                "{} path(s) could not be recorded, nor saved for the next mount (see the \
                 alarms); history differs from the backing tree at these paths until each is \
                 changed again: {}",
                report.unrepaired,
                report.unrepaired_paths.join(", ")
            ),
        }
    }
}
