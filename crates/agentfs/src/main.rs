//! The `agentfs` command.
//!
//! `mount` mounts a backing tree through the FUSE layer (`agentfs_fuse`)
//! and runs the commit and push loops in the same process until SIGINT,
//! SIGTERM or an unmount (design §17.1). `record` records paths from a
//! directory directly, through the same recorder, including its path rules:
//! paths the rules exclude are reported as skipped, with the reason.
//!
//! `push` pushes once to the configured remotes, or asks a running daemon
//! (`mount`, `run`, `serve-history`) to commit and push now over its
//! control socket ([`push_now`], design §2.3); `run` runs the commit and
//! push loops in the foreground until SIGINT or SIGTERM (design §11.2).
//!
//! `restore` and `revert` write content from history back into the tree,
//! recorded as one commit (design §15; [`recovery`]).
//!
//! `init` creates a state area or accepts one that already belongs to the
//! volume, so a pod can run it at every start; `probe` reports whether a
//! mount point holds a live agentfs mount, for Kubernetes probes; and
//! `mount --on-stale-mount recover` makes a sidecar restarted after a crash
//! mount over the stale mount its predecessor left, and `hold` makes it
//! wait for a pod restart instead ([`hold`], design §17.1).
//!
//! `serve-history` is a volume's history service and `mount
//! --history-service` and `record --history-service` are its fronts, one
//! per pod (design §14; [`serve_history`], [`front`]): the service is the
//! only writer of the journal and branch, and each front records its pod's
//! changes through it, carrying its pod's committer.
//!
//! `mount`, `run`, `record`, `flush`, `serve-history`, and `push`,
//! `restore` and `revert` when no daemon answers, take the state-area lock
//! (design §2.3) and exit 75 while another process holds it; fronts take
//! no lock; `status` is read-only and takes no lock.
//!
//! `mount`, `flush`, `record`, `push` and `run` export telemetry over OTLP
//! when the standard `OTEL_*` variables configure an endpoint (see
//! [`otel`], design §13); alarms are also printed to stderr. `init` and
//! `status` export nothing.

mod control;
mod daemon;
#[cfg(test)]
mod doc_tables;
mod front;
mod hold;
mod otel;
mod patch_cmd;
mod push_now;
mod recovery;
#[cfg(test)]
mod reference_tests;
mod serve_history;

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Parser, Subcommand};

use agentfs_core::config::parse_duration;
use agentfs_core::events::{Event, Events, PushStage};
use agentfs_core::history::History;
use agentfs_core::identity::Context;
use agentfs_core::lfs;
use agentfs_core::lock::{LockError, StateLock};
use agentfs_core::paths::{PathError, RepoPath};
use agentfs_core::push::{self, PushOutcome, RemotePusher, RemoteStatus};
use agentfs_core::record::{Caller, Trigger};
use agentfs_core::service::{FinalPush, Service};
use agentfs_core::telemetry::Telemetry;
use agentfs_core::volume::{self, Input, StateDir};
use agentfs_fuse::{
    Mount, MountConfig, MountState, Passthrough, StackOver, UnmountReport, agentfs_layers,
    detach_stale_layers, mount_state, preflight, preflight_stacked,
};

/// Most stale layers a stop (or a hold) detaches at the mount point: one
/// per crash recovered by stacking (design §17.1).
pub const STALE_LAYERS: usize = 16;

use crate::daemon::{Daemon, Wake};

