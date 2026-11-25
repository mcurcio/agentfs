//! The tree engine's launcher (docs/trees.md): `init` ensures the volume
//! through the Volume domain; `mount` prepares each view's mount point
//! ([`mountpoint`]), reads the operator settings ([`settings`]), stages the
//! volume through Volume, restores Trees and Clients on the store adapter,
//! makes or finds its fork through Trees (a new unkeyed one, or the open one
//! under `--fork-key`), attaches (or, after a kill, rebinds) one client per
//! view through Clients, serves each view through the view host on its
//! client's session, optionally runs a command in it ([`command`]), detaches
//! each client as its view ends, and on a stop (SIGTERM, SIGINT, the
//! command's exit, or its last view gone) unstages and exits once Volume
//! releases; `host` serves one volume's views on its parent's requests
//! ([`host`]); `fork end` ends a keyed fork. `mount` and `host` share their
//! boot and drain ([`serving`]). The volume is composed by the service
//! (`agentfs_service`), whose one dispatcher hands each domain event to its
//! consumers after the call that caused it returns; this module mints the
//! launcher it opens as, and one channel carries every trigger of the loop.
//! It wires domains to adapters and decides nothing.

mod command;
mod host;
mod mountpoint;
mod serving;
mod settings;
mod verify;

pub use self::host::{HostArgs, host};
pub use self::verify::{VerifyArgs, verify};

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use agentfs_contracts::caps::{LaunchCap, LaunchScope, Minter};
use agentfs_contracts::clients::FileOwner;
use agentfs_contracts::clients::{Access, Binding, ClientError, ClientId, ClientSet};
use agentfs_contracts::name::{ClientLabel, ForkKey, LauncherId, VolumeName};
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{Found, StateArea};
use agentfs_contracts::trees::{ForkSettings, Landing, TreeError};
use agentfs_contracts::views::{MountOptions, ViewError, ViewId};
use agentfs_contracts::volume::{
    EnsureOutcome, StartPoint, VolumeError, VolumeIdentity, VolumeSpec,
};
use agentfs_core::identity::Context as Identity;
use agentfs_engine::operator_settings::OperatorSettings;
use agentfs_engine::state_area::TreeStateArea;
use agentfs_engine::telemetry;
use agentfs_engine::tree_actor::{self, TreeId, Upkeep};
use agentfs_service::{Domains, Launcher, ServiceError};
use anyhow::{Context, Result, bail};
use opentelemetry::metrics::Meter;

use self::serving::{Serving, Trigger};
use crate::daemon::Daemon;
use crate::pod_args::PodArgs;

/// Exit status when another process holds the state area (`EX_TEMPFAIL`).
const EXIT_BUSY: u8 = 75;

/// Exit status when `-- COMMAND` could not be started (as a shell answers).
const EXIT_NOT_STARTED: u8 = 127;

/// What the runbook that moves an old state area aside is called.
const UPGRADE_RUNBOOK: &str = "docs/runbooks/upgrading-the-sidecar.md";

/// The gauge of the volume's flagged saves (`agentfs.history.unverified`),
/// registered on `meter`: it reads the staged volume's counts while the
/// returned handle lives, never the actor's lock.
///
/// # Errors
///
/// The actor stopped before the gauge could be registered.
fn observe_unverified(meter: &Meter, domains: &Domains) -> Result<UnverifiedGauge> {
    let verification = domains
        .sessions()
        .recording()
        .actor()
        .context("reading the volume's flagged saves")?
        .verification();
    let gauge: UnverifiedGauge = std::sync::Arc::new(move || verification.counts());
    telemetry::observe_unverified(meter, std::sync::Arc::downgrade(&gauge));
    Ok(gauge)
}

/// What keeps the gauge of [`observe_unverified`] read.
type UnverifiedGauge = std::sync::Arc<dyn Fn() -> tree_actor::UnverifiedCounts + Send + Sync>;

/// Says another process holds the state area, and answers its exit status.
fn busy() -> ExitCode {
    eprintln!("error: {}", VolumeError::Busy);
    ExitCode::from(EXIT_BUSY)
}

