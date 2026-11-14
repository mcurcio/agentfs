//! Target-model composition root (git-model.md, docs/trees.md): `trees init`
//! ensures the volume through the Volume domain; `fork`, and `mount` with a
//! new sandbox, create the fork through the Trees domain on the store adapter;
//! `mount` serves the volume through the tree actor. It wires domains to
//! adapters and decides nothing.
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use agentfs_contracts::caps::{LaunchCap, LaunchScope, Minter};
use agentfs_contracts::clients::{AttachedLookup, ClientSet, FileOwner};
use agentfs_contracts::name::{LauncherId, VolumeName, WriterLabel};
use agentfs_contracts::pod::{PodContext, WriterAt};
use agentfs_contracts::ports::{Found, StateArea};
use agentfs_contracts::trees::{ForkSettings, Landing, Trees};
use agentfs_contracts::views::MountOptions;
use agentfs_contracts::volume::{EnsureOutcome, StartPoint, Volume, VolumeError, VolumeSpec};
use agentfs_core::state_area::TreeStateArea;
use agentfs_core::store::TreeStore;
use agentfs_core::tree_actor::{self, Recording, TreeId};
use agentfs_fuse::tree_view::{Binding, serve_with_metrics};
use agentfs_trees::TreesService;
use agentfs_volume::VolumeService;
use anyhow::{Context, Result, bail};

use crate::domain_events::DomainEvents;
use crate::pod_args::PodArgs;

/// Exit status when another process holds the state area (`EX_TEMPFAIL`).
const EXIT_BUSY: u8 = 75;

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Create the volume, or find it (same name: nothing written), then
    /// recover it. Prints `<name> <id> created|found`.
    Init {
        /// The volume's name: a label and a guard, kept in its identity file.
        /// Another name at the same state area is refused, nothing written.
        #[arg(long, env = "AGENTFS_VOLUME")]
        volume: String,
        #[command(flatten)]
        pod: PodArgs,
    },
    /// Create an isolated sandbox fork at the root's saved head; print its identity.
    Fork {
        #[command(flatten)]
        pod: PodArgs,
    },
    /// Recover and mount the root, optionally also a sandbox. Unmount all views to exit.
    /// Never creates a volume.
    Mount {
        /// Mount the root here: an existing empty directory, outside the
        /// state area. Not named `root`: that id is the global `--root`.
        #[arg(value_name = "ROOT")]
        mountpoint: PathBuf,
        /// Mount a sandbox in this existing empty directory too.
        #[arg(long)]
        sandbox: Option<PathBuf>,
        /// Reuse this sandbox (fork/N); otherwise create one when --sandbox is set.
        #[arg(long, requires = "sandbox")]
        fork: Option<TreeId>,
        /// Write process-local measurement counters to this private path.
        #[arg(long)]
        metrics_output: Option<PathBuf>,
        /// The root's trusted writer label (not an authenticated identity).
        #[arg(long, default_value = "root-client")]
        writer: String,
        /// The sandbox's trusted writer label.
        #[arg(long, default_value = "sandbox-client")]
        sandbox_writer: String,
        /// A guard: refuse, before any write, a state area whose volume has
        /// another name.
        #[arg(long, env = "AGENTFS_VOLUME")]
        volume: Option<String>,
        /// Every file's owner in the views, `UID:GID` (default: this
        /// process's effective user and group).
        #[arg(long, env = "AGENTFS_OWNER", value_parser = parse_owner, value_name = "UID:GID")]
        owner: Option<FileOwner>,
        /// Only the mounting user reaches the views (no FUSE `allow_other`).
        #[arg(long)]
        no_allow_other: bool,
        #[command(flatten)]
        pod: PodArgs,
    },
}

fn parse_owner(text: &str) -> Result<FileOwner, String> {
    let (uid, gid) = text
        .split_once(':')
        .ok_or_else(|| format!("{text:?} is not UID:GID"))?;
    let id = |part: &str| {
        part.parse::<u32>()
            .map_err(|_| format!("{text:?} is not UID:GID"))
    };
    Ok(FileOwner {
        uid: id(uid)?,
        gid: id(gid)?,
    })
}

