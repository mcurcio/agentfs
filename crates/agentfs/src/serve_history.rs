//! `agentfs serve-history`: the history service of a volume (design §14).
//!
//! It holds the state-area lock, opens the volume (recovery included), and
//! is the only writer of the journal and the branch: fronts
//! (`mount --history-service`) hand it their records over a Unix socket,
//! and its commit and push loops do the rest, as `run` does.
//!
//! Start order: lock, open (recovery), the patch queue (landed only when no
//! front holds a lease and `--workload-attached` was not given: patches
//! write the backing tree, which fronts serve; else judged, design §12),
//! the commit and push loops, the socket fronts use, then the control
//! socket (`agentfs push` asks it to push now; `restore` and `revert` it
//! serves live, writing the backing tree with its fronts' help, design
//! §2.3, §15). On SIGINT or SIGTERM it stops the control socket: a
//! restore in progress stops before its next path, records what it wrote
//! and replies, and the service waits for that (up to `--shutdown-grace`,
//! and at least the control socket's own grace). Then it stops accepting
//! fronts, lets their
//! requests in progress finish (up to `--shutdown-grace`), then makes the
//! final commit and pushes it, as every
//! long-running mode does: every remote that lacks commits gets one more
//! attempt, all within `--push-grace` (design §11.2, §14.2). Under the CSI
//! driver the service stops after the volume's last pod, and nothing else
//! would push it.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};

use agentfs_core::history_service::{Address, FrontLease, HistoryServer};
use agentfs_core::patch::attached::Deferred;
use agentfs_core::push;
use agentfs_core::service::Service;
use agentfs_core::volume::StateDir;

use crate::daemon::Daemon;
use crate::{
    Pod, Session, describe_wake, list, misconfigured, patch_cmd, push_now, recovery, start_control,
    stop_control_within, stop_service,
};

/// Where the running service's address is written for `agentfs status`.
const ADDRESS_FILE: &str = "address";

/// The address the running history service listens on, if it said.
pub(crate) fn read_address(state: &StateDir) -> Option<String> {
    std::fs::read_to_string(state.history_dir().join(ADDRESS_FILE))
        .ok()
        .map(|a| a.trim().to_owned())
        .filter(|a| !a.is_empty())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn serve(
    daemon: Daemon,
    state: &StateDir,
    root: &Path,
    listen: Option<&str>,
    pod: &Pod,
    push_grace: Duration,
    shutdown_grace: Duration,
    workload_attached: bool,
) -> Result<ExitCode> {
    let backing = root
        .canonicalize()
        .with_context(|| format!("the backing tree {}", root.display()))?;
    if !backing.is_dir() {
        bail!("the backing tree {} is not a directory", backing.display());
    }
    let address = match listen {
        Some(listen) => listen.parse::<Address>()?,
        None => Address::Unix(state.history_socket()),
    };
    let session = Session::start(state, pod)?;
    let events = session.events.clone();
    let (recorder, committer, recovery) = session.open(state, &backing, pod)?;
    let commit_window = committer.history().settings().commit_window;
    // Orders fronts' records that race for one path (design §14.1).
    recorder.enable_stamps();
    let settings = committer.history().settings().clone();
    if std::fs::metadata(state.repairs()).is_ok_and(|m| m.len() > 0) {
        eprintln!(
            "warning: {} lists paths a single-process mount could not record; fronts do not \
             drain it (mount without --history-service once to repair them)",
            state.repairs().display()
        );
    }
    // The patch queue lands only with nothing attached (design §12): no
    // front holds a lease, and the launcher did not say a workload is
    // attached (the CSI node plugin says so while a pod of the volume is
    // published, whether or not its front is back yet).
    let live = FrontLease::live(state).context("reading the front leases")?;
    let deferred = if workload_attached {
        Some(Deferred::WorkloadAttached)
    } else if !live.is_empty() {
        Some(Deferred::FrontsRunning)
    } else {
        None
    };
    patch_cmd::queue_at_start(
        state,
        &settings,
        &recorder,
        &committer,
        &backing,
        session.telemetry(),
        &events,
        deferred,
    );
    let pushers = push::open_all(state, &settings, events.clone(), session.telemetry())?;
    let names: Vec<String> = pushers.iter().map(|p| p.name().to_owned()).collect();
    let applier = patch_cmd::Applier::new(&committer, events.clone(), session.telemetry());
    let service = Service::start(committer, pushers, events).context("starting the service")?;
    let server = match HistoryServer::start(
        &address,
        recorder.clone(),
        state,
        &settings,
        session.telemetry(),
    ) {
        Ok(server) => server,
        Err(e) => {
            // Commits what recovery found.
            let _ = stop_service(service, push_grace);
            session.finish();
            return Err(e).with_context(|| format!("listening on {address}"));
        }
    };
    // `agentfs push` asks it to push now; restores and reverts it writes
    // into the backing tree, calling its fronts (design §2.3, §15).
    // Set at shutdown: a restore in progress stops before its next path.
    let stopping = Arc::new(AtomicBool::new(false));
    let control = start_control(
        state,
        push_now::handler(
            state.clone(),
            service.handle(),
            misconfigured(&settings),
            push_now::Restores::ServeFronts(recovery::ServiceDaemon {
                state: state.clone(),
                recorder,
                fronts: server.listening_fronts(),
                backing: backing.clone(),
                commit_window,
                stopping: stopping.clone(),
            }),
            applier,
        ),
    );
    let address_file = state.history_dir().join(ADDRESS_FILE);
    if let Err(e) = std::fs::create_dir_all(state.history_dir())
        .and_then(|()| std::fs::write(&address_file, format!("{address}\n")))
    {
        eprintln!(
            "warning: writing {}: {e}; status will not show the address",
            address_file.display()
        );
    }
    eprintln!(
        "serving history for volume {} at {address} ({} uncommitted record(s) recovered; \
         remotes: {}); stop with SIGINT or SIGTERM",
        settings.volume,
        recovery.uncommitted,
        list(&names)
    );

    let wake = daemon.wait();
    eprintln!(
        "{}: stopping (waiting up to {shutdown_grace:?} for fronts' requests, then \
         {push_grace:?} for the final push; signal again to exit now)",
        describe_wake(wake)
    );
    // The control socket first: a restore in progress still calls fronts.
    // It stops before its next path and records what it wrote; that, and
    // its reply, are waited for.
    stopping.store(true, Ordering::SeqCst);
    if !stop_control_within(
        &service,
        control,
        shutdown_grace.max(push_now::CONTROL_GRACE),
    ) {
        eprintln!(
            "a restore or revert was still writing one path at shutdown: what it wrote and \
             did not record yet differs from history until it is run again (it then checks \
             anew) or those paths change again"
        );
    }
    if !server.shutdown(shutdown_grace) {
        eprintln!("fronts' requests were still running at shutdown; they fail with EIO");
    }
    let _ = std::fs::remove_file(&address_file);
    let code = stop_service(service, push_grace);
    session.finish();
    Ok(code)
}
