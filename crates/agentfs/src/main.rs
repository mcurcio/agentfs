//! The `agentfs` command.
//!
//! `mount` mounts a backing tree through the FUSE layer (`agentfs_fuse`)
//! and runs the commit and push loops in the same process until SIGINT,
//! SIGTERM or an unmount (design §17.1). `record` records paths from a
//! directory directly, through the same recorder, including its path rules:
//! paths the rules exclude are reported as skipped, with the reason.
//!
//! `push` pushes once to the configured remotes; `run` runs the commit and
//! push loops in the foreground until SIGINT or SIGTERM (design §11.2).
//!
//! `restore` and `revert` write content from history back into the tree,
//! recorded as one commit (design §15; [`recovery`]).
//!
//! `mount`, `run`, `record`, `flush`, `push`, and `restore` and `revert`
//! when no mount runs, take the state-area lock (design §2.3) and exit 75
//! while another process holds it; `status` is read-only and takes no lock.
//!
//! `mount`, `flush`, `record`, `push` and `run` export telemetry over OTLP
//! when the standard `OTEL_*` variables configure an endpoint (see
//! [`otel`], design §13); alarms are also printed to stderr. `init` and
//! `status` export nothing.

mod control;
mod daemon;
mod otel;
mod recovery;

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Parser, Subcommand};

