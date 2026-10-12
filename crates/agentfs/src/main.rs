//! The `agentfs` command.
//!
//! `mount` (the FUSE layer) is not built yet. Until it is, `record` records
//! paths from a directory directly, through the same recorder the FUSE layer
//! will use, including its path rules: paths the rules exclude are reported
//! as skipped, with the reason.
//!
//! `push` pushes once to the configured remotes; `run` runs the commit and
//! push loops in the foreground until SIGINT or SIGTERM (design §11.2).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Parser, Subcommand};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use agentfs_core::config::parse_duration;
use agentfs_core::events::{Event, Events};
use agentfs_core::history::History;
use agentfs_core::identity::Context;
use agentfs_core::paths::{PathError, RepoPath};
use agentfs_core::push::{self, PushOutcome, RemotePusher, RemoteStatus};
use agentfs_core::record::{Caller, Trigger};
use agentfs_core::service::Service;
use agentfs_core::volume::{self, Input, StateDir};

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
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new state area.
    Init {
        /// The volume's name, unique within any repository it pushes to.
        #[arg(long)]
        volume: String,
        /// History branch (default: agentfs/<volume>).
        #[arg(long)]
        branch: Option<String>,
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

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let state = StateDir::new(&cli.state);
    match cli.command {
        Command::Init { volume, branch } => {
            volume::init(&state, &volume, branch.as_deref())
                .with_context(|| format!("initialising {}", state.root().display()))?;
            println!("initialised volume {volume} in {}", state.root().display());
        }
        Command::Status(pod) => {
            let (_, committer, recovery) =
                open(&state, cli.root.as_deref().unwrap_or(Path::new(".")), &pod)?;
            let history = committer.history();
            println!("volume:    {}", history.settings().volume);
            println!("branch:    {}", history.settings().branch_ref);
            match history.head()? {
                Some(head) => println!("head:      {}", head.id()),
                None => println!("head:      (none)"),
            }
            match recovery.committed_through {
                Some(seq) => println!("committed: through seq {seq}"),
                None => println!("committed: nothing"),
            }
            println!("backlog:   {} record(s)", committer.backlog());
            for remote in push::status(&state, history.settings())? {
                println!("{}", remote_line(&remote));
            }
        }
        Command::Flush(pod) => {
            let (_, committer, _) =
                open(&state, cli.root.as_deref().unwrap_or(Path::new(".")), &pod)?;
            let report = committer.commit_pending()?;
            println!(
                "{} record(s) into {} commit(s)",
                report.records, report.commits
            );
        }
        Command::Record { paths, pod } => {
            // Guessing the root would turn every path missing from the wrong
            // directory into a recorded removal.
            let Some(root) = cli.root.as_deref() else {
                bail!("record needs --root (or AGENTFS_ROOT): the backing tree the paths are in");
            };
            let (recorder, committer, _) = open(&state, root, &pod)?;
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
        }
        Command::Push { remotes } => return push_once(&state, &remotes),
        Command::Run { pod, push_grace } => {
            let root = cli.root.as_deref().unwrap_or(Path::new("."));
            return run(&state, root, &pod, push_grace);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `agentfs push`: one attempt per selected remote, in name order.
fn push_once(state: &StateDir, only: &[String]) -> Result<ExitCode> {
    let history = History::open(&state.git())
        .with_context(|| format!("opening {}", state.root().display()))?;
    let settings = history.settings();
    for name in only {
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
        println!("no remotes configured");
        return Ok(ExitCode::SUCCESS);
    }
    let events = stderr_sink();
    let mut ok = true;
    for remote in selected {
        let mut pusher = RemotePusher::open(state, settings, remote, events.clone())?;
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
fn run(state: &StateDir, root: &Path, pod: &Pod, push_grace: Duration) -> Result<ExitCode> {
    // Registered before anything starts, so an early signal is not lost.
    let mut signals = Signals::new([SIGINT, SIGTERM]).context("installing signal handlers")?;
    let events = stderr_sink();
    let (_recorder, committer, recovery) = open_with(state, root, pod, events.clone())?;
    let settings = committer.history().settings().clone();
    let pushers = push::open_all(state, &settings, events.clone())?;
    let names: Vec<String> = pushers.iter().map(|p| p.name().to_owned()).collect();
    let service = Service::start(committer, pushers, events).context("starting the service")?;
    eprintln!(
        "running volume {} ({} uncommitted record(s) recovered; remotes: {}); \
         stop with SIGINT or SIGTERM",
        settings.volume,
        recovery.uncommitted,
        if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        }
    );

    let signal = signals.forever().next();
    eprintln!(
        "signal {}: stopping (waiting up to {push_grace:?} for pushes; signal again to exit now)",
        signal.unwrap_or_default()
    );
    std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            eprintln!("second signal: exiting now");
            std::process::exit(130);
        }
    });

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
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            eprintln!("final commit failed: {e}; the records stay journaled");
            Ok(ExitCode::FAILURE)
        }
    }
}

fn open(
    state: &StateDir,
    root: &Path,
    pod: &Pod,
) -> Result<(volume::Recorder, volume::Committer, volume::Recovery)> {
    open_with(state, root, pod, stderr_sink())
}

fn open_with(
    state: &StateDir,
    root: &Path,
    pod: &Pod,
    events: Events,
) -> Result<(volume::Recorder, volume::Committer, volume::Recovery)> {
    let volume = History::open(&state.git())
        .with_context(|| format!("opening {}", state.root().display()))?
        .settings()
        .volume
        .clone();
    Ok(volume::open(state, root, pod.context(&volume), events)?)
}

/// Prints alarms (design §13) to stderr until telemetry exports them.
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
