//! A front of a history service (design §14): `mount --history-service`
//! and `record --history-service`.
//!
//! A front takes no state-area lock. It leases a pending slot
//! ([`FrontLease`]), connects to the service (waiting up to
//! `--history-wait` for it), and records through a front recorder: its
//! own rules, checks and freeze, then a round trip to the service, which
//! journals and commits. A front mount's failed records wait in its slot's
//! repair file (`meta/fronts/slot-N.repairs`) for the next front in that
//! slot. A front serves no control socket: restores and reverts go to
//! the service, which writes the backing tree itself and calls each front
//! on its listening connection (design §15): to settle what is dirty, to
//! say what is open for writing, and to drop its kernel's caches of what
//! was written.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};

use agentfs_core::history::History;
use agentfs_core::history_service::{
    Address, ClientConfig, FrontHooks, FrontInfo, FrontLease, HistoryClient, ListenerHandle,
    NoHooks,
};
use agentfs_core::record::Origin;
use agentfs_core::volume::{self, Recorder, StateDir};
use agentfs_fuse::{Mount, MountConfig, preflight};

use crate::daemon::{Daemon, Wake};
use crate::{HistoryArgs, MountArgs, Pod, Session};

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
    history: &HistoryArgs,
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
        history.history_slot,
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
        // Where its workloads see the tree, as this process knows (design §5).
        views: agentfs_core::nested::views(),
    };
    let client = HistoryClient::connect(
        ClientConfig {
            address: address.clone(),
            timeout: history.history_timeout,
            connect_wait: history.history_wait,
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
            history.history_wait
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
    let front = connect(state, root, pod, address, history, &session, "record")?;
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

/// `agentfs mount --history-service`: a FUSE mount whose records go to the
/// history service, until SIGINT, SIGTERM or an unmount.
pub(crate) fn mount_front(
    daemon: Daemon,
    state: &StateDir,
    root: Option<&Path>,
    args: &MountArgs,
    address: &str,
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
    crate::create_mountpoint(&args.mountpoint)?;
    let (backing, mountpoint) = preflight(state, &settings, &args.backing, &args.mountpoint)?;
    if args.seal_mountpoint {
        crate::seal_mountpoint(&mountpoint)?;
    }
    // Where its workloads see the tree, for this front and the service
    // (design §5).
    agentfs_core::nested::add_views(crate::workload_views(&mountpoint, args));
    // Before mounting: a missing service exits without a mount.
    let front = connect(
        state,
        &backing,
        &args.pod,
        address,
        &args.history,
        &session,
        "mount",
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
        // A finished fork is read-only (design §11.5): the kernel refuses
        // its writes, whatever the binds of this mount say.
        read_only: matches!(
            agentfs_core::volume::refuse_finished(state),
            Err(agentfs_core::volume::VolumeError::ForkFinished { .. })
        ),
        // Mount::start refuses the overlay on a front (not served there
        // yet), so the flag is never silently dropped.
        overlay: crate::sandbox_overlay(state, &settings, &mountpoint, args)?,
    };
    let mounted = Mount::start(front.recorder.clone(), config)?;
    crate::add_mounted_views(&mountpoint);
    daemon.watch(mounted.ended(), Wake::Unmounted)?;
    daemon.watch(mounted.failed(), Wake::PassthroughFailed)?;
    // The service calls the mount on this for restores (design §15).
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
    let report = mounted.shutdown(args.unmount_grace);
    crate::print_unmount(&report, args.unmount_grace);
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
