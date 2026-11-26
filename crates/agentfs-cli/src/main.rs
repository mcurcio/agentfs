//! The `agentfs` command.
//!
//! `init`, `mount`, `fork end` and `verify` run the tree engine ([`launcher`],
//! docs/trees.md): `init` creates or finds a volume, `mount` serves its root
//! (and optionally a fork) as tree views until SIGINT, SIGTERM, its command's
//! exit or an unmount, `fork end` ends a keyed fork, and `verify` checks the
//! saves history flagged unverified again (its exit status:
//! `launcher/verify.rs`). `probe` reports
//! whether a mount point holds a live agentfs mount, for Kubernetes probes;
//! `mount --on-stale-mount recover` makes a sidecar restarted after a crash
//! mount over the stale mount its predecessor left, and `hold` makes it wait
//! for a pod restart instead ([`hold`]).
//!
//! Every other command runs the recording engine, which the CSI driver
//! runs, and refuses a tree volume before it reads or writes anything
//! ([`refuse_tree_volume`]). `record` records paths from a directory through
//! the recorder, including its path rules: paths the rules exclude are
//! reported as skipped, with the reason.
//!
//! `push` pushes once to the configured remotes, or asks a running daemon
//! (`run`, `serve-history`) to commit and push now over its control socket
//! ([`push_now`]); `run` runs the commit and push loops in the foreground
//! until SIGINT or SIGTERM.
//!
//! `restore` and `revert` write content from history back into the tree,
//! recorded as one commit ([`recovery`]).
//!
//! `serve-history` is a volume's history service and `front` and `record
//! --history-service` are its fronts, one per pod ([`serve_history`],
//! [`front`]): the service is the only writer of the journal and branch, and
//! each front records its pod's changes through it, carrying its pod's
//! committer. Only the CSI node plugin starts `front`.
//!
//! `run`, `record`, `flush`, `serve-history`, and `push`, `restore` and
//! `revert` when no daemon answers, take the state-area lock and exit 75
//! while another process holds it; fronts take no lock; `status` is
//! read-only and takes no lock.
//!
//! `fork create` makes a fork of a volume, `fork finish` finishes and
//! publishes it, `fork status` reads its state, `fork abandon` discards it
//! on record and `fork list` lists the published changes on a fork remote
//! ([`fork_cmd`]). A finished fork is read-only: `run`, `record`,
//! `restore`, `revert` and `patch apply` refuse it.
//!
//! `serve-git` serves the volume's history repository to pushers over
//! git's smart HTTP, as the volume's patch source ([`serve_git`], bead
//! agentfs-mia.9): it takes no lock, and its hooks judge each push.
//!
//! `start accept --tip OID` accepts a tip of the branch a volume started
//! from, clearing the start-branch alarm ([`start_cmd`]); it
//! takes no lock.
//!
//! `mount`, `front`, `flush`, `record`, `push` and `run` export telemetry
//! over OTLP when the standard `OTEL_*` variables configure an endpoint
//! (see [`otel`]); alarms are also printed to stderr. `init` and `status`
//! export nothing.

mod checkpoint_cmd;
mod control;
mod daemon;
mod fork_cmd;
mod front;
mod hold;
mod launcher;
mod lfs_cmd;
mod mountpoint;
mod otel;
mod patch_cmd;
mod pod_args;
mod push_now;
mod recovery;
#[cfg(test)]
mod reference_tests;
mod serve_git;
mod serve_history;
mod start_cmd;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Parser, Subcommand};

use agentfs_contracts::paths::{PathError, RepoPath};
use agentfs_contracts::units::duration_arg;

use agentfs_core::config::Remotes;
use agentfs_core::events::{Event, Events, PushStage};
use agentfs_core::history::History;
use agentfs_core::identity::Context;
use agentfs_core::lfs;
use agentfs_core::lock::{LockError, StateLock};
use agentfs_core::push::{self, LeaseOutcome, PushOutcome, RemotePusher, RemoteStatus};
use agentfs_core::record::{Caller, Trigger};
use agentfs_core::service::{FinalPush, Service};
use agentfs_core::telemetry::Telemetry;
use agentfs_core::volume::{self, Input, StateDir};
use agentfs_fuse::stale::{MountState, mount_state};

