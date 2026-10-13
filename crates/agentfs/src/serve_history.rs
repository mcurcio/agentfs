//! `agentfs serve-history`: the history service of a volume (design §14).
//!
//! It holds the state-area lock, opens the volume (recovery included), and
//! is the only writer of the journal and the branch: fronts
//! (`mount --history-service`) hand it their records over a Unix socket,
//! and its commit and push loops do the rest, as `run` does.
//!
//! Start order: lock, open (recovery), the patch queue (only when no front
//! holds a lease: patches write the backing tree, which live fronts serve),
//! the commit and push loops, then the socket. On SIGINT or SIGTERM it
//! stops accepting, lets fronts' requests in progress finish (up to
//! `--shutdown-grace`), then makes the final commit and, unlike `run`,
//! pushes it: every remote that lacks commits gets one more attempt, all
//! within `--push-grace` (design §14.2). Under the CSI driver the service
//! stops after the volume's last pod, and nothing else would push it.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};

use agentfs_core::history_service::{Address, FrontLease, HistoryServer};
use agentfs_core::push;
use agentfs_core::service::Service;
use agentfs_core::volume::StateDir;

use crate::daemon::Daemon;
use crate::{Pod, Session, describe_wake, list, patch_cmd, stop_service_and_push};

/// Where the running service's address is written for `agentfs status`.
const ADDRESS_FILE: &str = "address";

/// The address the running history service listens on, if it said.
pub(crate) fn read_address(state: &StateDir) -> Option<String> {
    std::fs::read_to_string(state.history_dir().join(ADDRESS_FILE))
        .ok()
        .map(|a| a.trim().to_owned())
        .filter(|a| !a.is_empty())
}

pub(crate) fn serve(
    daemon: Daemon,
    state: &StateDir,
    root: &Path,
    listen: Option<&str>,
    pod: &Pod,
    push_grace: Duration,
    shutdown_grace: Duration,
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
    let settings = committer.history().settings().clone();
    if std::fs::metadata(state.repairs()).is_ok_and(|m| m.len() > 0) {
        eprintln!(
            "warning: {} lists paths a single-process mount could not record; fronts do not \
             drain it (mount without --history-service once to repair them)",
            state.repairs().display()
        );
    }
    let live = FrontLease::live(state).context("reading the front leases")?;
    if live.is_empty() {
        if let Err(e) = patch_cmd::apply_queue(
            state,
            &settings,
            &recorder,
            &committer,
            &backing,
            session.telemetry(),
            &events,
            true,
        ) {
            eprintln!("patches: the queue could not run: {e:#}");
        }
    } else {
        eprintln!(
            "patches: {} front(s) are running, so the queue is not applied; it is applied when \
             the history service starts with no fronts",
            live.len()
        );
    }
    let pushers = push::open_all(state, &settings, events.clone(), session.telemetry())?;
    let names: Vec<String> = pushers.iter().map(|p| p.name().to_owned()).collect();
    let service = Service::start(committer, pushers, events).context("starting the service")?;
    let server =
        match HistoryServer::start(&address, recorder, state, &settings, session.telemetry()) {
            Ok(server) => server,
            Err(e) => {
                // Commits what recovery found.
                let _ = stop_service_and_push(service, push_grace);
                session.finish();
                return Err(e).with_context(|| format!("listening on {address}"));
            }
        };
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
         {push_grace:?} for pushes; signal again to exit now)",
        describe_wake(wake)
    );
    if !server.shutdown(shutdown_grace) {
        eprintln!("fronts' requests were still running at shutdown; they fail with EIO");
    }
    let _ = std::fs::remove_file(&address_file);
    let code = stop_service_and_push(service, push_grace);
    session.finish();
    Ok(code)
}
