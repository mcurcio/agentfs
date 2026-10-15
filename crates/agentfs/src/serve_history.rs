//! `agentfs serve-history`: the history service of a volume (design §14).
//!
//! It holds the state-area lock, opens the volume (recovery included), and
//! is the only writer of the journal and the branch: fronts
//! (`mount --history-service`) hand it their records over a Unix socket,
//! and its commit and push loops do the rest, as `run` does.
//!
//! Start order: lock, open (recovery), the patch queue (only when no front
//! holds a lease: patches write the backing tree, which live fronts serve),
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

use agentfs_core::events::Event;
use agentfs_core::fence::LockHeartbeat;
use agentfs_core::history_service::{Address, Endpoint, FrontLease, HistoryServer};
use agentfs_core::lock::StateLock;
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

/// How `serve-history` serves.
pub(crate) struct Options<'a> {
    pub listen: Option<&'a str>,
    pub listen_tls: Option<&'a str>,
    pub heartbeat: LockHeartbeat,
    pub fence: Duration,
    pub push_grace: Duration,
    pub shutdown_grace: Duration,
}

/// Exit status when the state lock is lost (`EX_TEMPFAIL`): the CSI
/// supervisor restarts the service, as a standby.
const EXIT_LOCK_LOST: i32 = 75;

pub(crate) fn serve(
    daemon: Daemon,
    state: &StateDir,
    root: &Path,
    lock: &StateLock,
    options: Options<'_>,
    pod: &Pod,
) -> Result<ExitCode> {
    let Options {
        listen,
        listen_tls,
        heartbeat,
        fence,
        push_grace,
        shutdown_grace,
    } = options;
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
    if !matches!(address, Address::Unix(_)) {
        bail!(
            "--listen takes a Unix socket (unix:<path> or an absolute path); TLS is --listen-tls"
        );
    }
    let tls = match listen_tls {
        Some(tls) => Some(tls_address(tls)?),
        None => None,
    };
    let session = Session::start(state, pod)?;
    let events = session.events.clone();
    // On a shared filesystem the lock is a lease the node can lose: renew
    // it, and stop writing the moment it may be another node's (design
    // §14.2). Before the volume is opened, so recovery is fenced too.
    let _heartbeat = if heartbeat.enabled(state.root()) {
        let events = events.clone();
        let beat = lock
            .start_heartbeat(fence, move |e| {
                events.emit(Event::StateLockLost {
                    error: e.to_string(),
                });
                eprintln!(
                    "lost the state lock ({e}): exiting without a final commit; another node's \
                     standby takes over"
                );
                std::process::exit(EXIT_LOCK_LOST);
            })
            .context("starting the state lock's heartbeat")?;
        eprintln!(
            "state lock heartbeat: renewing every {:?}, fencing writes after {fence:?}",
            fence / 5
        );
        Some(beat)
    } else {
        None
    };
    let (recorder, committer, recovery) = session.open(state, &backing, pod)?;
    // A previous holder on another node that lost its lock may still have
    // the journal open: from now on it can only append to a file without
    // a name (design §14.2).
    committer
        .replace_journal_file()
        .context("replacing the journal file at takeover")?;
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
    let mut addresses = vec![address.clone()];
    if let Some(tls) = &tls {
        // The volume's certificate, made once under the state lock we hold
        // (design §14.1).
        #[cfg(feature = "history-tls")]
        if let Err(e) = agentfs_core::history_service::tls::ensure(state, &settings.volume) {
            let _ = stop_service(service, push_grace);
            session.finish();
            return Err(e).context("creating the volume's TLS identity");
        }
        addresses.push(tls.clone());
    }
    let server = match HistoryServer::start(
        &addresses,
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
    // Fronts given `state` find this service here (design §14.2).
    let bound = server.bound();
    let endpoint = Endpoint::new(
        &settings.volume,
        bound.iter().find_map(|a| match a {
            Address::Unix(p) => Some(p.clone()),
            _ => None,
        }),
        bound.iter().find_map(|a| match a {
            Address::Tls(t) => Some(*t),
            _ => None,
        }),
    );
    if let Err(e) = endpoint.write(state) {
        eprintln!(
            "warning: writing the endpoint file: {e}; fronts given `--history-service state` \
             cannot find this service"
        );
    }
    let shown: Vec<String> = bound.iter().map(Address::to_string).collect();
    eprintln!(
        "serving history for volume {} at {} on node {} ({} uncommitted record(s) recovered; \
         remotes: {}); stop with SIGINT or SIGTERM",
        settings.volume,
        shown.join(" and "),
        endpoint.node,
        recovery.uncommitted,
        list(&names)
    );

    let wake = daemon.wait();
    eprintln!(
        "{}: stopping (waiting up to {shutdown_grace:?} for fronts' requests, then \
         {push_grace:?} for the final push; signal again to exit now)",
        describe_wake(wake)
    );
    // No new front finds this service; fronts given `state` wait for the
    // next one (design §14.2).
    if let Err(e) = endpoint.remove(state) {
        eprintln!("warning: removing the endpoint file: {e}");
    }
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

/// `--listen-tls`: `<ip>:<port>` (or `tls:<ip>:<port>`); port 0 picks a
/// free one, which the endpoint file then names.
fn tls_address(value: &str) -> Result<Address> {
    let address = match value.parse::<Address>() {
        Ok(a @ Address::Tls(_)) => a,
        _ => format!("tls:{value}").parse::<Address>().map_err(|_| {
            anyhow::anyhow!("--listen-tls {value:?}: give an IP address and port, as 10.0.0.7:0")
        })?,
    };
    if !agentfs_core::history_service::TLS_BUILT {
        bail!("--listen-tls: this agentfs is built without the history-tls feature (design §14.1)");
    }
    Ok(address)
}