use crate::daemon::{Daemon, Wake};

/// `--version`: the package version, with `+failpoints` in a test build
/// with failpoints compiled in, so such a binary is
/// never mistaken for a release.
#[cfg(not(feature = "failpoints"))]
const VERSION: &str = env!("CARGO_PKG_VERSION");
#[cfg(feature = "failpoints")]
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+failpoints");

#[derive(Parser)]
#[command(
    name = env!("CARGO_BIN_NAME"),
    version = VERSION,
    about = "Records every change an agent makes, in git history"
)]
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
    /// area (every command that writes it takes it). By
    /// default a busy state area exits 75 at once.
    #[arg(long, global = true, env = "AGENTFS_WAIT_LOCK", default_value = "0s", value_parser = duration_arg, value_name = "DURATION")]
    wait_lock: Duration,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the tree volume, or find it (same name: nothing written), then
    /// recover it. Prints `<name> <id> created|found`; exit 1 refused, 75
    /// busy.
    Init(launcher::InitArgs),
    /// Stage the tree volume and mount its root, optionally also a fork, and
    /// optionally run a command in it. SIGTERM, SIGINT, the command's exit
    /// or unmounting every view drains it. Never creates a volume.
    Mount(launcher::MountArgs),
    /// Check every save history flagged unverified again with this binary,
    /// one line each. Exit 0 when no flag stands, 2 when one does, 1 on an
    /// error, 75 while another process holds the volume.
    Verify(launcher::VerifyArgs),
    /// Serve one tree volume's views on its launcher's requests, read from
    /// the standard input (a Unix socket the launcher holds the other end
    /// of). Its EOF, SIGTERM or SIGINT drains it. Exit 1 refused, 2 not run
    /// by a launcher or told a malformed line, 75 busy.
    Host(launcher::HostArgs),
    /// A front of a history service (the CSI node plugin starts it): mount
    /// BACKING at MOUNTPOINT and record every change through the service,
    /// until SIGINT, SIGTERM or an unmount. A second signal exits at once.
    Front(front::FrontArgs),
    /// Show the branch head, the last committed sequence and the backlog.
    Status(Pod),
    /// Commit every recorded change now.
    Flush(Pod),
    /// Record the current state of PATHs under ROOT, then commit. With
    /// --history-service, record through the history service instead,
    /// which commits them.
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
    /// Serve the volume's history to fronts (`front`)
    /// until SIGINT or SIGTERM: the only writer of the journal and the
    /// branch, with the commit and push loops. Needs --root,
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
        #[arg(long, default_value = "5s", value_parser = duration_arg)]
        push_grace: Duration,
        /// How long shutdown waits for fronts' requests in progress.
        #[arg(long, default_value = "10s", value_parser = duration_arg)]
        shutdown_grace: Duration,
        /// A workload may be attached to the volume (the CSI node plugin
        /// passes it while a pod of the volume is published): the patch
        /// queue is judged at start, not applied.
        #[arg(long)]
        workload_attached: bool,
    },
    /// Serve the volume's history repository to pushers over git's smart
    /// HTTP as its patch source, until SIGINT or SIGTERM: pushers push
    /// patches (`patch submit --remote http://NAME:TOKEN@ADDR/`) and
    /// approvals by their rights (`[agentfsPusher "<name>"]`); each push is
    /// admitted whole or refused, then judged and notified, never landed.
    /// Needs agentfs.patchRemote = none. Takes no lock.
    ServeGit {
        /// Where to listen: HOST:PORT (port 0 picks one; the address is
        /// printed). Required.
        #[arg(long, value_name = "ADDR")]
        listen: Option<String>,
        #[command(flatten)]
        pod: Pod,
        #[command(subcommand)]
        hook: Option<ServeGitHook>,
    },
    /// Push the volume's refs to its remotes once. With a daemon running
    /// (`run`, `serve-history`), asks it to commit and push now
    /// over its control socket; otherwise pushes directly. Exits 1 unless
    /// every selected remote succeeded. `--force-with-lease` always pushes
    /// directly, one remote only.
    Push {
        /// Push only this remote; repeatable (default: every remote).
        #[arg(long = "remote", value_name = "NAME")]
        remotes: Vec<String>,
        /// How long to wait for a running daemon's push (1s to 10m); a push
        /// still running then carries on in the background. Pushing
        /// directly ignores it.
        #[arg(long, default_value = "120s", value_parser = duration_arg, value_name = "DURATION")]
        timeout: Duration,
        /// Replace this volume's own branch on one remote, leased on OID (runbooks only).
        #[arg(long, value_name = "OID")]
        force_with_lease: Option<String>,
        #[command(flatten)]
        pod: Pod,
    },
    /// Cut a checkpoint: one commit on the change line
    /// (refs/agentfs/<volume>/change-line) with the branch head's tree and
    /// the patches landed since, merged back into the branch. With a daemon
    /// running, its commit loop cuts it; otherwise takes the state-area lock
    /// (exit 75 when it is busy) and commits what is recorded first. Exit 0
    /// also when there is nothing new.
    Checkpoint {
        /// The checkpoint's message (default: `checkpoint`).
        #[arg(short = 'm', long = "message")]
        message: Option<String>,
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
        #[arg(long, default_value = "5s", value_parser = duration_arg)]
        push_grace: Duration,
    },
    /// Write PATH (a file, symlink or directory, or `.` for the whole
    /// tree) back as it was in COMMIT, removing what COMMIT did not have
    /// there; recorded as one commit. Goes through a running `run` or
    /// history service when there is one; otherwise writes --root
    /// directly.
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
    /// commit. Through a running `run` or history service when
    /// there is one; otherwise writes --root directly.
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
    /// Report what is mounted at MOUNTPOINT: a live agentfs mount (exit 0),
    /// a stale one whose daemon died, nothing, or another filesystem (exit
    /// 1). Takes no lock and reads no state area: for startup and readiness
    /// probes.
    Probe {
        mountpoint: PathBuf,
        /// Exit 0 for a stale agentfs mount too (a sidecar holding after a
        /// crash).
        #[arg(long)]
        allow_stale: bool,
    },
    /// The patch queue: submit a patch for a volume, apply
    /// the queue now, or show it.
    Patch {
        #[command(subcommand)]
        command: patch_cmd::PatchCommand,
    },
    /// The local Git LFS object cache: fetch the objects
    /// commits reference, or prune those every remote holds.
    Lfs {
        #[command(subcommand)]
        command: lfs_cmd::LfsCommand,
    },
    /// Forked volumes: create a fork; finish and
    /// publish it.
    Fork {
        #[command(subcommand)]
        command: fork_cmd::ForkCommand,
    },
    /// A volume started from another branch (F8): accept a
    /// tip of that branch, which clears the start-branch alarm.
    Start {
        #[command(subcommand)]
        command: start_cmd::StartCommand,
    },
}