#[derive(Parser)]
#[command(version, about = "Records every change an agent makes, in git history")]
struct Cli {
    /// The volume's state area.
    #[arg(long, global = true, env = "AGENTFS_STATE", default_value = ".")]
    state: PathBuf,
    /// The backing tree that paths are relative to. Required by `record`,
    /// which records a missing path as removed; other commands default to
    /// the current directory.
    #[arg(long, global = true, env = "AGENTFS_ROOT")]
    root: Option<PathBuf>,
    /// How long to wait for another agentfs process to release the state
    /// area (`mount`, `run`, `record`, `flush` and `push` take it; design
    /// §2.3). By default a busy state area exits 75 at once.
    #[arg(long, global = true, env = "AGENTFS_WAIT_LOCK", default_value = "0s", value_parser = parse_grace, value_name = "DURATION")]
    wait_lock: Duration,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a state area, or check an existing one: safe to run at every
    /// start (exit 0 when it already belongs to the volume, 1 when it
    /// belongs to another volume or branch). With --root, also creates the
    /// backing tree directory.
    Init {
        /// The volume's name, unique within any repository it pushes to.
        #[arg(long, env = "AGENTFS_VOLUME")]
        volume: String,
        /// History branch (default: agentfs/<volume>).
        #[arg(long)]
        branch: Option<String>,
        /// An operator config fragment to include (`include.path`, design
        /// §4.3); repeatable. When given, these become exactly the
        /// state area's includes.
        #[arg(long = "include", value_name = "FILE")]
        includes: Vec<PathBuf>,
    },
    /// Show the branch head, the last committed sequence and the backlog.
    Status(Pod),
    /// Commit every recorded change now.
    Flush(Pod),
    /// Record the current state of PATHs under ROOT, then commit. With
    /// --history-service, record through the history service instead,
    /// which commits them (design §14).
    Record {
        /// Paths relative to ROOT. A missing path is recorded as removed. A
        /// path in the fsync class records its whole group (database,
        /// -wal, -journal) as one record; stop the database writer first.
        #[arg(required = true)]
        paths: Vec<String>,
        #[command(flatten)]
        pod: Pod,
        #[command(flatten)]
        history: HistoryArgs,
    },
    /// Serve the volume's history to fronts (`mount --history-service`)
    /// until SIGINT or SIGTERM: the only writer of the journal and the
    /// branch, with the commit and push loops (design §14). Needs --root,
    /// the backing tree the fronts mount. A second signal exits at once.
    ServeHistory {
        /// Where to listen: `unix:<path>` or an absolute socket path
        /// (default: <state>/meta/history/history.sock).
        #[arg(long, env = "AGENTFS_HISTORY_LISTEN", value_name = "ADDR")]
        listen: Option<String>,
        #[command(flatten)]
        pod: Pod,
        /// How long shutdown may spend on a last push of the final commit;
        /// 0s skips it.
        #[arg(long, default_value = "5s", value_parser = parse_grace)]
        push_grace: Duration,
        /// How long shutdown waits for fronts' requests in progress.
        #[arg(long, default_value = "10s", value_parser = parse_grace)]
        shutdown_grace: Duration,
    },
    /// Push the volume's refs to its remotes once. With a daemon running
    /// (`mount`, `run`, `serve-history`), asks it to commit and push now
    /// over its control socket; otherwise pushes directly. Exits 1 unless
    /// every selected remote succeeded.
    Push {
        /// Push only this remote; repeatable (default: every remote).
        #[arg(long = "remote", value_name = "NAME")]
        remotes: Vec<String>,
        /// How long to wait for a running daemon's push (1s to 10m); a push
        /// still running then carries on in the background. Pushing
        /// directly ignores it.
        #[arg(long, default_value = "120s", value_parser = parse_grace, value_name = "DURATION")]
        timeout: Duration,
        #[command(flatten)]
        pod: Pod,
    },
    /// Run the commit and push loops until SIGINT or SIGTERM. A second
    /// signal exits at once.
    Run {
        #[command(flatten)]
        pod: Pod,
        /// How long shutdown may spend on a last push of the final commit;
        /// 0s skips it.
        #[arg(long, default_value = "5s", value_parser = parse_grace)]
        push_grace: Duration,
    },
    /// Write PATH (a file, symlink or directory, or `.` for the whole
    /// tree) back as it was in COMMIT, removing what COMMIT did not have
    /// there; recorded as one commit (design §15). Goes through a running
    /// mount when there is one; otherwise writes --root directly.
    Restore {
        /// Relative to the tree root.
        path: String,
        /// A commit on the volume's branch (any git revision syntax).
        commit: String,
        /// Write even files open for writing through the mount.
        #[arg(long)]
        force: bool,
        /// Print the plan and write nothing.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        pod: Pod,
    },
    /// Undo COMMIT's changes (against its first parent); recorded as one
    /// commit (design §15). Through a running mount when there is one;
    /// otherwise writes --root directly.
    Revert {
        /// A commit on the volume's branch (any git revision syntax).
        commit: String,
        /// Revert paths changed again since COMMIT too (the later changes
        /// stay in history), and write files open for writing through the
        /// mount.
        #[arg(long)]
        force: bool,
        /// Print the plan and write nothing.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        pod: Pod,
    },
    /// Mount BACKING at MOUNTPOINT and record every change through it,
    /// with the commit and push loops, until SIGINT, SIGTERM or an unmount.
    /// A second signal exits at once.
    Mount(MountArgs),
    /// Report what is mounted at MOUNTPOINT: a live agentfs mount (exit 0),
    /// a stale one whose daemon died, nothing, or another filesystem (exit
    /// 1). Takes no lock and reads no state area: for startup and readiness
    /// probes (design §17.1).
    Probe {
        mountpoint: PathBuf,
        /// Exit 0 for a stale agentfs mount too (a sidecar holding after a
        /// crash).
        #[arg(long)]
        allow_stale: bool,
    },
    /// The patch queue (design §12): submit a patch for a volume, apply
    /// the queue now, or show it.
    Patch {
        #[command(subcommand)]
        command: patch_cmd::PatchCommand,
    },
}

/// Being a front of a history service (design §14).
#[derive(Args)]
struct HistoryArgs {
    /// Record through the history service at ADDR (`unix:<path>` or an
    /// absolute socket path) as a front: take no state-area lock, freeze
    /// into a leased pending slot, and let the service journal and commit.
    #[arg(long, env = "AGENTFS_HISTORY_SERVICE", value_name = "ADDR")]
    history_service: Option<String>,
    /// How long a boundary waits for the history service before it fails
    /// with EIO.
    #[arg(long, env = "AGENTFS_HISTORY_TIMEOUT", default_value = "10s", value_parser = parse_grace, value_name = "DURATION")]
    history_timeout: Duration,
    /// How long to wait at start for the history service to be up.
    #[arg(long, env = "AGENTFS_HISTORY_WAIT", default_value = "30s", value_parser = parse_grace, value_name = "DURATION")]
    history_wait: Duration,
    /// Prefer pending slot N (1..=1023) when leasing as a front, else the
    /// lowest free slot. A front restarted after a crash passes its
    /// predecessor's slot so it drains that slot's repairs (the CSI
    /// driver does this, design §14.2).
    #[arg(long, env = "AGENTFS_HISTORY_SLOT", value_name = "N", value_parser = clap::value_parser!(u16).range(1..=1023))]
    history_slot: Option<u16>,
}

fn parse_grace(value: &str) -> Result<Duration, String> {
    parse_duration(value).ok_or_else(|| format!("{value:?} is not a duration (use e.g. 500ms, 5s)"))
}

/// Where the volume is mounted. In Kubernetes these come from the downward
/// API.
#[derive(Args)]
struct Pod {
    #[arg(long, env = "AGENTFS_NAMESPACE", default_value = "")]
    namespace: String,
    #[arg(long, env = "AGENTFS_POD", default_value = "")]
    pod: String,
    #[arg(long, env = "AGENTFS_POD_UID", default_value = "")]
    pod_uid: String,
    #[arg(long, env = "AGENTFS_NODE", default_value = "")]
    node: String,
    /// Committer identity's `{replica}` (default: the pod name, else the
    /// host name).
    #[arg(long, env = "AGENTFS_REPLICA")]
    replica: Option<String>,
}