use agentfs_core::config::parse_duration;
use agentfs_core::events::{Event, Events};
use agentfs_core::history::History;
use agentfs_core::identity::Context;
use agentfs_core::lock::{LockError, StateLock};
use agentfs_core::paths::{PathError, RepoPath};
use agentfs_core::push::{self, PushOutcome, RemotePusher, RemoteStatus};
use agentfs_core::record::{Caller, Trigger};
use agentfs_core::service::Service;
use agentfs_core::telemetry::Telemetry;
use agentfs_core::volume::{self, Input, StateDir};
use agentfs_fuse::{Mount, MountConfig, Passthrough, preflight};

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
    /// Record the current state of PATHs under ROOT, then commit.
    Record {
        /// Paths relative to ROOT. A missing path is recorded as removed.
        #[arg(required = true)]
        paths: Vec<String>,
        #[command(flatten)]
        pod: Pod,
    },
    /// Push the volume's refs to its remotes once. Exits 1 unless every
    /// selected remote succeeded.
    Push {
        /// Push only this remote; repeatable (default: every remote).
        #[arg(long = "remote", value_name = "NAME")]
        remotes: Vec<String>,
        #[command(flatten)]
        pod: Pod,
    },
    /// Run the commit and push loops until SIGINT or SIGTERM. A second
    /// signal exits at once.
    Run {
        #[command(flatten)]
        pod: Pod,
        /// How long shutdown waits for pushes in progress.
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
            match StateLock::holder(&state)? {
                Some(holder) => println!("lock:      held by {holder}"),
                None => println!("lock:      free"),
            }
            for remote in push::status(&state, history.settings())? {
                println!("{}", remote_line(&remote));
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
        Command::Record { paths, pod } => {
            // Guessing the root would turn every path missing from the wrong
            // directory into a recorded removal.
            let Some(root) = cli.root.as_deref() else {
                bail!("record needs --root (or AGENTFS_ROOT): the backing tree the paths are in");
            };
            let _lock = StateLock::acquire(&state, "record", cli.wait_lock)?;
            let session = Session::start(&state, &pod)?;
            let (recorder, committer, _) = session.open(&state, root, &pod)?;
            let caller = Caller {
                uid: current_uid(),
                gid: current_gid(),
                pid: std::process::id(),
            };
            for path in paths {
                let input = match input_for(root, &path) {
                    Ok(input) => input,
                    Err(e) => match e.downcast_ref::<PathError>() {
                        Some(err @ PathError::DotGit(_)) => {
                            recorder.refuse_invalid(&path, err.clone());
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
            let report = committer.commit_pending()?;
            println!(
                "{} record(s) into {} commit(s)",
                report.records, report.commits
            );
            session.finish();
        }
        Command::Push { remotes, pod } => {
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
        Command::Mount(args) => {
            let daemon = Daemon::start()?;
            let _lock = StateLock::acquire(&state, "mount", cli.wait_lock)?;
            return mount(daemon, &state, cli.root.as_deref(), &args);
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
        } => format!("failed ({kind}): {message}; retry in {retry_in:?}"),
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
        line.push_str(&format!(", last error ({}): {}", error.kind, error.message));
    }
    if let Some(diverged) = &state.diverged {
        match &diverged.diverged_ref {
            Some(saved) => line.push_str(&format!(", diverged: head saved as {saved}")),
            None => line.push_str(", diverged: head not saved yet"),
        }
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
    eprintln!(
        "running volume {} ({} uncommitted record(s) recovered; remotes: {}); \
         stop with SIGINT or SIGTERM",
        settings.volume,
        recovery.uncommitted,
        list(&names)
    );

    let wake = daemon.wait();
    eprintln!(
        "{}: stopping (waiting up to {push_grace:?} for pushes; signal again to exit now)",
        describe_wake(wake)
    );
    let code = stop_service(service, push_grace);
    session.finish();
    Ok(code)
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

/// Stops the service: the final commit, then pushes in progress for up to
/// `push_grace`. Exit status 1 if the final commit failed.
fn stop_service(service: Service, push_grace: Duration) -> ExitCode {
    let report = service.shutdown(push_grace);
    for name in &report.abandoned_pushes {
        println!("{name}: push still running at shutdown; abandoned");
    }
    match report.final_commit {
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
    /// How long shutdown waits for pushes in progress.
    #[arg(long, default_value = "5s", value_parser = parse_grace)]
    push_grace: Duration,
    /// How long shutdown waits for a busy mount (files still open) to be
    /// released after it is detached.
    #[arg(long, default_value = "10s", value_parser = parse_grace)]
    unmount_grace: Duration,
}

fn parse_passthrough(value: &str) -> Result<Passthrough, String> {
    value.parse()
}

/// `agentfs mount`: the FUSE filesystem, the commit loop and the push loops
/// in one process, until SIGINT, SIGTERM or an unmount.
fn mount(
    daemon: Daemon,
    state: &StateDir,
    root: Option<&Path>,
    args: &MountArgs,
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
    let (backing, mountpoint) = preflight(state, &settings, &args.backing, &args.mountpoint)?;
    let events = session.events.clone();
    let (recorder, committer, recovery) = session.open(state, &backing, &args.pod)?;
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
    let control = {
        let handler = recovery::Daemon {
            state: state.clone(),
            recorder: restorer,
            view: mounted.view(),
            commit_window: settings.commit_window,
        };
        let handler: control::Handler = Arc::new(move |request, peer| handler.serve(request, peer));
        match control::Server::start(&state.control_dir(), handler) {
            Ok(server) => Some(server),
            Err(e) => {
                eprintln!(
                    "warning: control socket in {}: {e}; restore and revert need the mount \
                     stopped",
                    state.control_dir().display()
                );
                None
            }
        }
    };
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
        "{}: unmounting (waiting up to {:?} for open files, then {:?} for pushes; signal \
         again to exit now)",
        describe_wake(wake),
        args.unmount_grace,
        args.push_grace
    );
    // Before the unmount: a restore writes through the mount.
    if let Some(control) = control
        && !control.shutdown(args.unmount_grace)
    {
        eprintln!("a restore was still running at shutdown; it stops with the unmount");
    }
    let report = mounted.shutdown(args.unmount_grace);
    if let Some(error) = &report.unmount_error {
        eprintln!("unmount failed: {error}");
    }
    if report.lazy {
        eprintln!("the mount was busy: detached lazily");
    }
    if report.session_abandoned {
        eprintln!(
            "files were still open after {:?}: their later changes are not recorded",
            args.unmount_grace
        );
    }
    if report.swept > 0 || report.repaired > 0 {
        println!(
            "final sweep: {} open file(s) recorded, {} failed record(s) repaired",
            report.swept, report.repaired
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
    let code = stop_service(service, args.push_grace);
    session.finish();
    Ok(if failed { ExitCode::FAILURE } else { code })
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

/// Prints alarms (design §13) to stderr, alongside any telemetry export.
fn stderr_sink() -> Events {
    Arc::new(|event: Event| {
        if event.is_alarm() {
            eprintln!("alarm: {event}");
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