/// `serve-git`'s git hooks, run by `git receive-pack` (bead agentfs-mia.9).
#[derive(Subcommand)]
enum ServeGitHook {
    /// `pre-receive`: admit the whole push, or refuse it.
    #[command(hide = true)]
    Admit,
    /// `post-receive`: judge the queue and notify; never land.
    #[command(hide = true)]
    Arrived,
}

/// Being a front of a history service.
#[derive(Args)]
struct HistoryArgs {
    /// Record through the history service at ADDR (`unix:<path>` or an
    /// absolute socket path) as a front: take no state-area lock, freeze
    /// into a leased pending slot, and let the service journal and commit.
    #[arg(long, env = "AGENTFS_HISTORY_SERVICE", value_name = "ADDR")]
    history_service: Option<String>,
    /// How long a boundary waits for the history service before it fails
    /// with EIO.
    #[arg(long, env = "AGENTFS_HISTORY_TIMEOUT", default_value = "10s", value_parser = duration_arg, value_name = "DURATION")]
    history_timeout: Duration,
    /// How long to wait at start for the history service to be up.
    #[arg(long, env = "AGENTFS_HISTORY_WAIT", default_value = "30s", value_parser = duration_arg, value_name = "DURATION")]
    history_wait: Duration,
    /// Prefer pending slot N (1..=1023) when leasing as a front, else the
    /// lowest free slot. A front restarted after a crash passes its
    /// predecessor's slot so it drains that slot's repairs (the CSI
    /// driver does this).
    #[arg(long, env = "AGENTFS_HISTORY_SLOT", value_name = "N", value_parser = clap::value_parser!(u16).range(1..=1023))]
    history_slot: Option<u16>,
}