/// Says the state area holds another format, and answers exit 1.
fn other_format(state: &Path) -> ExitCode {
    eprintln!(
        "error: {} holds another format, such as a recording state area: it is never converted \
         or overwritten; move it with {UPGRADE_RUNBOOK}",
        state.display()
    );
    ExitCode::FAILURE
}

/// What `mount` says when a fork's one writer has gone (C4,
/// docs/library/domains.md).
const FORK_TAKEN: &str =
    "this fork has had its writer; mount it with --fork-read-only or use a new fork";

/// What `mount` says when its key's fork has ended.
const KEY_ENDED: &str = "the fork under this --fork-key has ended; use a new key";

/// `agentfs init`'s flags.
#[derive(clap::Args)]
pub struct InitArgs {
    /// The volume's name: a label and a guard, kept in its identity file.
    /// Another name at the same state area is refused, nothing written.
    #[arg(long, env = "AGENTFS_VOLUME")]
    pub volume: String,
    /// An operator settings file to include (an absolute path, which must
    /// exist); repeatable. When given, these become exactly the includes of
    /// `<state>/config`.
    #[arg(long = "include", value_name = "FILE")]
    pub includes: Vec<PathBuf>,
    /// The owner a new volume's root directory is recorded with. Applies
    /// only when the volume is created.
    #[arg(long, env = "AGENTFS_OWNER", value_name = "UID:GID")]
    pub owner: Option<FileOwner>,
    #[command(flatten)]
    pub pod: PodArgs,
}

/// `agentfs mount`'s flags.
#[derive(clap::Args)]
pub struct MountArgs {
    /// Mount the root here: an empty directory (created when missing),
    /// outside the state area. Not named `root`: that id is the global
    /// `--root`.
    #[arg(value_name = "ROOT")]
    pub mountpoint: PathBuf,
    /// Mount a fork of the root in this empty directory too: a new fork,
    /// ended when its last client detaches, or with --fork-key the open fork
    /// under that key (made the first time).
    #[arg(long)]
    pub fork_mount: Option<PathBuf>,
    /// The fork's idempotency key: 1 to 128 bytes of printable ASCII with no
    /// whitespace. A keyed fork ends only at `fork end`.
    #[arg(long, requires = "fork_mount")]
    pub fork_key: Option<String>,
    /// Write process-local measurement counters to this private path.
    #[arg(long)]
    pub metrics_output: Option<PathBuf>,
    /// The root's client label: a safe name of at most 64 bytes. With the
    /// client's number, launcher and tree it is every save's writer.
    #[arg(long, default_value = "root-client")]
    pub writer: String,
    /// The fork's client label.
    #[arg(long, default_value = "fork-client", requires = "fork_mount")]
    pub fork_writer: String,
    /// Mount the root read-only: its client cannot write.
    #[arg(long)]
    pub read_only: bool,
    /// Mount the fork read-only: how a keyed fork whose writer has gone (a
    /// clean unmount ends it) is mounted again.
    #[arg(long, requires = "fork_mount", conflicts_with = "fork_writer")]
    pub fork_read_only: bool,
    /// A guard: refuse, before any write, a state area whose volume has
    /// another name.
    #[arg(long, env = "AGENTFS_VOLUME")]
    pub volume: Option<String>,
    /// Only the mounting user reaches the views (no FUSE `allow_other`).
    #[arg(long)]
    pub no_allow_other: bool,
    /// What to do when a mount point is an agentfs mount whose daemon died:
    /// `refuse` (exit 1 with the command that clears it), `hold` (mount
    /// nothing, wait for SIGTERM, then detach the stale mount) or `recover`
    /// (stack each view over its stale mount, so the workload's copies of
    /// it reach this mount; needs CAP_SYS_ADMIN).
    #[arg(
        long,
        env = "AGENTFS_ON_STALE_MOUNT",
        default_value = "refuse",
        value_enum
    )]
    pub on_stale_mount: OnStaleMount,
    /// Seal each bare mount point before mounting: owned by root, mode 0000,
    /// so a non-root workload that reaches it while no view is mounted (a
    /// `subPath` bound while the sidecar restarts) gets EACCES instead of
    /// writing where nothing records it. Needs root.
    #[arg(long, env = "AGENTFS_SEAL_MOUNTPOINT")]
    pub seal_mountpoint: bool,
    #[command(flatten)]
    pub pod: PodArgs,
    /// Run this command once every view is up, with this process's stdio
    /// and environment; the mount drains when it exits, and exits with its
    /// status when that is not 0.
    #[arg(last = true, value_name = "COMMAND")]
    pub command: Vec<OsString>,
}