impl Pod {
    fn context(&self, volume: &str) -> Context {
        let replica = self
            .replica
            .clone()
            .filter(|r| !r.is_empty())
            .or_else(|| (!self.pod.is_empty()).then(|| self.pod.clone()))
            .unwrap_or_else(hostname);
        Context {
            volume: volume.to_owned(),
            namespace: self.namespace.clone(),
            pod: self.pod.clone(),
            pod_uid: if self.pod_uid.is_empty() {
                replica.clone()
            } else {
                self.pod_uid.clone()
            },
            node: self.node.clone(),
            replica,
        }
    }
}

/// Exit status when another process holds the state area (`EX_TEMPFAIL`).
const EXIT_LOCKED: u8 = 75;

fn main() -> ExitCode {
    match real_main() {
        Ok(code) => code,
        Err(e) => match e.downcast_ref::<LockError>() {
            Some(busy @ LockError::Busy { .. }) => {
                eprintln!("error: {busy}");
                ExitCode::from(EXIT_LOCKED)
            }
            _ => {
                eprintln!("Error: {e:?}");
                ExitCode::FAILURE
            }
        },
    }
}

fn real_main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let state = StateDir::new(&cli.state);
    match cli.command {
        Command::Init {
            volume,
            branch,
            includes,
        } => init(
            &state,
            cli.root.as_deref(),
            cli.wait_lock,
            &volume,
            branch,
            includes,
        )?,
        Command::Status(_pod) => {
            // Read-only: no recovery, no lock, so it is safe beside a
            // running `mount` or `run` (design §2.3).
            let (history, inspection) = volume::inspect(&state)
                .with_context(|| format!("opening {}", state.root().display()))?;
            println!("volume:    {}", history.settings().volume);
            println!("branch:    {}", history.settings().branch_ref);
            match inspection.head {
                Some(head) => println!("head:      {head}"),
                None => println!("head:      (none)"),
            }
            match inspection.committed_through {
                Some(seq) => println!("committed: through seq {seq}"),
                None => println!("committed: nothing"),
            }
            println!("backlog:   {} record(s)", inspection.backlog);
            if inspection.torn_bytes > 0 {
                println!(
                    "journal:   {} byte(s) past the last valid record (a torn tail, \
                     or a record being written)",
                    inspection.torn_bytes
                );
            }
            let holder = StateLock::holder(&state)?;
            match &holder {
                Some(holder) => println!("lock:      held by {holder}"),
                None => println!("lock:      free"),
            }
            if holder.is_some_and(|h| h.contains("command=serve-history")) {
                match serve_history::read_address(&state) {
                    Some(address) => println!("history:   serving fronts at {address}"),
                    None => println!("history:   serving fronts"),
                }
            }
            front::print_status(&state);
            let settings = history.settings();
            match settings.lfs.threshold {
                Some(bytes) => println!("lfs:       files over {bytes} byte(s) are stored as LFS"),
                None => println!("lfs:       no size threshold (only lfs attributes select LFS)"),
            }
            for remote in push::status(&state, settings)? {
                println!("{}", remote_line(&remote));
                // Where its LFS objects go: resolved from config, no network.
                if let Some(r) = settings.remotes.iter().find(|r| r.name == remote.name) {
                    match lfs::endpoint::resolve(settings, r, lfs::Direction::Upload) {
                        Ok(endpoint) => println!("           lfs: {}", endpoint.describe()),
                        Err(e) => println!("           lfs: none ({e})"),
                    }
                }
            }
            let patches = agentfs_core::patch::status::PatchState::load(&state.patch_state());
            match settings.patch_remote() {
                Some(remote) => println!("patches:   {} (from {})", patches.summary(), remote.name),
                None => println!("patches:   {} (no patch remote)", patches.summary()),
            }
            for bad in &history.settings().invalid_remotes {
                println!(
                    "remote:    {}: misconfigured, not pushed: {}",
                    bad.name, bad.error
                );
            }
        }
        Command::Flush(pod) => {
            let _lock = StateLock::acquire(&state, "flush", cli.wait_lock)?;
            let session = Session::start(&state, &pod)?;
            let (_, mut committer, _) =
                session.open(&state, cli.root.as_deref().unwrap_or(Path::new(".")), &pod)?;
            // Without --root the tree is a guess: no commit-time probes of it.
            committer.set_tree_known(cli.root.is_some());
            let report = committer.commit_pending()?;
            println!(
                "{} record(s) into {} commit(s)",
                report.records, report.commits
            );
            session.finish();
        }
        Command::Record {
            paths,
            pod,
            history,
        } => {
            // Guessing the root would turn every path missing from the wrong
            // directory into a recorded removal.
            let Some(root) = cli.root.as_deref() else {
                bail!("record needs --root (or AGENTFS_ROOT): the backing tree the paths are in");
            };
            if let Some(address) = &history.history_service {
                return front::record_front(&state, root, &pod, address, &history, &paths);
            }
            let _lock = StateLock::acquire(&state, "record", cli.wait_lock)?;
            let session = Session::start(&state, &pod)?;
            let (recorder, committer, _) = session.open(&state, root, &pod)?;
            record_paths(&recorder, root, &paths)?;
            let report = committer.commit_pending()?;
            println!(
                "{} record(s) into {} commit(s)",
                report.records, report.commits
            );
            session.finish();
        }
        Command::ServeHistory {
            listen,
            pod,
            push_grace,
            shutdown_grace,
        } => {
            let daemon = Daemon::start()?;
            let Some(root) = cli.root.as_deref() else {
                bail!(
                    "serve-history needs --root (or AGENTFS_ROOT): the backing tree its fronts \
                     mount"
                );
            };
            let _lock = StateLock::acquire(&state, "serve-history", cli.wait_lock)?;
            return serve_history::serve(
                daemon,
                &state,
                root,
                listen.as_deref(),
                &pod,
                push_grace,
                shutdown_grace,
            );
        }
        Command::Push {
            remotes,
            timeout,
            pod,
        } => {
            // A running daemon pushes for us (design §2.3).
            if let Some(code) = push_now::client(&state, &remotes, timeout) {
                return Ok(code);
            }
            let _lock = StateLock::acquire(&state, "push", cli.wait_lock)?;
            let session = Session::start(&state, &pod)?;
            let code = push_once(&state, &remotes, &session)?;
            session.finish();
            return Ok(code);
        }
        Command::Run { pod, push_grace } => {
            let daemon = Daemon::start()?;
            let _lock = StateLock::acquire(&state, "run", cli.wait_lock)?;
            return run(daemon, &state, cli.root.as_deref(), &pod, push_grace);
        }
        Command::Restore {
            path,
            commit,
            force,
            dry_run,
            pod,
        } => {
            let ask = match recovery::Ask::restore(&path, commit, force, dry_run) {
                Ok(ask) => ask,
                Err(e) => return Ok(recovery::refused("restore", &e)),
            };
            return recovery::run(&state, cli.root.as_deref(), cli.wait_lock, &pod, ask);
        }
        Command::Revert {
            commit,
            force,
            dry_run,
            pod,
        } => {
            let ask = recovery::Ask::revert(commit, force, dry_run);
            return recovery::run(&state, cli.root.as_deref(), cli.wait_lock, &pod, ask);
        }
        Command::Probe {
            mountpoint,
            allow_stale,
        } => return Ok(probe(&mountpoint, allow_stale)),
        Command::Patch { command } => {
            return patch_cmd::run(command, &state, cli.root.as_deref(), cli.wait_lock);
        }
        Command::Mount(args) => {
            let daemon = Daemon::start()?;
            // A restarted sidecar whose daemon died (design §17.1).
            let stale = match mount_state(&args.mountpoint) {
                Ok(MountState::Stale { source }) => Some(source),
                _ => None,
            };
            let mut stacked = false;
            if let Some(source) = &stale {
                match args.on_stale_mount {
                    // Mount nothing and take no lock.
                    OnStaleMount::Hold => return Ok(hold::hold(daemon, &args.mountpoint, source)),
                    OnStaleMount::Recover => {
                        if let Some(refusal) = recover_refusal(&args) {
                            eprintln!(
                                "error: --on-stale-mount recover: {refusal}; {} is a stale \
                                 agentfs mount ({source}) and is left as it is",
                                args.mountpoint.display()
                            );
                            eprintln!(
                                "hint: use --on-stale-mount hold (and restart the pod), or \
                                 detach it: umount -l {}",
                                args.mountpoint.display()
                            );
                            return Ok(ExitCode::FAILURE);
                        }
                        stacked = true;
                    }
                    // preflight reports it, with the command that clears it.
                    OnStaleMount::Refuse => {}
                }
            }
            if let Some(address) = &args.history.history_service {
                // A front takes no lock: the history service holds it.
                return front::mount_front(daemon, &state, cli.root.as_deref(), &args, address);
            }
            // The dead daemon's lock went with it.
            let _lock = StateLock::acquire(&state, "mount", cli.wait_lock)?;
            return mount(daemon, &state, cli.root.as_deref(), &args, stacked);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `agentfs init`: creates the state area or checks an existing one
/// (design §2.3, §17.1), and creates `--root` when it is missing.
fn init(
    state: &StateDir,
    root: Option<&Path>,
    wait_lock: Duration,
    volume: &str,
    branch: Option<String>,
    includes: Vec<PathBuf>,
) -> Result<()> {
    // An existing state area may be in use: take its lock. A new one is
    // created whole, so there is nothing to lock yet (and a refused name
    // leaves nothing behind).
    let _lock = if state.git().join("config").exists() {
        Some(StateLock::acquire(state, "init", wait_lock)?)
    } else {
        None
    };
    let includes = includes
        .iter()
        .map(std::path::absolute)
        .collect::<std::io::Result<Vec<_>>>()
        .context("resolving --include")?;
    let includes = (!includes.is_empty()).then_some(includes.as_slice());
    let outcome = volume::init_with(state, volume, branch.as_deref(), includes)
        .with_context(|| format!("initialising {}", state.root().display()))?;
    if let Some(root) = root
        && !root.exists()
    {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(root)
            .with_context(|| format!("creating the backing tree {}", root.display()))?;
        println!("created the backing tree {}", root.display());
    }
    match outcome {
        volume::Init::Created => {
            println!("initialised volume {volume} in {}", state.root().display());
        }
        volume::Init::AlreadyInitialised => {
            println!(
                "volume {volume} already initialised in {}",
                state.root().display()
            );
        }
    }
    Ok(())
}

/// `agentfs push`: one attempt per selected remote, in name order.
fn push_once(state: &StateDir, only: &[String], session: &Session) -> Result<ExitCode> {
    let history = History::open(&state.git())
        .with_context(|| format!("opening {}", state.root().display()))?;
    let settings = history.settings();
    session.telemetry().observe_volume(state, settings);
    let events = session.events.clone();
    let mut ok = true;
    for bad in &settings.invalid_remotes {
        if only.is_empty() || only.contains(&bad.name) {
            events.emit(Event::RemoteMisconfigured {
                remote: bad.name.clone(),
                error: bad.error.clone(),
            });
            ok = false;
        }
    }
    for name in only {
        if settings.invalid_remotes.iter().any(|r| r.name == *name) {
            continue;
        }
        if !settings.remotes.iter().any(|r| r.name == *name) {
            bail!("no remote named {name:?} is configured");
        }
    }
    let selected: Vec<_> = settings
        .remotes
        .iter()
        .filter(|r| only.is_empty() || only.contains(&r.name))
        .collect();
    if selected.is_empty() {
        if ok {
            println!("no remotes configured");
            return Ok(ExitCode::SUCCESS);
        }
        return Ok(ExitCode::FAILURE);
    }
    for remote in selected {
        let mut pusher =
            RemotePusher::open(state, settings, remote, events.clone(), session.telemetry())?;
        let outcome = pusher.push_once();
        ok &= outcome.is_success();
        println!("{}: {}", remote.name, describe(&outcome));
        if let Some(queue) = pusher.queue() {
            ok &= queue.error.is_none();
            println!("{}: queue: {}", remote.name, queue.summary());
        }
    }
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn describe(outcome: &PushOutcome) -> String {
    match outcome {
        PushOutcome::NothingToPush => "nothing to push".into(),
        PushOutcome::UpToDate { head } => format!("up to date at {head}"),
        PushOutcome::Pushed { from, to } => match from {
            Some(from) => format!("pushed {from}..{to}"),
            None => format!("pushed none..{to}"),
        },
        PushOutcome::Diverged {
            local_head,
            diverged_ref: Some(saved),
            ..
        } => format!("diverged: head {local_head} saved as {saved}"),
        PushOutcome::Diverged {
            local_head,
            diverged_ref: None,
            ..
        } => format!("diverged: head {local_head} could not be saved yet"),
        PushOutcome::Failed {
            kind,
            message,
            retry_in,
            stage: PushStage::Git,
        } => format!("failed ({kind}): {message}; retry in {retry_in:?}"),
        PushOutcome::Failed {
            kind,
            message,
            retry_in,
            stage,
        } => format!("failed ({kind}, stage {stage}): {message}; retry in {retry_in:?}"),
        PushOutcome::Busy => "busy: another push in progress".into(),
    }
}

/// One `status` line per remote.
fn remote_line(remote: &RemoteStatus) -> String {
    let state = &remote.state;
    let mut line = format!(
        "remote:    {} {}: last pushed {}",
        remote.name,
        remote.url,
        state.last_pushed.as_deref().unwrap_or("never")
    );
    match remote.unpushed {
        Some(n) => line.push_str(&format!(", {n} unpushed")),
        None => line.push_str(", no branch yet"),
    }
    line.push_str(&format!(
        ", {} consecutive failure(s)",
        state.consecutive_failures
    ));
    if let Some(error) = &state.last_error {
        let stage = match error.stage {
            PushStage::Git => "",
            PushStage::Lfs => ", stage lfs",
            PushStage::Queue => ", stage queue",
        };
        line.push_str(&format!(
            ", last error ({}{stage}): {}",
            error.kind, error.message
        ));
    }
    if let Some(diverged) = &state.diverged {
        match &diverged.diverged_ref {
            Some(saved) => line.push_str(&format!(", diverged: head saved as {saved}")),
            None => line.push_str(", diverged: head not saved yet"),
        }
    }
    if let Some(error) = &state.queue_error {
        line.push_str(&format!(
            ", queue push failed ({}): {}",
            error.kind, error.message
        ));
    }
    line
}

/// `agentfs run`: the commit and push loops until SIGINT or SIGTERM.
fn run(
    daemon: Daemon,
    state: &StateDir,
    root: Option<&Path>,
    pod: &Pod,
    push_grace: Duration,
) -> Result<ExitCode> {
    let session = Session::start(state, pod)?;
    let events = session.events.clone();
    let (_recorder, mut committer, recovery) =
        session.open(state, root.unwrap_or(Path::new(".")), pod)?;
    // Without --root the tree is a guess: no commit-time probes of it.
    committer.set_tree_known(root.is_some());
    let settings = committer.history().settings().clone();
    let pushers = push::open_all(state, &settings, events.clone(), session.telemetry())?;
    let names: Vec<String> = pushers.iter().map(|p| p.name().to_owned()).collect();
    let service = Service::start(committer, pushers, events).context("starting the service")?;
    // `agentfs push` asks it to push now; restores are refused (design
    // §2.3).
    let control = start_control(
        state,
        push_now::handler(
            service.handle(),
            misconfigured(&settings),
            push_now::Restores::Refuse(push_now::RUN_SERVES_NO_RESTORES),
        ),
    );
    eprintln!(
        "running volume {} ({} uncommitted record(s) recovered; remotes: {}); \
         stop with SIGINT or SIGTERM",
        settings.volume,
        recovery.uncommitted,
        list(&names)
    );

    let wake = daemon.wait();
    eprintln!(
        "{}: stopping (waiting up to {push_grace:?} for the final push; signal again to exit now)",
        describe_wake(wake)
    );
    stop_control(&service, control);
    let code = stop_service(service, push_grace);
    session.finish();
    Ok(code)
}

/// The remotes the settings refused, with why: `agentfs push` reports them.
fn misconfigured(settings: &agentfs_core::config::Settings) -> Vec<(String, String)> {
    settings
        .invalid_remotes
        .iter()
        .map(|r| (r.name.clone(), r.error.clone()))
        .collect()
}

/// Starts the control socket (design §2.2), or warns: the daemon works
/// without it, but `agentfs push` then pushes directly and the lock
/// refuses it.
fn start_control(state: &StateDir, handler: control::Handler) -> Option<control::Server> {
    match control::Server::start(&state.control_dir(), handler) {
        Ok(server) => Some(server),
        Err(e) => {
            eprintln!(
                "warning: control socket in {}: {e}; `agentfs push` cannot reach this daemon",
                state.control_dir().display()
            );
            None
        }
    }
}

/// Stops the control socket of `run` or `serve-history`: requests waiting
/// on the service return first.
fn stop_control(service: &Service, control: Option<control::Server>) {
    service.handle().close();
    if let Some(control) = control {
        control.shutdown(push_now::CONTROL_GRACE);
    }
}

fn list(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

fn describe_wake(wake: Wake) -> String {
    match wake {
        Wake::Signal(signal) => format!("signal {signal}"),
        Wake::Unmounted => "unmounted from outside".to_owned(),
        Wake::PassthroughFailed => "--passthrough on failed".to_owned(),
    }
}

/// Stops the service: the final commit, then one final push of what the
/// remotes lack, within `push_grace` (design §11.2). Exit status 1 if the
/// final commit failed.
fn stop_service(service: Service, push_grace: Duration) -> ExitCode {
    report_stop(service.shutdown(push_grace))
}

fn report_stop(report: agentfs_core::service::ShutdownReport) -> ExitCode {
    let code = match report.final_commit {
        Ok(commit) => {
            println!(
                "final commit: {} record(s) into {} commit(s)",
                commit.records, commit.commits
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("final commit failed: {e}; the records stay journaled");
            ExitCode::FAILURE
        }
    };
    for (name, result) in &report.final_pushes {
        println!("final push: {name}: {}", describe_final(result));
    }
    for name in &report.abandoned_pushes {
        println!("final push: {name}: still running at the push grace's end; abandoned");
    }
    code
}

fn describe_final(result: &FinalPush) -> String {
    match result {
        FinalPush::Pushed(outcome) => describe(outcome),
        FinalPush::NothingNew => "nothing new".to_owned(),
        FinalPush::Skipped { unpushed } => {
            format!("skipped, push grace spent ({unpushed} commit(s) wait for the next start)")
        }
    }
}

/// `agentfs mount`'s own options.
#[derive(Args)]
struct MountArgs {
    /// The backing tree: the real directory whose files the mount shows.
    backing: PathBuf,
    /// Where to mount it. Must not be inside the backing tree or the state
    /// area.
    mountpoint: PathBuf,
    #[command(flatten)]
    pod: Pod,
    /// Kernel passthrough of reads and writes: `auto` uses it when the
    /// kernel (6.9+) and privileges (root) allow, `on` requires it, `off`
    /// never uses it.
    #[arg(long, env = "AGENTFS_PASSTHROUGH", default_value = "auto", value_parser = parse_passthrough)]
    passthrough: Passthrough,
    /// FUSE threads (default: the number of CPUs, between 2 and 16).
    #[arg(long, env = "AGENTFS_THREADS", default_value_t = 0)]
    threads: usize,
    /// Mount without `allow_other`: only the mounting user can use it.
    #[arg(long)]
    no_allow_other: bool,
    /// How long shutdown may spend on a last push of the final commit; 0s
    /// skips it.
    #[arg(long, default_value = "5s", value_parser = parse_grace)]
    push_grace: Duration,
    /// How long shutdown waits for a busy mount (files still open) to be
    /// released after it is detached.
    #[arg(long, default_value = "10s", value_parser = parse_grace)]
    unmount_grace: Duration,
    /// What to do when MOUNTPOINT is an agentfs mount whose daemon died:
    /// `refuse` (exit 1 with the command that clears it), `hold` (a
    /// restarted sidecar: mount nothing, wait for SIGTERM, then detach the
    /// stale mount) or `recover` (a restarted sidecar: mount over the stale
    /// mount, so the workload's copies of it reach the new daemon; needs
    /// CAP_SYS_ADMIN; design §17.1).
    #[arg(
        long,
        env = "AGENTFS_ON_STALE_MOUNT",
        default_value = "refuse",
        value_enum
    )]
    on_stale_mount: OnStaleMount,
    /// Seal the bare mount point before mounting: owned by root, mode 0000,
    /// so a non-root workload that reaches it while no daemon is mounted
    /// (a `subPath` bound while the sidecar restarts) gets EACCES instead
    /// of writing where nothing records it. Needs root (design §17.1).
    #[arg(long, env = "AGENTFS_SEAL_MOUNTPOINT")]
    seal_mountpoint: bool,
    #[command(flatten)]
    history: HistoryArgs,
}

/// `mount --on-stale-mount`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum OnStaleMount {
    Refuse,
    Hold,
    Recover,
}

/// Why `--on-stale-mount recover` cannot stack over a stale mount here.
fn recover_refusal(args: &MountArgs) -> Option<&'static str> {
    if args.history.history_service.is_some() {
        // A front's mount is the CSI driver's to stack (design §14.2).
        Some("fronts (--history-service) do not recover in place")
    } else if !agentfs_fuse::privileges::Privileges::current().can_unmount() {
        Some("stacking a mount needs CAP_SYS_ADMIN (run as root)")
    } else {
        None
    }
}

fn parse_passthrough(value: &str) -> Result<Passthrough, String> {
    value.parse()
}

/// `agentfs probe`: one line, exit 0 for a live agentfs mount (or a stale
/// one with `allow_stale`), else 1.
fn probe(mountpoint: &Path, allow_stale: bool) -> ExitCode {
    match mount_state(mountpoint) {
        Ok(state) => {
            println!("{}", describe_state(&state));
            let ok = match state {
                MountState::Live { .. } => true,
                MountState::Stale { .. } => allow_stale,
                MountState::NotMounted | MountState::Foreign { .. } => false,
            };
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            println!("cannot check {}: {e}", mountpoint.display());
            ExitCode::FAILURE
        }
    }
}

fn describe_state(state: &MountState) -> String {
    match state {
        MountState::Live { source } => format!("live agentfs mount ({source})"),
        MountState::Stale { .. } => "stale agentfs mount (daemon gone)".to_owned(),
        MountState::NotMounted => "not mounted".to_owned(),
        MountState::Foreign { fstype } => format!("not an agentfs mount ({fstype})"),
    }
}

/// Creates a missing mount point (one level: its parent must exist), so a
/// sidecar can mount into a subdirectory of a fresh emptyDir.
fn create_mountpoint(mountpoint: &Path) -> Result<()> {
    match std::fs::symlink_metadata(mountpoint) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .mode(0o755)
                .create(mountpoint)
                .with_context(|| format!("creating the mount point {}", mountpoint.display()))?;
            eprintln!("created the mount point {}", mountpoint.display());
            Ok(())
        }
        // Anything else (it exists, or is stale) is preflight's to judge.
        _ => Ok(()),
    }
}

