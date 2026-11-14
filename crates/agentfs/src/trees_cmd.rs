//! Target-model composition root (git-model.md, docs/trees.md): `trees init`
//! ensures the volume through the Volume domain; `mount` restores Trees and
//! Clients on the store adapter, makes or finds its fork through Trees (a new
//! unkeyed one, or the open one under `--fork-key`), attaches (or, after a
//! kill, rebinds) one client per view through Clients, serves each view through
//! the view host on its client's session, and detaches each client as its view
//! ends; `end` ends a keyed fork. One dispatcher hands each domain event to its
//! consumers after the call that caused it returns. It wires domains to
//! adapters and decides nothing.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, mpsc};

use agentfs_clients::ClientsService;
use agentfs_contracts::caps::{LaunchCap, LaunchScope, Minter};
use agentfs_contracts::clients::{
    Access, AttachedLookup, Binding, ClientError, ClientEvent, ClientId, ClientSet, Clients,
    Consistency, FileOwner, Grant,
};
use agentfs_contracts::name::{ClientLabel, ForkKey, LauncherId, VolumeName};
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{Event, Found, StateArea};
use agentfs_contracts::trees::{ForkSettings, Landing, TreeError, Trees, TreesEvent};
use agentfs_contracts::views::{MountOptions, MountPoint, ViewId, Views};
use agentfs_contracts::volume::{EnsureOutcome, StartPoint, Volume, VolumeError, VolumeSpec};
use agentfs_core::session::Sessions;
use agentfs_core::state_area::TreeStateArea;
use agentfs_core::store::TreeStore;
use agentfs_core::tree_actor::{self, Recording, TreeId};
use agentfs_fuse::tree_view::{HostEvent, ViewHost};
use agentfs_trees::TreesService;
use agentfs_volume::VolumeService;
use anyhow::{Context, Result, bail};

use crate::domain_events::DomainEvents;
use crate::pod_args::PodArgs;
use crate::shared::Shared;

/// Exit status when another process holds the state area (`EX_TEMPFAIL`).
const EXIT_BUSY: u8 = 75;

/// What `mount` says when a fork's one writer has gone (C4).
const FORK_TAKEN: &str =
    "this fork has had its writer; mount it with --fork-read-only or use a new fork";