/// `agentfs fork end`'s flags.
#[derive(clap::Args)]
pub struct EndArgs {
    /// The key of the fork to end.
    #[arg(long)]
    pub fork_key: String,
    #[command(flatten)]
    pub pod: PodArgs,
}

/// `mount --on-stale-mount`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OnStaleMount {
    Refuse,
    Hold,
    Recover,
}

fn context(pod: &PodArgs) -> Result<PodContext> {
    pod.context().context("reading the pod flags")
}

/// The volume at `state`, read before any write; `Err` holds the exit
/// status (1) of no volume, with the hint to create one, or of another
/// format.
///
/// # Errors
///
/// The state area cannot be read.
fn found(state: &Path) -> Result<std::result::Result<VolumeIdentity, ExitCode>> {
    Ok(match TreeStateArea::at(state).inspect()? {
        Found::Nothing => {
            eprintln!("error: no volume here: run `agentfs init --volume NAME` first");
            Err(ExitCode::FAILURE)
        }
        Found::Other => Err(other_format(state)),
        Found::Volume(identity) => Ok(identity),
    })
}

/// The `--volume` guard: another name at the state area is refused before
/// any write.
///
/// # Errors
///
/// The refusal, or a name that is no volume name.
fn guard(name: &VolumeName, wanted: &str) -> Result<()> {
    let wanted = VolumeName::parse(wanted)?;
    if wanted != *name {
        bail!(
            "the volume here is {:?}, not {:?}: nothing was written",
            name.as_str(),
            wanted.as_str()
        );
    }
    Ok(())
}

/// The volume's own launcher, the CLI, at `at`: its capability minted from
/// `minter`, which it then carries for Clients.
fn root_launcher(minter: Minter, at: PodContext) -> Result<Launcher> {
    let cap = LaunchCap::mint(&minter, LauncherId::parse("cli")?, LaunchScope::Root);
    Ok(launcher(minter, cap, at))
}

/// The launcher every tree command opens a volume as: `cap` at `at`, its
/// journal kept by [`upkeep`].
fn launcher(minter: Minter, cap: LaunchCap, at: PodContext) -> Launcher {
    Launcher::new(minter, cap, at).with_upkeep(upkeep())
}

/// One view `mount` serves: its tree, mount point and client, and whether
/// it stacks over a stale mount.
struct ViewSpec {
    tree: TreeId,
    at: PathBuf,
    label: ClientLabel,
    access: Access,
    over_stale: bool,
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
            over_stale: false,
        })
    }
}

/// The fork view `mount` was asked for, and the key it is under.
struct ForkView {
    spec: ViewSpec,
    key: Option<ForkKey>,
}

/// `agentfs init`: `ensure` as the volume's own launcher, then recovery of
/// what it found or made, as every start does; with `--include`, the
/// operator settings' includes, written under the recovered volume's lock.
/// Prints `<name> <id> created|found`.
///
/// # Errors
///
/// A failed recovery or write.
pub fn init(state: &Path, args: InitArgs, minter: Minter) -> Result<ExitCode> {
    let at = context(&args.pod)?;
    for include in &args.includes {
        if !include.is_absolute() {
            eprintln!(
                "error: --include {}: give an absolute path (nothing was written)",
                include.display()
            );
            return Ok(ExitCode::FAILURE);
        }
        if !include.exists() {
            eprintln!(
                "error: --include {}: no such file (nothing was written)",
                include.display()
            );
            return Ok(ExitCode::FAILURE);
        }
    }
    let spec = VolumeSpec {
        name: VolumeName::parse(&args.volume)?,
        start: StartPoint::Empty,
        root_owner: args.owner,
    };
    let launcher = root_launcher(minter, at)?;
    let ensured = match agentfs_service::init(state, spec.clone(), &launcher, &args.includes) {
        Ok(ensured) => ensured,
        Err(ServiceError::Volume(VolumeError::Busy)) => return Ok(busy()),
        Err(ServiceError::Volume(VolumeError::OtherFormat)) => return Ok(other_format(state)),
        Err(ServiceError::Volume(refused)) => {
            eprintln!("error: {refused}");
            return Ok(ExitCode::FAILURE);
        }
        Err(e) => return Err(e.into()),
    };
    let outcome = match ensured.outcome {
        EnsureOutcome::Created => "created",
        EnsureOutcome::Found => {
            if args.owner.is_some() {
                eprintln!("found: --owner applies only when the volume is created");
            }
            "found"
        }
    };
    println!("{} {} {outcome}", spec.name, ensured.id.0.hyphenated());
    Ok(ExitCode::SUCCESS)
}