/// `--seal-mountpoint`: makes the bare mount point root's with mode 0000
/// (design §17.1). The mount on top is unaffected; what it hides is a
/// directory no non-root process (nor root without `CAP_DAC_OVERRIDE`) can
/// write, so a workload that binds it while no daemon is mounted (a
/// `subPath` bound while the sidecar restarts) gets EACCES instead of
/// writing where nothing records it.
fn seal_mountpoint(mountpoint: &Path) -> Result<()> {
    if !rustix::process::geteuid().is_root() {
        bail!(
            "--seal-mountpoint (AGENTFS_SEAL_MOUNTPOINT) needs root: the sealed mount point is \
             owned by root"
        );
    }
    std::os::unix::fs::chown(mountpoint, Some(0), Some(0))
        .and_then(|()| std::fs::set_permissions(mountpoint, std::fs::Permissions::from_mode(0o000)))
        .with_context(|| format!("sealing the mount point {}", mountpoint.display()))?;
    Ok(())
}

/// `agentfs mount`: the FUSE filesystem, the commit loop and the push loops
/// in one process, until SIGINT, SIGTERM or an unmount.
fn mount(
    daemon: Daemon,
    state: &StateDir,
    root: Option<&Path>,
    args: &MountArgs,
    stacked: bool,
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
    let (backing, mountpoint, stale_layers) = if stacked {
        let (backing, mountpoint) =
            preflight_stacked(state, &settings, &args.backing, &args.mountpoint)?;
        let layers = agentfs_layers(&mountpoint).unwrap_or(1);
        (backing, mountpoint, Some(layers))
    } else {
        create_mountpoint(&args.mountpoint)?;
        let (backing, mountpoint) = preflight(state, &settings, &args.backing, &args.mountpoint)?;
        if std::fs::read_dir(&mountpoint).is_ok_and(|mut d| d.next().is_some()) {
            eprintln!(
                "warning: {} is not empty: the files in it are hidden by the mount and not \
                 recorded (a workload wrote there while no daemon was mounted?)",
                mountpoint.display()
            );
        }
        if args.seal_mountpoint {
            seal_mountpoint(&mountpoint)?;
        }
        (backing, mountpoint, None)
    };
    let events = session.events.clone();
    let (recorder, committer, recovery) = session.open(state, &backing, &args.pod)?;
    // The patch queue, before anything can see the tree (design §12). A
    // blocked queue is an alarm; the volume mounts anyway, and so it does
    // when the queue cannot run at all (apply_queue raised the alarm and
    // recorded the failure).
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
    let restorer = recorder.clone();
    let pushers = push::open_all(state, &settings, events.clone(), session.telemetry())?;
    let names: Vec<String> = pushers.iter().map(|p| p.name().to_owned()).collect();
    let service = Service::start(committer, pushers, events).context("starting the service")?;
    let config = MountConfig {
        volume: settings.volume.clone(),
        backing: backing.clone(),
        mountpoint: mountpoint.clone(),
        passthrough: args.passthrough,
        allow_other: !args.no_allow_other,
        threads: args.threads,
        sweep_interval: settings.sweep_interval,
        repair_store: Some(state.repairs()),
        stack_over: stacked.then(|| StackOver {
            source: state.stack_source(),
        }),
    };
    let mounted = match Mount::start(recorder, config) {
        Ok(mounted) => mounted,
        Err(e) => {
            // Commits what recovery found; nothing was mounted.
            let _ = service.shutdown(args.push_grace);
            session.finish();
            return Err(e.into());
        }
    };
    // Restores and reverts through the mount (design §15). Without the
    // socket the mount still works; restores then need it stopped.
    // `agentfs push` through it too (design §2.3).
    let control = start_control(
        state,
        push_now::handler(
            service.handle(),
            misconfigured(&settings),
            push_now::Restores::Serve(recovery::Daemon {
                state: state.clone(),
                recorder: restorer,
                view: mounted.view(),
                commit_window: settings.commit_window,
            }),
        ),
    );
    daemon.watch(mounted.ended(), Wake::Unmounted)?;
    daemon.watch(mounted.failed(), Wake::PassthroughFailed)?;
    let startup = mounted.startup_repairs();
    if startup.loaded > 0 {
        eprintln!(
            "{} path(s) a previous mount could not record: {} repaired from the backing tree, \
             {} still failing",
            startup.loaded, startup.repaired, startup.still_queued
        );
    }
    if let Some(layers) = stale_layers {
        eprintln!(
            "stacked over a stale mount left by a crashed daemon at {} ({layers} stale \
             layer(s) below): the workload's copies of the mount now reach this daemon",
            mountpoint.display()
        );
    }
    eprintln!(
        "mounted {} at {} (volume {}, passthrough: {}, threads {}, {} uncommitted record(s) \
         recovered, remotes: {}); stop with SIGINT or SIGTERM",
        backing.display(),
        mountpoint.display(),
        settings.volume,
        mounted.passthrough(),
        mounted.threads(),
        recovery.uncommitted,
        list(&names)
    );

    let wake = daemon.wait();
    let failed = wake == Wake::PassthroughFailed;
    if failed {
        eprintln!("passthrough: {}", mounted.passthrough());
    }
    eprintln!(
        "{}: unmounting (waiting up to {:?} for open files, then {:?} for the final push; signal \
         again to exit now)",
        describe_wake(wake),
        args.unmount_grace,
        args.push_grace
    );
    // Before the unmount: a restore writes through the mount. Push
    // requests waiting on the service return first.
    service.handle().close();
    if let Some(control) = control
        && !control.shutdown(args.unmount_grace)
    {
        eprintln!("a restore was still running at shutdown; it stops with the unmount");
    }
    let report = mounted.shutdown(args.unmount_grace);
    print_unmount(&report, args.unmount_grace);
    // The layers earlier crashes left under a stacked mount.
    match detach_stale_layers(&mountpoint, STALE_LAYERS) {
        Ok(0) => {}
        Ok(n) => eprintln!("detached {n} stale layer(s) at {}", mountpoint.display()),
        Err(e) => eprintln!(
            "could not detach the stale layers at {}: {e}",
            mountpoint.display()
        ),
    }
    let code = stop_service(service, args.push_grace);
    session.finish();
    Ok(if failed { ExitCode::FAILURE } else { code })
}

