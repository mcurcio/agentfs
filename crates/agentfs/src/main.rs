//! The `agentfs` command.
//!
//! `mount` (the FUSE layer) is not built yet. Until it is, `record` records
//! paths from a directory directly, through the same recorder the FUSE layer
//! will use, including its path rules: paths the rules exclude are reported
//! as skipped, with the reason.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use clap::{Args, Parser, Subcommand};

use agentfs_core::history::History;
use agentfs_core::identity::Context;
use agentfs_core::paths::{PathError, RepoPath};
use agentfs_core::record::{Caller, Trigger};
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

fn main() -> Result<()> {
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
                        Some(PathError::DotGit(_)) => {
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
    }
    Ok(())
}

fn open(
    state: &StateDir,
    root: &Path,
    pod: &Pod,
) -> Result<(volume::Recorder, volume::Committer, volume::Recovery)> {
    let volume = History::open(&state.git())
        .with_context(|| format!("opening {}", state.root().display()))?
        .settings()
        .volume
        .clone();
    Ok(volume::open(state, root, pod.context(&volume))?)
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