/// `agentfs mount`: the volume checked, the operator settings read and
/// logged (at every boot, a held one included), then the mount points
/// judged and prepared, then the views served until a stop. Every refusal
/// comes before a mount point is created or sealed.
///
/// # Errors
///
/// A refusal or failure, before or while serving.
pub fn mount(state: &Path, args: MountArgs, minter: Minter) -> Result<ExitCode> {
    let at = context(&args.pod)?;
    let mut root = ViewSpec::new(TreeId::Root, args.mountpoint, &args.writer, args.read_only)?;
    let mut fork = args
        .fork_mount
        .map(|at| -> Result<ForkView> {
            Ok(ForkView {
                spec: ViewSpec::new(TreeId::Root, at, &args.fork_writer, args.fork_read_only)?,
                key: args.fork_key.as_deref().map(ForkKey::parse).transpose()?,
            })
        })
        .transpose()?;
    let name = match found(state)? {
        Ok(identity) => identity.name,
        Err(code) => return Ok(code),
    };
    if let Some(wanted) = args.volume {
        guard(&name, &wanted)?;
    }
    let views: Vec<&Path> = std::iter::once(root.at.as_path())
        .chain(fork.as_ref().map(|f| f.spec.at.as_path()))
        .collect();
    mountpoint::outside_the_state_area(state, &views)?;
    let boot = settings::at_boot(state, &views)?;
    let otel = crate::otel::init(name.as_str(), &identity(name.as_str(), &args.pod));
    telemetry::observe_process(otel.meter(), &boot.not_built);
    let expect: Vec<mountpoint::Expect> = std::iter::once(mountpoint::Expect::Source(
        agentfs_fuse::tree_view::view_source(name.as_str(), TreeId::Root),
    ))
    .chain(fork.as_ref().map(|f| match f.key {
        Some(_) => mountpoint::Expect::KeyedFork {
            volume: name.as_str().to_owned(),
        },
        None => mountpoint::Expect::UnkeyedFork,
    }))
    .collect();
    let stacked =
        match mountpoint::prepare(&views, &expect, args.on_stale_mount, args.seal_mountpoint)? {
            mountpoint::Plan::Mount(stacked) => stacked,
            mountpoint::Plan::Hold(stale) => {
                let stale: Vec<(&Path, &str)> = stale
                    .iter()
                    .map(|(path, source)| (path.as_path(), source.as_str()))
                    .collect();
                // Mounts nothing and takes no lock.
                let code = crate::hold::hold(Daemon::start()?, &stale);
                drop(otel);
                return Ok(code);
            }
        };
    root.over_stale = stacked[0];
    if let Some(fork) = fork.as_mut() {
        fork.spec.over_stale = stacked.get(1).copied().unwrap_or(false);
    }
    let mount = Mount {
        at,
        allow_other: !args.no_allow_other,
        metrics_output: args.metrics_output,
        command: args.command,
        volume: name,
        settings: boot.settings,
        meter: otel.meter().clone(),
    };
    let code = mount.run(state, root, fork, minter);
    drop(otel);
    code
}

/// The telemetry identity of a mount: its pod flags, the replica the pod
/// name or else the host name.
fn identity(volume: &str, pod: &PodArgs) -> Identity {
    let replica = if pod.pod.is_empty() {
        crate::hostname()
    } else {
        pod.pod.clone()
    };
    Identity {
        volume: volume.to_owned(),
        namespace: pod.namespace.clone(),
        pod: pod.pod.clone(),
        pod_uid: pod.pod_uid.clone(),
        node: pod.node.clone(),
        replica,
    }
}