/// What `mount` says when its key's fork has ended.
const KEY_ENDED: &str = "the fork under this --fork-key has ended; use a new key";

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
    /// Recover and mount the root, optionally also a fork. Unmount all views to exit.
    /// Never creates a volume.
    Mount {
        /// Mount the root here: an existing empty directory, outside the
        /// state area. Not named `root`: that id is the global `--root`.
        #[arg(value_name = "ROOT")]
        mountpoint: PathBuf,
        /// Mount a fork of the root in this existing empty directory too: a
        /// new fork, ended when its last client detaches, or with --fork-key
        /// the open fork under that key (made the first time).
        #[arg(long)]
        fork_mount: Option<PathBuf>,
        /// The fork's idempotency key: 1 to 128 bytes of printable ASCII
        /// with no whitespace. A keyed fork ends only at `trees end`.
        #[arg(long, requires = "fork_mount")]
        fork_key: Option<String>,
        /// Write process-local measurement counters to this private path.
        #[arg(long)]
        metrics_output: Option<PathBuf>,
        /// The root's client label: a safe name of at most 64 bytes. With the
        /// client's number, launcher and tree it is every save's writer.
        #[arg(long, default_value = "root-client")]
        writer: String,
        /// The fork's client label.
        #[arg(long, default_value = "fork-client", requires = "fork_mount")]
        fork_writer: String,
        /// Mount the root read-only: its client cannot write.
        #[arg(long)]
        read_only: bool,
        /// Mount the fork read-only: how a keyed fork whose writer has gone
        /// (a clean unmount ends it) is mounted again.
        #[arg(long, requires = "fork_mount", conflicts_with = "fork_writer")]
        fork_read_only: bool,
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
    /// End the fork under a key: it drops once its landings are taken and no
    /// file is open in it. Exits 75 while a mount holds the volume.
    End {
        #[arg(long)]
        fork_key: String,
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

/// `trees init`'s Clients: it attaches none and judges no release.
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

/// One view `mount` serves: its tree, mount point and client.
struct ViewSpec {
    tree: TreeId,
    at: PathBuf,
    label: ClientLabel,
    access: Access,
}

impl ViewSpec {
    fn new(tree: TreeId, at: PathBuf, label: &str, read_only: bool) -> Result<Self> {
        Ok(Self {
            tree,
            at,
            label: ClientLabel::parse(label)?,
            access: if read_only {
                Access::ReadOnly
            } else {
                Access::ReadWrite
            },
        })
    }
}

/// The fork view `mount` was asked for, and the key it is under.
struct ForkView {
    spec: ViewSpec,
    key: Option<ForkKey>,
}

pub fn run(state: &Path, args: Args, minter: Minter) -> Result<ExitCode> {
    match args.command {
        Command::Init { volume, pod } => init(state, &volume, &context(&pod)?, &minter),
        Command::End { fork_key, pod } => {
            end(state, &ForkKey::parse(&fork_key)?, &context(&pod)?, minter)
        }
        Command::Mount {
            mountpoint: root,
            fork_mount,
            fork_key,
            writer,
            fork_writer,
            read_only,
            fork_read_only,
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
            let root = ViewSpec::new(TreeId::Root, root, &writer, read_only)?;
            let fork = fork_mount
                .map(|at| -> Result<ForkView> {
                    Ok(ForkView {
                        spec: ViewSpec::new(TreeId::Root, at, &fork_writer, fork_read_only)?,
                        key: fork_key.as_deref().map(ForkKey::parse).transpose()?,
                    })
                })
                .transpose()?;
            let mount = Mount {
                at,
                owner,
                options: MountOptions {
                    allow_other: !no_allow_other,
                },
                metrics_output,
            };
            mount.run(state, root, fork, minter)
        }
    }
}

type Forks = TreesService<TreeStore, TreeStore, DomainEvents>;
type ClientsOf = ClientsService<TreeStore, Sessions, Shared<Forks>, DomainEvents>;

/// One volume's domains, restored on one store, and their event queue.
struct Domains {
    trees: Shared<Forks>,
    clients: ClientsOf,
    sessions: Sessions,
    events: DomainEvents,
    cap: LaunchCap,
}

impl Domains {
    /// Trees and Clients restored over `recording`, as every start restores them.
    fn restore(recording: Arc<Recording>, minter: Minter) -> Result<Self> {
        let store = TreeStore::new(Arc::clone(&recording));
        let events = DomainEvents::default();
        let trees = Shared::new(
            TreesService::restore(store.clone(), store.clone(), events.clone())
                .context("reading the forks' rows")?,
        );
        let cap = root_cap(&minter)?;
        let sessions = Sessions::new(recording);
        let clients = ClientsService::restore(
            store,
            sessions.clone(),
            trees.clone(),
            events.clone(),
            minter,
        )
        .context("reading the clients' rows")?;
        Ok(Self {
            trees,
            clients,
            sessions,
            events,
            cap,
        })
    }

    /// The one dispatcher: each domain event, in order, to its consumers,
    /// after the call that caused it returns. `LastDetached` ends an unkeyed
    /// fork; `ForkDropped` releases its writer claim. A consumer's failure is
    /// logged: the next boot re-judges from asked facts.
    fn dispatch(&mut self) {
        while let Some(event) = self.events.next() {
            let handled = match event {
                Event::Clients(ClientEvent::LastDetached { tree }) => {
                    self.trees.get_mut().on_last_detached(tree)
                }
                Event::Trees(TreesEvent::ForkDropped(fork)) => self.clients.on_fork_dropped(fork),
                _ => Ok(()),
            };
            if let Err(e) = handled {
                eprintln!("tree EventFailed: {e}");
            }
        }
    }

    /// Drops every ended fork nothing holds: at boot and after each landing.
    fn settle(&mut self) {
        let settled = self.trees.get_mut().settle();
        if let Err(e) = settled {
            eprintln!("tree SettleFailed: {e}");
        }
        self.dispatch();
    }

    /// The fork view's fork: the open one under its key, or a new one. Its
    /// branch and its row are one step, durable when this returns.
    fn fork(&mut self, key: Option<ForkKey>) -> Result<TreeId, TreeError> {
        let settings = ForkSettings {
            landing: Landing::Automatic,
        };
        let fork = self
            .trees
            .get_mut()
            .create(TreeId::Root, key, settings, &self.cap);
        self.dispatch();
        Ok(fork?.into())
    }

    /// Detaches each client, its opens saved first; a failure is logged (the
    /// next boot's reconcile ends it).
    fn detach_all(&mut self, ids: &[ClientId]) {
        for client in ids {
            if let Err(e) = self.clients.detach(client, &self.cap) {
                eprintln!("tree DetachFailed: client={client}: {e}");
            }
            self.dispatch();
        }
    }
}

/// `trees mount`, once its flags are read.
struct Mount {
    /// The pod context of every client it attaches.
    at: PodContext,
    owner: FileOwner,
    options: MountOptions,
    metrics_output: Option<PathBuf>,
}

impl Mount {
    /// Restores Trees and Clients on one store; makes or finds the fork view's
    /// fork; per view, rebinds the restored client of this launcher, tree,
    /// label and access, else attaches one; reconciles the rest (a view host
    /// dies with its process) and drops what may drop; serves each view on its
    /// session and detaches its client when it ends.
    fn run(
        self,
        state: &Path,
        root: ViewSpec,
        fork: Option<ForkView>,
        minter: Minter,
    ) -> Result<ExitCode> {
        let recording = open(state, &self.at)?;
        let mut domains = Domains::restore(recording, minter)?;
        let mut specs = vec![root];
        if let Some(ForkView { mut spec, key }) = fork {
            spec.tree = match domains.fork(key) {
                Ok(tree) => tree,
                Err(TreeError::Ended) => {
                    eprintln!("error: {KEY_ENDED}");
                    return Ok(ExitCode::FAILURE);
                }
                Err(e) => bail!("creating the fork: {e}"),
            };
            eprintln!("fork tree: {}", spec.tree);
            specs.push(spec);
        }
        let mut bound: Vec<(PathBuf, Binding)> = Vec::new();
        for spec in specs {
            let session = domains.sessions.open();
            let resumable =
                domains
                    .clients
                    .resumable(&domains.cap, spec.tree, &spec.label, spec.access);
            let answered = match resumable {
                Some(client) => {
                    domains
                        .clients
                        .rebind(&client, self.at.clone(), session, &domains.cap)
                }
                None => {
                    let grant = match spec.access {
                        Access::ReadWrite => {
                            Grant::live_read_write(spec.tree, Consistency::PerFile, self.owner)
                        }
                        Access::ReadOnly => {
                            Grant::live_read_only(spec.tree, Consistency::PerFile, self.owner)
                        }
                    };
                    domains.clients.attach(
                        grant,
                        spec.label,
                        self.at.clone(),
                        session,
                        &domains.cap,
                    )
                }
            };
            domains.dispatch();
            match answered {
                Ok(binding) => bound.push((spec.at, binding)),
                Err(refused) => {
                    let ids: Vec<ClientId> = bound.into_iter().map(|(_, b)| b.client).collect();
                    domains.detach_all(&ids);
                    if refused == ClientError::ForkTaken {
                        eprintln!("error: {FORK_TAKEN}");
                        return Ok(ExitCode::FAILURE);
                    }
                    bail!("attaching the client of {}: {refused}", spec.tree);
                }
            }
        }
        let live: ClientSet = bound.iter().map(|(_, b)| b.client.clone()).collect();
        domains.clients.reconcile(&live, &domains.cap);
        domains.dispatch();
        domains.settle();
        let (sender, ended) = mpsc::channel();
        let notify = Arc::new(move |event: HostEvent| {
            // The receiver goes only once every view has ended.
            let _ = sender.send(event);
        });
        let mut host = ViewHost::new(
            domains.sessions.clone(),
            state,
            self.metrics_output.as_deref(),
            notify,
        )
        .context("serving tree views")?;
        let mut views: BTreeMap<ViewId, ClientId> = BTreeMap::new();
        let mut unopened: Vec<ClientId> = Vec::new();
        let mut failed = None;
        for (at, binding) in bound {
            let client = binding.client.clone();
            if failed.is_some() {
                unopened.push(client);
                continue;
            }
            match host.open(MountPoint(at.clone()), binding, self.options) {
                Ok(view) => {
                    views.insert(view, client);
                }
                Err(e) => {
                    failed = Some(format!("mounting {}: {e}", at.display()));
                    unopened.push(client);
                    for view in views.keys() {
                        host.close(*view);
                    }
                }
            }
        }
        domains.detach_all(&unopened);
        while !views.is_empty() {
            let Ok(event) = ended.recv() else { break };
            match event {
                HostEvent::ViewEnded(view) => {
                    if let Some(client) = views.remove(&view) {
                        domains.detach_all(&[client]);
                    }
                }
                HostEvent::Landed => domains.settle(),
            }
        }
        host.wait().context("serving tree views")?;
        if let Some(failed) = failed {
            bail!("serving tree views: {failed}");
        }
        Ok(ExitCode::SUCCESS)
    }
}

/// `trees end --fork-key`: the keyed fork ends, and drops at once when
/// nothing holds it (else at a later mount's settle).
fn end(state: &Path, key: &ForkKey, at: &PodContext, minter: Minter) -> Result<ExitCode> {
    let recording = match Recording::open(state, at) {
        Ok(recording) => recording,
        Err(tree_actor::Error::Busy) => {
            eprintln!("error: {}", VolumeError::Busy);
            return Ok(ExitCode::from(EXIT_BUSY));
        }
        Err(e) => return Err(e).context("opening tree state"),
    };
    let mut domains = Domains::restore(recording, minter)?;
    let ended = domains.trees.get_mut().end_keyed(key, &domains.cap);
    domains.dispatch();
    match ended {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(refused @ TreeError::KeyUnknown) => {
            eprintln!("error: {refused}");
            Ok(ExitCode::FAILURE)
        }
        Err(e) => bail!("ending the fork: {e}"),
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
            "--fork-mount",
            "x",
            "--fork-key",
            "pvc-1",
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
        let error = super::run(&state, args, minter).unwrap_err();
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