/// Where the volume is mounted. In Kubernetes these come from the downward
/// API.
#[derive(Args, Default)]
struct Pod {
    #[command(flatten)]
    args: pod_args::PodArgs,
    /// Committer identity's `{replica}` (default: the pod name, else the
    /// host name).
    #[arg(long, env = "AGENTFS_REPLICA")]
    replica: Option<String>,
}

impl Pod {
    /// `--replica`, else [`pod_args::PodArgs::replica`].
    fn replica(&self) -> String {
        self.replica
            .clone()
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| self.args.replica())
    }

    fn context(&self, volume: &str) -> Context {
        let replica = self.replica();
        Context {
            volume: volume.to_owned(),
            namespace: self.args.namespace.clone(),
            pod: self.args.pod.clone(),
            pod_uid: if self.args.pod_uid.is_empty() {
                replica.clone()
            } else {
                self.args.pod_uid.clone()
            },
            node: self.args.node.clone(),
            replica,
        }
    }
}

/// Exit status when another process holds the state area (`EX_TEMPFAIL`).
const EXIT_LOCKED: u8 = 75;

fn main() -> ExitCode {
    agentfs_os::allocator::limit_arenas();
    if cfg!(feature = "failpoints") && std::env::var_os(agentfs_failpoints::ENV_VAR).is_some() {
        eprintln!(
            "agentfs: warning: failpoints are set ({}): a test build, never for production",
            agentfs_failpoints::ENV_VAR
        );
    }
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
    if let Err(refused) = tree_command_flags(&cli) {
        eprintln!("error: {refused}");
        return Ok(ExitCode::FAILURE);
    }
    if let Some(command) = recording_command(&cli.command) {
        let source = match &cli.command {
            Command::Fork {
                command: fork_cmd::ForkCommand::Create(args),
            } => Some(args.source_state.as_path()),
            _ => None,
        };
        for state in std::iter::once(cli.state.as_path()).chain(source) {
            if let Some(refused) = refuse_tree_volume(state, command) {
                return Ok(refused);
            }
        }
    }
    // The composition root: the one place capabilities are minted.
    #[allow(
        clippy::disallowed_methods,
        reason = "the composition root mints capabilities"
    )]
    let minter = agentfs_contracts::caps::Minter::new();
    match cli.command {
        Command::Init(args) => return launcher::init(&cli.state, args, minter),
        Command::Mount(args) => return launcher::mount(&cli.state, args, minter),
        Command::Host(args) => return launcher::host(&cli.state, args, minter),
        Command::Verify(args) => return launcher::verify(&cli.state, args),
        Command::Status(_pod) => {
            // Read-only: no recovery, no lock, so it is safe beside a
            // running `mount` or `run`.
            let (history, inspection) = volume::inspect(&state)
                .with_context(|| format!("opening {}", state.root().display()))?;
            println!("volume:    {}", history.settings().volume);
            println!("branch:    {}", history.settings().branch_ref);
            if let Some(start) = &history.settings().start {
                // As the last push to the start remote saw it (F8 R9).
                println!("start:     {start}");
                let pushed = push::PushState::load(&state.push_state(&start.remote));
                if let Some(moved) = pushed.start_moved {
                    println!(
                        "start branch moved: {}..{} (agentfs start accept --tip {})",
                        moved.pinned, moved.tip, moved.tip
                    );
                }
            }
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
            println!("remotes:   {}", remotes_line(&settings.remotes));
            for remote in push::status(&state, settings)? {
                println!("{}", remote_line(&remote));
                // Where its LFS objects go: resolved from config, no network.
                if let Some(r) = settings
                    .remotes
                    .pushed()
                    .iter()
                    .find(|r| r.name == remote.name)
                {
                    match lfs::endpoint::resolve(settings, r, lfs::Direction::Upload) {
                        Ok(endpoint) => println!("           lfs: {}", endpoint.describe()),
                        Err(e) => println!("           lfs: none ({e})"),
                    }
                    if let Some(proxy) = &r.proxy {
                        let scope = if agentfs_core::proxy::git_uses_proxy(r.push_url()) {
                            "git and LFS"
                        } else {
                            "LFS only"
                        };
                        println!("           proxy: {proxy}, {scope}");
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
            refuse_finished(&state)?;
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
            workload_attached,
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
                workload_attached,
            );
        }
        Command::ServeGit { listen, pod, hook } => {
            return match hook {
                None => {
                    // Optional for clap only: the hooks run without it.
                    let Some(listen) = listen else {
                        bail!("serve-git needs --listen ADDR (HOST:PORT)");
                    };
                    serve_git::serve(
                        &state,
                        cli.root.as_deref(),
                        &listen,
                        &pod,
                        serve_git::SHUTDOWN_GRACE,
                    )
                }
                Some(ServeGitHook::Admit) => serve_git::admit_hook(&state),
                Some(ServeGitHook::Arrived) => serve_git::arrived_hook(&state, &pod),
            };
        }
        Command::Push {
            remotes,
            timeout,
            force_with_lease,
            pod,
        } => {
            // A lease push is always direct, under the remote's push lock
            // only: never through a daemon, never the state lock.
            if let Some(lease) = force_with_lease {
                let session = Session::start(&state, &pod)?;
                let code = push_with_lease(&state, &remotes, &lease, &session)?;
                session.finish();
                return Ok(code);
            }
            // Local-only: nothing is pushed, daemon or not.
            if remotes.is_empty()
                && History::open(&state.git())
                    .with_context(|| format!("opening {}", state.root().display()))?
                    .settings()
                    .remotes
                    == Remotes::LocalOnly
            {
                println!("{LOCAL_ONLY_PUSH}");
                return Ok(ExitCode::SUCCESS);
            }
            // A running daemon pushes for us.
            if let Some(code) = push_now::client(&state, &remotes, timeout) {
                return Ok(code);
            }
            let _lock = StateLock::acquire(&state, "push", cli.wait_lock)?;
            let session = Session::start(&state, &pod)?;
            let code = push_once(&state, &remotes, &session)?;
            session.finish();
            return Ok(code);
        }
        Command::Checkpoint { message, pod } => {
            return checkpoint_cmd::run(&state, cli.root.as_deref(), cli.wait_lock, &pod, message);
        }
        Command::Run { pod, push_grace } => {
            refuse_finished(&state)?;
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
            refuse_finished(&state)?;
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
            refuse_finished(&state)?;
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
        Command::Lfs { command } => return lfs_cmd::run(command, &state, cli.wait_lock),
        Command::Fork {
            command: fork_cmd::ForkCommand::End(args),
        } => return launcher::end_fork(&cli.state, args, minter),
        Command::Fork { command } => {
            return fork_cmd::run(command, &state, cli.root.as_deref(), cli.wait_lock);
        }
        Command::Start { command } => return start_cmd::run(command, &state),
        Command::Front(args) => {
            let daemon = Daemon::start()?;
            // A front takes no lock: the history service holds it. A front
            // of a finished fork mounts (the CSI node plugin binds it
            // read-only); its history service refuses every record.
            return front::mount_front(daemon, &state, cli.root.as_deref(), &args);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `init`, `mount`, `host`, `fork end` and `verify` (the tree engine's) take `--state` only:
/// a given `--root` (or `AGENTFS_ROOT`) or a non-zero `--wait-lock` is
/// refused, so an old manifest fails loudly.
fn tree_command_flags(cli: &Cli) -> Result<(), &'static str> {
    let tree = matches!(
        cli.command,
        Command::Init(_)
            | Command::Mount(_)
            | Command::Host(_)
            | Command::Verify(_)
            | Command::Fork {
                command: fork_cmd::ForkCommand::End(_)
            }
    );
    if tree && (cli.root.is_some() || !cli.wait_lock.is_zero()) {
        return Err(
            "init, mount, host, fork end and verify take --state only: a tree volume has no \
             backing tree (--root, AGENTFS_ROOT), and a busy state area exits 75",
        );
    }
    Ok(())
}

/// The name of a command that runs the recording engine only, as
/// [`refuse_tree_volume`] says it; `None` for the tree engine's commands,
/// `probe` and `serve-git`'s hooks. Every command is listed: a new one must
/// say which engine it runs.
fn recording_command(command: &Command) -> Option<&'static str> {
    match command {
        Command::Init(_)
        | Command::Mount(_)
        | Command::Host(_)
        | Command::Verify(_)
        | Command::Probe { .. } => None,
        Command::Fork {
            command: fork_cmd::ForkCommand::End(_),
        } => None,
        Command::Fork { .. } => Some("fork"),
        Command::ServeGit { hook: Some(_), .. } => None,
        Command::ServeGit { hook: None, .. } => Some("serve-git"),
        Command::Status(_) => Some("status"),
        Command::Flush(_) => Some("flush"),
        Command::Record { .. } => Some("record"),
        Command::ServeHistory { .. } => Some("serve-history"),
        Command::Front(_) => Some("front"),
        Command::Push { .. } => Some("push"),
        Command::Checkpoint { .. } => Some("checkpoint"),
        Command::Run { .. } => Some("run"),
        Command::Restore { .. } => Some("restore"),
        Command::Revert { .. } => Some("revert"),
        Command::Patch { .. } => Some("patch"),
        Command::Lfs { .. } => Some("lfs"),
        Command::Start { .. } => Some("start"),
    }
}

/// Refuses, before anything is read or written, a recording command on a
/// tree volume: prints "error: `agentfs <command>` runs on the recording
/// engine only; <state> is a tree volume, where it is not built yet
/// (docs/trees.md)" and answers exit 1. A state area carrying the tree
/// format's marker is refused whatever its identity file says, and one whose
/// marker cannot be read is refused too (exit 1, naming the error): the
/// guard fails closed. `None` when no tree volume is there.
fn refuse_tree_volume(state: &Path, command: &'static str) -> Option<ExitCode> {
    match agentfs_engine::tree_actor::holds_tree_format(state) {
        Ok(false) => None,
        Ok(true) => {
            eprintln!(
                "error: `agentfs {command}` runs on the recording engine only; {} is a tree \
                 volume, where it is not built yet (docs/trees.md)",
                state.display()
            );
            Some(ExitCode::FAILURE)
        }
        Err(e) => {
            eprintln!(
                "error: `agentfs {command}` runs on the recording engine only, and whether {} \
                 is a tree volume could not be read: {e}",
                state.display()
            );
            Some(ExitCode::FAILURE)
        }
    }
}

/// A finished fork is read-only: `mount`, `run`, `record`,
/// `restore`, `revert` and `patch apply` refuse it before they change
/// anything. Any other problem opening the state area is the command's own
/// to report.
fn refuse_finished(state: &StateDir) -> Result<()> {
    match volume::refuse_finished(state) {
        Err(e @ volume::VolumeError::ForkFinished { .. }) => Err(e.into()),
        _ => Ok(()),
    }
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
        if !settings.remotes.pushed().iter().any(|r| r.name == *name) {
            bail!("no remote named {name:?} is configured");
        }
    }
    match &settings.remotes {
        Remotes::LocalOnly => {
            println!("{LOCAL_ONLY_PUSH}");
            return Ok(ExitCode::SUCCESS);
        }
        Remotes::Pushed(_) | Remotes::NoneValid => {}
    }
    let selected: Vec<_> = settings
        .remotes
        .pushed()
        .iter()
        .filter(|r| only.is_empty() || only.contains(&r.name))
        .collect();
    if selected.is_empty() {
        return Ok(if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }
    for remote in selected {
        let mut pusher =
            RemotePusher::open(state, settings, remote, events.clone(), session.telemetry())?;
        // A backlog over the push size limit goes out in steps: each limited step is followed at once by the next.
        let mut outcome = pusher.push_once();
        while outcome.is_limited() {
            println!("{}: {}", remote.name, describe(&outcome));
            outcome = pusher.push_once();
        }
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

/// `agentfs push --force-with-lease OID` (the divergence and
/// history-rewrite runbooks): replaces this volume's own branch on exactly
/// one remote with the local head, if the remote holds `lease`, it shares
/// a commit with the head, and the commits it would drop are no
/// recorder's history the head lacks (`push::lease_claim`: divergence case
/// A, never case B). Direct, under the remote's push lock.
/// Exit 0 for pushed and up to date, 1 otherwise, 75 when the remote is
/// being pushed.
fn push_with_lease(
    state: &StateDir,
    only: &[String],
    lease: &str,
    session: &Session,
) -> Result<ExitCode> {
    let lease = agentfs_core::fork::identity::full_commit(lease)
        .with_context(|| format!("--force-with-lease: {lease:?} is not a full commit id"))?;
    let history = History::open(&state.git())
        .with_context(|| format!("opening {}", state.root().display()))?;
    let settings = history.settings();
    session.telemetry().observe_volume(state, settings);
    if let Some(bad) = settings
        .invalid_remotes
        .iter()
        .find(|bad| only.contains(&bad.name))
    {
        bail!("remote {:?} is misconfigured: {}", bad.name, bad.error);
    }
    let remote = match only {
        [name] => settings
            .remotes
            .pushed()
            .iter()
            .find(|r| r.name == *name)
            .with_context(|| format!("no remote named {name:?} is configured"))?,
        [] => match settings.remotes.pushed() {
            [remote] => remote,
            [] => bail!("--force-with-lease: no remote is configured"),
            _ => bail!("--force-with-lease needs --remote NAME: this volume has several remotes"),
        },
        _ => bail!("--force-with-lease replaces the branch on one remote: give one --remote"),
    };
    let mut pusher = RemotePusher::open(
        state,
        settings,
        remote,
        session.events.clone(),
        session.telemetry(),
    )?;
    let name = &remote.name;
    let ok = match pusher.push_with_lease(lease) {
        LeaseOutcome::Pushed { from, to } => {
            println!("{name}: replaced {from} with {to} (leased on {lease})");
            true
        }
        LeaseOutcome::UpToDate { head } => {
            println!("{name}: up to date at {head}");
            true
        }
        LeaseOutcome::NothingToPush => {
            println!("{name}: nothing to push: the branch has no commit yet");
            false
        }
        LeaseOutcome::LeaseBroken { found: now } => {
            println!(
                "{name}: lease broken: the remote holds {}, not {lease}; nothing was written",
                now.map_or_else(|| "no such branch".to_owned(), |o| o.to_string())
            );
            false
        }
        LeaseOutcome::Unrelated { lease, head } => {
            println!(
                "{name}: refused: the remote holds {lease}, which shares no commit with the head \
                 {head}; it is another history and is never replaced (continue under a new name)"
            );
            false
        }
        LeaseOutcome::Recorded { commit } => {
            println!(
                "{name}: refused: the remote's branch holds {commit}, which carries agentfs \
                 trailers and this state area lacks: this volume's newer history, another \
                 worktree's, or agentfs commits someone amended, rebased, cherry-picked or \
                 squashed; it is never replaced (divergence runbook: continue under a new name, \
                 or remove those commits from the branch by hand)"
            );
            false
        }
        LeaseOutcome::Failed { kind, message } => {
            println!("{name}: failed ({kind}): {message}");
            false
        }
        LeaseOutcome::Busy => {
            eprintln!("error: another process is pushing remote {name}; retry");
            return Ok(ExitCode::from(EXIT_LOCKED));
        }
    };
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
        PushOutcome::Pushed { from, to, limited } => {
            let from = from.map_or_else(|| "none".to_owned(), |f| f.to_string());
            match limited {
                None => format!("pushed {from}..{to}"),
                Some(left) => format!(
                    "pushed {from}..{to} (push size limit: {left} commit(s) left, pushing the \
                     next step)"
                ),
            }
        }
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
        PushOutcome::Cancelled => "cancelled at the push grace's end; nothing was sent".into(),
    }
}

/// What `agentfs push` prints for a local-only volume (no remote section).
const LOCAL_ONLY_PUSH: &str = "local-only: nothing to push";

/// `agentfs status`'s `remotes:` line: the volume's remotes mode.
fn remotes_line(remotes: &Remotes) -> String {
    match remotes {
        Remotes::Pushed(list) => format!("{} pushed", list.len()),
        Remotes::NoneValid => "none valid (every remote section is misconfigured)".into(),
        Remotes::LocalOnly => "local-only (no remote)".into(),
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
    let applier = patch_cmd::Applier::new(&committer, events.clone(), session.telemetry());
    let service = Service::start(committer, pushers, events).context("starting the service")?;
    // `agentfs push` asks it to push now; restores are refused.
    let control = start_control(
        state,
        push_now::handler(
            state.clone(),
            service.handle(),
            misconfigured(&settings),
            push_now::Restores::Refuse(push_now::RUN_SERVES_NO_RESTORES),
            applier,
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

/// Starts the control socket, or warns: the daemon works
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
    stop_control_within(service, control, push_now::CONTROL_GRACE);
}

/// Stops the control socket, waiting up to `grace` for the requests in
/// progress; whether they all finished.
fn stop_control_within(
    service: &Service,
    control: Option<control::Server>,
    grace: Duration,
) -> bool {
    service.handle().close();
    control.is_none_or(|control| control.shutdown(grace))
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
/// remotes lack, within `push_grace`. Exit status 1 if the
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
    for name in &report.abandoned_updates {
        println!(
            "final push: {name}: still updating the local remote {:?} after the push grace's \
             end; abandoned, and a ref lock may remain in the remote (runbook: stuck-pushes.md, \
             \"At shutdown\")",
            agentfs_core::service::LOCAL_UPDATE_CAP
        );
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
        MountState::Stale { source } => format!("stale agentfs mount ({source}, daemon gone)"),
        MountState::NotMounted => "not mounted".to_owned(),
        MountState::Foreign { fstype } => format!("not an agentfs mount ({fstype})"),
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

/// Telemetry and alarms for one command on an existing volume. Telemetry is flushed when this is dropped, waiting at most
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
        let otel = otel::init(&volume, &telemetry_identity(pod.context(&volume)));
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
        for lock in &opened.2.removed_ref_locks {
            eprintln!(
                "warning: removed {}, a ref lock a crash left in the history",
                lock.display()
            );
        }
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
        // record.
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
            Input::Write { .. } | Input::Tree { .. } => Trigger::Close,
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
/// Prints alarms to stderr, alongside any telemetry export.
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

/// A recording command's telemetry identity: its commit context's pod
/// fields, `pod_uid` already falling back to the replica.
fn telemetry_identity(context: Context) -> otel::Identity {
    otel::Identity {
        namespace: context.namespace,
        pod: context.pod,
        pod_uid: context.pod_uid,
        node: context.node,
        replica: context.replica,
    }
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

#[cfg(test)]
mod cli_tests {
    use super::{Cli, VERSION};
    use clap::CommandFactory;

    /// The package is `agentfs-cli`, but the command is `agentfs`: its name,
    /// its version line and its usage line, which clap would otherwise take
    /// from the package's name. The name's one home is the manifest's
    /// `[[bin]] name`, read through `CARGO_BIN_NAME`; this pins it.
    #[test]
    fn the_command_is_named_agentfs() {
        let mut cli = Cli::command();
        assert_eq!(cli.get_name(), "agentfs");
        assert_eq!(cli.render_version(), format!("agentfs {VERSION}\n"));
        let usage = cli.render_usage().to_string();
        assert!(usage.starts_with("Usage: agentfs "), "{usage}");
    }

    /// Clap stores a value by arg id, so a subcommand arg sharing an id with
    /// a global arg sends its value to the global one. No id may be shared.
    #[test]
    fn no_subcommand_arg_shares_a_global_id() {
        fn walk(cmd: &clap::Command, path: &str, globals: &[String], clashes: &mut Vec<String>) {
            for sub in cmd.get_subcommands() {
                let path = format!("{path} {}", sub.get_name());
                for arg in sub.get_arguments().filter(|a| !a.is_global_set()) {
                    if globals.iter().any(|g| g == arg.get_id().as_str()) {
                        clashes.push(format!("{path}: {}", arg.get_id()));
                    }
                }
                walk(sub, &path, globals, clashes);
            }
        }
        let cli = Cli::command();
        let globals: Vec<String> = cli
            .get_arguments()
            .filter(|a| a.is_global_set())
            .map(|a| a.get_id().to_string())
            .collect();
        assert!(globals.contains(&"root".to_string()));
        let mut clashes = Vec::new();
        walk(&cli, "agentfs", &globals, &mut clashes);
        assert!(clashes.is_empty(), "args sharing a global id: {clashes:?}");
    }
}