/// How every tree volume this binary opens keeps its journal: `mount` and
/// `host` (through `serving`), `init` and `fork end`, each through its
/// [`Launcher`]. A failpoints build
/// with `AGENTFS_COMPACT_GATE` set gives each a compaction gate on the FIFOs
/// in that directory: one frame per segment, a compaction at every built
/// event, parked at each gap until the gate's holder answers
/// (docs/configuration.md, "Test builds only"). A gate that cannot open
/// aborts, as the take gate's does: a test must not pass with the compactor
/// running by itself. Any other build, or no variable: the defaults.
pub(crate) fn upkeep() -> Upkeep {
    #[cfg(feature = "failpoints")]
    {
        use agentfs_engine::tree_actor::COMPACT_GATE_VAR;
        use agentfs_failpoints::FifoGate;
        if let Some(dir) = std::env::var_os(COMPACT_GATE_VAR) {
            match FifoGate::open(Path::new(&dir)) {
                Ok(gate) => return Upkeep::gated(std::sync::Arc::new(gate)),
                Err(e) => {
                    eprintln!("{COMPACT_GATE_VAR}: {e}");
                    std::process::abort();
                }
            }
        }
    }
    Upkeep::default()
}

/// `agentfs fork end`: the keyed fork ends, and drops at once when nothing
/// holds it (else at a later mount's settle).
///
/// # Errors
///
/// A failed recovery or end.
pub fn end_fork(state: &Path, args: EndArgs, minter: Minter) -> Result<ExitCode> {
    let at = context(&args.pod)?;
    let key = ForkKey::parse(&args.fork_key)?;
    let launcher = root_launcher(minter, at)?;
    match agentfs_service::end_fork(state, launcher, &key) {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(ServiceError::Volume(VolumeError::Busy)) => Ok(busy()),
        Err(ServiceError::Volume(e)) => bail!("opening tree state: {e}"),
        Err(ServiceError::Fork(refused @ TreeError::KeyUnknown)) => {
            eprintln!("error: {refused}");
            Ok(ExitCode::FAILURE)
        }
        Err(ServiceError::Fork(e)) => bail!("ending the fork: {e}"),
        Err(e) => Err(e.into()),
    }
}

/// `mount`'s own triggers: the exit of `-- COMMAND`.
enum MountTrigger {
    CommandExited(std::io::Result<std::process::ExitStatus>),
}

/// `mount`, once its flags are read and its mount points prepared.
struct Mount {
    /// The pod context of every client it attaches.
    at: PodContext,
    allow_other: bool,
    metrics_output: Option<PathBuf>,
    /// `-- COMMAND`: empty for none.
    command: Vec<OsString>,
    volume: VolumeName,
    /// The operator settings read at boot: the volume is staged with them.
    settings: OperatorSettings,
    /// The process's meter: the flagged saves' gauge registers on it.
    meter: Meter,
}

/// What a view that could not open says.
fn open_failure(at: &Path, error: &ViewError) -> String {
    match error {
        ViewError::Stale { found } => format!(
            "mounting {}: it is a stale agentfs mount ({found}) this mount does not stack \
             over: another volume's, or --on-stale-mount is not recover; detach it first: \
             umount -l {}",
            at.display(),
            at.display()
        ),
        e => format!("mounting {}: {e}", at.display()),
    }
}

/// `mount`'s part of the shared loop: the command told to stop at a stop,
/// its exit a stop, and the last view ending a stop.
struct MountDriver {
    running: Option<command::Running>,
    status: Option<u8>,
}

impl serving::Driver<MountTrigger> for MountDriver {
    fn stopping(&mut self, _: &mut Serving<MountTrigger>) {
        if let Some(command) = &self.running {
            command.terminate();
        }
    }

    fn view_ended(&mut self, serving: &mut Serving<MountTrigger>, _: ViewId) {
        if serving.views.is_empty() {
            serving.stop();
        }
    }

    fn own(&mut self, serving: &mut Serving<MountTrigger>, trigger: MountTrigger) {
        let MountTrigger::CommandExited(exited) = trigger;
        self.running = None;
        self.status = Some(command::exit_code(&exited));
        if let Err(e) = &exited {
            eprintln!("error: waiting for the command: {e}");
        }
        serving.stop();
    }
}