/// Clients until they are built: no client is ever attached.
struct NoClients;

impl AttachedLookup for NoClients {
    fn attached_any(&self) -> ClientSet {
        ClientSet::new()
    }
}

fn context(pod: &PodArgs) -> Result<PodContext> {
    pod.context().context("reading the pod flags")
}

/// The volume's own launcher: the trial CLI.
fn root_cap(minter: &Minter) -> Result<LaunchCap> {
    Ok(LaunchCap::mint(
        minter,
        LauncherId::parse("cli")?,
        LaunchScope::Root,
    ))
}

/// A new unkeyed fork of the root, through the Trees domain on the store:
/// its branch and its row are one step, durable when this returns.
fn create_fork(recording: &Arc<Recording>, cap: &LaunchCap) -> Result<TreeId> {
    let store = TreeStore::new(Arc::clone(recording));
    let mut trees = TreesService::restore(store.clone(), store, DomainEvents::default())
        .context("reading the forks' rows")?;
    let settings = ForkSettings {
        landing: Landing::Automatic,
    };
    let fork = trees
        .create(TreeId::Root, None, settings, cap)
        .context("creating the fork")?;
    Ok(fork.into())
}

pub fn run(state: &Path, args: Args, minter: &Minter) -> Result<ExitCode> {
    match args.command {
        Command::Init { volume, pod } => init(state, &volume, &context(&pod)?, minter),
        Command::Fork { pod } => {
            let recording = open(state, &context(&pod)?)?;
            let fork = create_fork(&recording, &root_cap(minter)?)?;
            // Durable before the id is printed: dropping the volume fsyncs.
            drop(recording);
            println!("{fork}");
            Ok(ExitCode::SUCCESS)
        }
        Command::Mount {
            mountpoint: root,
            sandbox,
            fork,
            writer,
            sandbox_writer,
            metrics_output,
            volume,
            owner,
            no_allow_other,
            pod,
        } => {
            let at = context(&pod)?;
            if let Some(name) = volume {
                let name = VolumeName::parse(&name)?;
                match tree_actor::inspect(state).context("reading the state area")? {
                    Found::Volume(identity) if identity.name != name => bail!(
                        "the volume here is {:?}, not {:?}: nothing was written",
                        identity.name.as_str(),
                        name.as_str()
                    ),
                    _ => {}
                }
            }
            let owner = owner.unwrap_or_else(|| FileOwner {
                uid: rustix::process::geteuid().as_raw(),
                gid: rustix::process::getegid().as_raw(),
            });
            let by = |label: &str| -> Result<WriterAt> {
                Ok(WriterAt {
                    writer: WriterLabel::parse(label)?,
                    at: at.clone(),
                })
            };
            let recording = open(state, &at)?;
            let mut bindings = vec![Binding {
                tree: TreeId::Root,
                by: by(&writer)?,
                owner,
                mountpoint: root,
            }];
            if let Some(mountpoint) = sandbox {
                let tree = if let Some(fork) = fork {
                    if fork.is_root() {
                        bail!("--fork must name an existing fork/N, not the root");
                    }
                    recording.actor()?.head(fork)?;
                    fork
                } else {
                    create_fork(&recording, &root_cap(minter)?)?
                };
                eprintln!("sandbox tree: {tree}");
                bindings.push(Binding {
                    tree,
                    by: by(&sandbox_writer)?,
                    owner,
                    mountpoint,
                });
            }
            let options = MountOptions {
                allow_other: !no_allow_other,
            };
            serve_with_metrics(
                state,
                recording,
                bindings,
                options,
                metrics_output.as_deref(),
            )
            .context("serving tree views")?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Recovers the volume; never creates one.
fn open(state: &Path, at: &PodContext) -> Result<Arc<Recording>> {
    Recording::open(state, at).context("opening tree state")
}

/// `ensure` as the volume's own launcher, then recovery of what it found or
/// made, as every start does.
fn init(state: &Path, name: &str, at: &PodContext, minter: &Minter) -> Result<ExitCode> {
    let spec = VolumeSpec {
        name: VolumeName::parse(name)?,
        start: StartPoint::Empty,
    };
    let cap = root_cap(minter)?;
    let mut volume =
        VolumeService::new(TreeStateArea::at(state), NoClients, DomainEvents::default());
    let ensured = match volume.ensure(spec.clone(), at, &cap) {
        Ok(ensured) => ensured,
        Err(VolumeError::Busy) => {
            eprintln!("error: {}", VolumeError::Busy);
            return Ok(ExitCode::from(EXIT_BUSY));
        }
        Err(refused) => {
            eprintln!("error: {refused}");
            return Ok(ExitCode::FAILURE);
        }
    };
    match TreeStateArea::at(state).open(at) {
        Ok(_) => {}
        Err(VolumeError::Busy) => {
            eprintln!("error: {}", VolumeError::Busy);
            return Ok(ExitCode::from(EXIT_BUSY));
        }
        Err(e) => bail!("recovering the volume: {e}"),
    }
    let outcome = match ensured.outcome {
        EnsureOutcome::Created => "created",
        EnsureOutcome::Found => "found",
    };
    println!("{} {} {outcome}", spec.name, ensured.id.0.hyphenated());
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::Command;
    use clap::Parser;

    /// The mount point never lands in the global `--root` (an arg id clash
    /// does exactly that, and `trees` then refuses to run).
    #[test]
    fn mount_point_is_not_the_global_root() {
        let cli = crate::Cli::try_parse_from([
            "agentfs",
            "--state",
            "s",
            "trees",
            "mount",
            "m",
            "--sandbox",
            "x",
            "--fork",
            "fork/1",
        ])
        .expect("parses");
        assert!(cli.root.is_none(), "positional leaked into --root");
        let crate::Command::Trees(args) = cli.command else {
            panic!("not trees");
        };
        let Command::Mount { mountpoint, .. } = args.command else {
            panic!("not mount");
        };
        assert_eq!(mountpoint, std::path::PathBuf::from("m"));
    }

    #[test]
    fn trees_init_needs_a_volume() {
        assert!(crate::Cli::try_parse_from(["agentfs", "--state", "s", "trees", "init"]).is_err());
        let cli = crate::Cli::try_parse_from([
            "agentfs", "--state", "s", "trees", "init", "--volume", "t", "--pod", "p-1",
        ])
        .expect("parses");
        let crate::Command::Trees(args) = cli.command else {
            panic!("not trees");
        };
        let Command::Init { volume, pod } = args.command else {
            panic!("not init");
        };
        assert_eq!(volume, "t");
        assert_eq!(pod.pod, "p-1");
    }

    /// `mount` recovers a volume and never creates one: nothing is written.
    #[test]
    fn mount_never_creates() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let view = tmp.path().join("view");
        std::fs::create_dir(&view).unwrap();
        let cli = crate::Cli::try_parse_from([
            "agentfs",
            "--state",
            state.to_str().unwrap(),
            "trees",
            "mount",
            view.to_str().unwrap(),
        ])
        .expect("parses");
        let crate::Command::Trees(args) = cli.command else {
            panic!("not trees");
        };
        #[allow(
            clippy::disallowed_methods,
            reason = "a test is its own composition root"
        )]
        let minter = agentfs_contracts::caps::Minter::new();
        let error = super::run(&state, args, &minter).unwrap_err();
        assert!(
            format!("{error:#}").contains("trees init --volume"),
            "{error:#}"
        );
        assert!(!state.exists());
    }

    #[test]
    fn an_owner_is_uid_colon_gid() {
        let owner = super::parse_owner("10000:10001").unwrap();
        assert_eq!((owner.uid, owner.gid), (10000, 10001));
        for bad in ["10000", "a:b", "1:", ":1", "1:2:3"] {
            assert!(super::parse_owner(bad).is_err(), "{bad}");
        }
    }
}