/// Reports what an unmount did and what it could not record.
fn print_unmount(report: &UnmountReport, unmount_grace: Duration) {
    if let Some(error) = &report.unmount_error {
        eprintln!("unmount failed: {error}");
    }
    if report.lazy {
        eprintln!("the mount was busy: detached lazily");
    }
    if report.session_abandoned {
        eprintln!(
            "files were still open after {:?}: their later changes are not recorded",
            unmount_grace
        );
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
type Opened = (volume::Recorder, volume::Committer, volume::Recovery);

fn volume_name(state: &StateDir) -> Result<String> {
    Ok(History::open(&state.git())
        .with_context(|| format!("opening {}", state.root().display()))?
        .settings()
        .volume
        .clone())
}

/// Telemetry and alarms for one command on an existing volume (design
/// §13). Telemetry is flushed when this is dropped, waiting at most
/// [`otel::TELEMETRY_SHUTDOWN_GRACE`]; declare it before anything that
/// records, so it is dropped after them.
struct Session {
    volume: String,
    otel: otel::Otel,
    /// Alarms to telemetry, then to stderr: the process's one wrapped sink.
    events: Events,
}

impl Session {
    fn start(state: &StateDir, pod: &Pod) -> Result<Self> {
        let volume = volume_name(state)?;
        let otel = otel::init(&volume, &pod.context(&volume));
        let events = otel.telemetry().sink(stderr_sink());
        Ok(Self {
            volume,
            otel,
            events,
        })
    }

    fn telemetry(&self) -> &Telemetry {
        self.otel.telemetry()
    }

    /// Flushes telemetry now, while the volume is still open, so the last
    /// export still reads its gauges. Dropping the session without this
    /// (an error return) flushes too, after the volume is closed.
    fn finish(mut self) {
        self.otel.shutdown(otel::TELEMETRY_SHUTDOWN_GRACE);
    }

    /// Opens the volume and registers its state gauges. Once per process.
    fn open(&self, state: &StateDir, root: &Path, pod: &Pod) -> Result<Opened> {
        let opened = volume::open(
            state,
            root,
            pod.context(&self.volume),
            self.events.clone(),
            self.telemetry(),
        )?;
        self.telemetry()
            .observe_volume(state, opened.1.history().settings());
        Ok(opened)
    }
}

/// Records the current state of each of `paths` under `root`, printing
/// what was recorded and skipped.
fn record_paths(recorder: &volume::Recorder, root: &Path, paths: &[String]) -> Result<()> {
    let caller = Caller {
        uid: current_uid(),
        gid: current_gid(),
        pid: std::process::id(),
    };
    let mut groups_done = std::collections::HashSet::new();
    for path in paths {
        // A path in the fsync class records its whole group as one
        // record (design §8).
        if let Ok(repo_path) = RepoPath::new(path.as_str())
            && let Some(group) = recorder.fsync_group(&repo_path)
        {
            if !groups_done.insert(group.key.clone()) {
                continue;
            }
            let mut inputs = Vec::with_capacity(group.members.len());
            for member in &group.members {
                match input_for(root, member.as_str()) {
                    Ok(input) => inputs.push(input),
                    // A directory at a member's name is no member.
                    Err(_) if *member != repo_path => {}
                    Err(e) => return Err(e),
                }
            }
            let names: Vec<&str> = group.members.iter().map(RepoPath::as_str).collect();
            let recorded = recorder.record_group(Trigger::Close, caller, inputs)?;
            if let Some(seq) = recorded.seq {
                println!("recorded group {} as seq {seq}", names.join(", "));
            }
            for skipped in recorded.skipped {
                println!("skipped {}: {}", skipped.path, skipped.reason);
            }
            continue;
        }
        let input = match input_for(root, path) {
            Ok(input) => input,
            Err(e) => match e.downcast_ref::<PathError>() {
                Some(err @ PathError::DotGit(_)) => {
                    recorder.refuse_invalid(path, err.clone());
                    println!("skipped {path}: {e}");
                    continue;
                }
                _ => return Err(e),
            },
        };
        let trigger = match input {
            Input::Remove { .. } => Trigger::Unlink,
            Input::Symlink { .. } => Trigger::Symlink,
            Input::Write { .. } => Trigger::Close,
        };
        let recorded = recorder.record(trigger, caller, vec![input])?;
        if let Some(seq) = recorded.seq {
            println!("recorded {path} as seq {seq}");
        }
        for skipped in recorded.skipped {
            println!("skipped {}: {}", skipped.path, skipped.reason);
        }
    }
    Ok(())
}
/// Prints alarms (design §13) to stderr, alongside any telemetry export.
fn stderr_sink() -> Events {
    Arc::new(|event: Event| {
        if event.is_alarm() {
            eprintln!("alarm: {event}");
        } else if let Event::HistoryServiceRestored { .. } = event {
            eprintln!("{event}");
        }
    })
}

fn input_for(root: &Path, path: &str) -> Result<Input> {
    let repo_path = RepoPath::new(path)?;
    let backing = root.join(path);
    let meta = match std::fs::symlink_metadata(&backing) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Input::Remove { path: repo_path });
        }
        Err(e) => return Err(e).with_context(|| backing.display().to_string()),
    };
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(&backing)?;
        let Some(target) = target.to_str() else {
            bail!("symlink target of {path} is not UTF-8");
        };
        return Ok(Input::Symlink {
            path: repo_path,
            target: target.to_owned(),
        });
    }
    if !meta.is_file() {
        bail!("{path} is not a regular file or symlink");
    }
    Ok(Input::Write {
        path: repo_path,
        backing,
        executable: meta.permissions().mode() & 0o111 != 0,
    })
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_owned())
        .unwrap_or_else(|_| "localhost".into())
}

fn current_uid() -> u32 {
    std::fs::metadata("/proc/self").map_or(0, |m| std::os::unix::fs::MetadataExt::uid(&m))
}

fn current_gid() -> u32 {
    std::fs::metadata("/proc/self").map_or(0, |m| std::os::unix::fs::MetadataExt::gid(&m))
}