impl Mount {
    /// Stages the volume (it recovers it, and never creates one); restores
    /// Trees and Clients on one store; makes or finds the fork view's fork;
    /// per view, rebinds the restored client of this launcher, tree, label
    /// and access, else attaches one; reconciles the rest (a view host dies
    /// with its process) and drops what may drop; serves each view on its
    /// session and detaches its client when it ends; once every view is up,
    /// starts the command. A stop (SIGTERM, SIGINT, the command's exit, or
    /// every view unmounted) unstages: the clients are detached, their opens
    /// saved, the views closed, and the process exits once the volume is
    /// released, with the command's status when that is not 0.
    fn run(
        self,
        state: &Path,
        root: ViewSpec,
        fork: Option<ForkView>,
        minter: Minter,
    ) -> Result<ExitCode> {
        let launcher = root_launcher(minter, self.at.clone())?;
        let staged = serving::stage::<MountTrigger>(state, launcher, &self.settings)?;
        let mut staged = match staged {
            Ok(staged) => staged,
            Err(code) => return Ok(code),
        };
        let _gauge = observe_unverified(&self.meter, &staged.domains)?;
        let domains = &mut staged.domains;
        let mut specs = vec![root];
        if let Some(ForkView { mut spec, key }) = fork {
            let automatic = ForkSettings {
                landing: Landing::Automatic,
            };
            spec.tree = match domains.fork(TreeId::Root, key, automatic) {
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
        let mut bound: Vec<(PathBuf, TreeId, bool, Binding)> = Vec::new();
        for spec in specs {
            match domains.bind(spec.tree, spec.label, spec.access, self.at.clone()) {
                Ok(binding) => bound.push((spec.at, spec.tree, spec.over_stale, binding)),
                Err(refused) => {
                    let ids: Vec<ClientId> = bound.into_iter().map(|(.., b)| b.client).collect();
                    domains.detach_all(&ids);
                    if refused == ClientError::ForkTaken {
                        eprintln!("error: {FORK_TAKEN}");
                        return Ok(ExitCode::FAILURE);
                    }
                    bail!("attaching the client of {}: {refused}", spec.tree);
                }
            }
        }
        let live: ClientSet = bound.iter().map(|(.., b)| b.client.clone()).collect();
        domains.reconcile(&live);
        domains.settle();
        let (mut serving, sender) = Serving::start(staged, state, self.metrics_output.as_deref())?;
        let mut unopened: Vec<ClientId> = Vec::new();
        let mut opened: Vec<(PathBuf, TreeId)> = Vec::new();
        let mut failed = None;
        for (at, tree, over_stale, binding) in bound {
            let client = binding.client.clone();
            if failed.is_some() {
                unopened.push(client);
                continue;
            }
            let options = MountOptions {
                allow_other: self.allow_other,
                over_stale,
            };
            match serving.open(&at, binding, options) {
                Ok(_) => opened.push((at, tree)),
                Err(e) => {
                    failed = Some(open_failure(&at, &e));
                    unopened.push(client);
                    serving.close_all();
                }
            }
        }
        serving.domains.detach_all(&unopened);
        let mut driver = MountDriver {
            running: None,
            status: None,
        };
        if serving.views.is_empty() {
            serving.stop();
        } else if failed.is_none() {
            let mut line = format!("mounted volume {}", self.volume);
            for (i, (at, tree)) in opened.iter().enumerate() {
                if i == 0 {
                    line.push_str(&format!(" at {}", at.display()));
                } else {
                    line.push_str(&format!(", fork {tree} at {}", at.display()));
                }
            }
            eprintln!("{line}; stop with SIGINT or SIGTERM");
            if !self.command.is_empty() {
                match command::spawn(&self.command, sender.clone()) {
                    Ok(started) => driver.running = Some(started),
                    Err(e) => {
                        eprintln!("error: starting {}: {e}", self.command[0].to_string_lossy());
                        driver.status = Some(EXIT_NOT_STARTED);
                        serving.stop();
                    }
                }
            }
        }
        drop(sender);
        serving.run(&mut driver);
        // The views are gone: a command still running is told to stop, and
        // its status is waited for.
        if let Some(command) = driver.running.take() {
            command.terminate();
            while let Some(trigger) = serving.next() {
                if let Trigger::Own(MountTrigger::CommandExited(exited)) = trigger {
                    driver.status = Some(command::exit_code(&exited));
                    break;
                }
            }
        }
        serving.finish()?;
        if let Some(failed) = failed {
            bail!("serving tree views: {failed}");
        }
        Ok(match driver.status {
            Some(code) if code != 0 => ExitCode::from(code),
            _ => ExitCode::SUCCESS,
        })
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    fn parse(args: &[&str]) -> crate::Cli {
        crate::Cli::try_parse_from(args).expect("parses")
    }

    /// The mount point never lands in the global `--root` (an arg id clash
    /// does exactly that, and `mount` then refuses to run).
    #[test]
    fn mount_point_is_not_the_global_root() {
        let cli = parse(&[
            "agentfs",
            "--state",
            "s",
            "mount",
            "m",
            "--fork-mount",
            "x",
            "--fork-key",
            "pvc-1",
        ]);
        assert!(cli.root.is_none(), "positional leaked into --root");
        let crate::Command::Mount(args) = cli.command else {
            panic!("not mount");
        };
        assert_eq!(args.mountpoint, std::path::PathBuf::from("m"));
        assert!(args.command.is_empty());
    }

    #[test]
    fn init_needs_a_volume() {
        assert!(crate::Cli::try_parse_from(["agentfs", "--state", "s", "init"]).is_err());
        let cli = parse(&[
            "agentfs", "--state", "s", "init", "--volume", "t", "--owner", "10:20", "--pod", "p-1",
        ]);
        let crate::Command::Init(args) = cli.command else {
            panic!("not init");
        };
        assert_eq!(args.volume, "t");
        assert_eq!(args.pod.pod, "p-1");
        assert_eq!(args.owner.map(|o| (o.uid, o.gid)), Some((10, 20)));
        assert!(
            crate::Cli::try_parse_from(["agentfs", "init", "--volume", "t", "--owner", "x"])
                .is_err()
        );
    }

    #[test]
    fn mount_takes_a_command_after_two_dashes() {
        let cli = parse(&["agentfs", "mount", "m", "--", "sh", "-c", "x"]);
        let crate::Command::Mount(args) = cli.command else {
            panic!("not mount");
        };
        assert_eq!(args.mountpoint, std::path::PathBuf::from("m"));
        assert_eq!(args.command, ["sh", "-c", "x"]);
    }

    #[test]
    fn fork_end_takes_a_key() {
        let cli = parse(&["agentfs", "fork", "end", "--fork-key", "k"]);
        let crate::Command::Fork {
            command: crate::fork_cmd::ForkCommand::End(args),
        } = cli.command
        else {
            panic!("not fork end");
        };
        assert_eq!(args.fork_key, "k");
    }

    /// `init`, `mount`, `host` and `fork end` take `--state` only: a given `--root`
    /// (or `AGENTFS_ROOT`) or `--wait-lock` exits 1 before anything runs.
    #[test]
    fn the_tree_commands_refuse_root_and_wait_lock() {
        for args in [
            &["agentfs", "--root", "b", "init", "--volume", "t"][..],
            &["agentfs", "--root", "b", "mount", "m"][..],
            &[
                "agentfs",
                "--root",
                "b",
                "host",
                "--launcher",
                "csi",
                "--volume",
                "t",
            ][..],
            &[
                "agentfs",
                "--wait-lock",
                "1s",
                "host",
                "--launcher",
                "csi",
                "--volume",
                "t",
            ][..],
            &["agentfs", "--root", "b", "fork", "end", "--fork-key", "k"][..],
            &["agentfs", "--wait-lock", "1s", "init", "--volume", "t"][..],
        ] {
            let cli = parse(args);
            let refused = crate::tree_command_flags(&cli).expect_err("refused");
            assert!(refused.contains("take --state only"), "{args:?}: {refused}");
        }
        let cli = parse(&["agentfs", "init", "--volume", "t"]);
        assert!(crate::tree_command_flags(&cli).is_ok());
    }

    #[test]
    fn probe_has_no_sandbox_overlay() {
        assert!(crate::Cli::try_parse_from(["agentfs", "probe", "m"]).is_ok());
        assert!(
            crate::Cli::try_parse_from(["agentfs", "probe", "--sandbox-overlay", "m"]).is_err()
        );
    }
}
