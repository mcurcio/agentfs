//! `agentfs host` (docs/library/filesystem.md, "The view host command"): one
//! tree volume's view host, run by a parent that opens and closes views on
//! it and creates and ends keyed forks, over the host's standard input (a
//! Unix socket only the parent holds the other end of), one JSON object per
//! line ([`agentfs_contracts::host`]). It boots as `mount` does, then writes
//! [`Hello`] and answers each request in order. The parent closing its end
//! (EOF), SIGTERM or SIGINT stops it: with no view open it drains; with a
//! view open it exits at once, each view's mount left stale and its client
//! attached, as a crash leaves them, so the next host rebinds and stacks
//! over them (the views belong to pods the parent does not own). The last
//! view ending does not stop it. Prior art: the LSP and MCP stdio
//! transports, Erlang ports.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;

use agentfs_contracts::caps::{LaunchCap, LaunchScope, Minter};
use agentfs_contracts::clients::{Access, ClientError, ClientSet};
use agentfs_contracts::host::{
    HOST_PROTOCOL, Hello, Refusal, RefusalKind, Reply, Request, TreeRef, ViewRequest,
};
use agentfs_contracts::name::{ClientLabel, ForkKey, LauncherId};
use agentfs_contracts::trees::{ForkPhase, ForkSettings, TreeError};
use agentfs_contracts::views::{MountOptions, ViewError, ViewId};
use agentfs_core::tree_actor::TreeId;
use anyhow::{Context as _, Result};

use super::serving::{self, Driver, Serving, Trigger};
use super::{context, found, guard, identity, settings};
use crate::pod_args::PodArgs;

/// Exit status of a host not run by a launcher, or told a malformed line.
const EXIT_USAGE: u8 = 2;

/// Exit status of a host stopped with views open: they are left stale for
/// the next host, and the volume is not released.
const EXIT_LEFT_OPEN: u8 = 3;

/// `agentfs host`'s flags.
#[derive(clap::Args)]
pub struct HostArgs {
    /// The launcher its clients and forks are recorded under (a launcher
    /// id): what a restarted host rebinds by.
    #[arg(long)]
    pub launcher: String,
    /// A guard: refuse, before any write, a state area whose volume has
    /// another name.
    #[arg(long, env = "AGENTFS_VOLUME")]
    pub volume: String,
    #[command(flatten)]
    pub pod: PodArgs,
}

/// The host's own triggers: each line its parent wrote.
enum Line {
    Request(Request),
    Malformed(String),
}

/// The control channel: the standard input, when it is a Unix stream
/// socket, through an owned clone of it.
fn control_channel() -> Option<UnixStream> {
    let stdin = std::io::stdin();
    let socket_type = rustix::net::sockopt::socket_type(stdin.as_fd()).ok()?;
    if socket_type != rustix::net::SocketType::STREAM {
        return None;
    }
    let stream = UnixStream::from(stdin.as_fd().try_clone_to_owned().ok()?);
    // A stream socket of another family has no Unix address.
    stream.local_addr().ok()?;
    Some(stream)
}

/// `agentfs host`: the control channel checked before the state area is
/// touched; then the volume checked, its operator settings logged, the
/// volume staged and its domains restored as `--launcher`, the view host
/// started and [`Hello`] written; then each request answered until a stop.
/// Exits 0 once the volume is released, 1 for a refusal at boot or a
/// stopped actor, 2 without a control channel or after a malformed line,
/// 3 when stopped with views open (left stale), 75 while another process
/// holds the volume.
///
/// # Errors
///
/// A refusal or failure, before or while serving.
pub fn host(state: &Path, args: HostArgs, minter: Minter) -> Result<ExitCode> {
    let Some(control) = control_channel() else {
        eprintln!(
            "error: agentfs host is run by a launcher that controls it over its standard input, \
             a Unix socket"
        );
        return Ok(ExitCode::from(EXIT_USAGE));
    };
    let at = context(&args.pod)?;
    let launcher = LauncherId::parse(&args.launcher).context("reading --launcher")?;
    let volume = match found(state)? {
        Ok(volume) => volume,
        Err(code) => return Ok(code),
    };
    guard(&volume.name, &args.volume)?;
    let not_built = settings::at_boot(state, &[])?;
    let otel = crate::otel::init(
        volume.name.as_str(),
        &identity(volume.name.as_str(), &args.pod),
    );
    otel.telemetry().observe_not_built(&not_built);
    let cap = LaunchCap::mint(&minter, launcher, LaunchScope::Root);
    let staged = match serving::stage::<Line>(state, &at, cap, minter)? {
        Ok(staged) => staged,
        Err(code) => return Ok(code),
    };
    let (mut serving, sender) = Serving::start(staged, state, None)?;
    let mut driver = HostDriver {
        control: control.try_clone().context("cloning the control channel")?,
        open: BTreeMap::new(),
        closing: None,
        deferred: VecDeque::new(),
        stopping: false,
        malformed: false,
    };
    let hello = Hello {
        v: HOST_PROTOCOL,
        volume: volume.name.clone(),
        id: volume.id,
    };
    driver.write(&mut serving, &hello);
    read_requests(control, sender)?;
    serving.run(&mut driver);
    if serving.left() {
        let open = serving.abandon();
        eprintln!(
            "stopped with {open} view(s) open: each mount is left stale (ENOTCONN) and its \
             client attached, for the next host to rebind and stack over"
        );
        drop(otel);
        return Ok(ExitCode::from(if driver.malformed {
            EXIT_USAGE
        } else {
            EXIT_LEFT_OPEN
        }));
    }
    let finished = serving.finish();
    drop(otel);
    finished?;
    Ok(if driver.malformed {
        ExitCode::from(EXIT_USAGE)
    } else {
        ExitCode::SUCCESS
    })
}

/// Reads the parent's lines on a thread of its own, each a trigger of the
/// loop. EOF or a failed read is a stop; a malformed line is the last one
/// read.
fn read_requests(control: UnixStream, sender: mpsc::Sender<Trigger<Line>>) -> Result<()> {
    std::thread::Builder::new()
        .name("host-requests".into())
        .spawn(move || {
            for line in BufReader::new(control).lines() {
                let Ok(line) = line else { break };
                let (trigger, last) = match serde_json::from_str::<Request>(&line) {
                    Ok(request) => (Line::Request(request), false),
                    Err(e) => (Line::Malformed(e.to_string()), true),
                };
                // The receiver goes only once the loop has ended.
                if sender.send(Trigger::Own(trigger)).is_err() || last {
                    return;
                }
            }
            let _ = sender.send(Trigger::Stop);
        })
        .context("starting the request thread")?;
    Ok(())
}

/// A view the parent opened, by where it is mounted.
struct OpenView {
    view: ViewId,
    tree: TreeId,
    label: ClientLabel,
    access: Access,
}

/// The host's part of the shared loop: each request answered in order.
struct HostDriver {
    control: UnixStream,
    open: BTreeMap<PathBuf, OpenView>,
    /// The view a `close` waits on: its reply is written once it has ended.
    closing: Option<ViewId>,
    /// Requests that came while a `close` waited, answered after it.
    deferred: VecDeque<Request>,
    stopping: bool,
    malformed: bool,
}

/// Whether a stop leaves the views mounted rather than draining: some view
/// is open and the actor still records (a stopped actor's views are closed,
/// as nothing more can be recorded through them).
fn leaves(serving: &Serving<Line>) -> bool {
    !serving.views.is_empty() && !serving.domains.stopped()
}

fn refused(kind: RefusalKind, message: impl ToString) -> Reply {
    Reply::Refused(Refusal {
        kind,
        message: message.to_string(),
    })
}

impl Driver<Line> for HostDriver {
    fn stopping(&mut self, serving: &mut Serving<Line>) {
        self.stopping = true;
        if leaves(serving) {
            serving.leave();
        }
    }

    fn view_ended(&mut self, serving: &mut Serving<Line>, view: ViewId) {
        self.open.retain(|_, open| open.view != view);
        if self.closing == Some(view) {
            self.closing = None;
            self.write(serving, &Reply::Closed);
            while self.closing.is_none()
                && let Some(request) = self.deferred.pop_front()
            {
                self.handle(serving, request);
            }
        }
    }

    fn own(&mut self, serving: &mut Serving<Line>, trigger: Line) {
        match trigger {
            Line::Request(request) if self.closing.is_some() => self.deferred.push_back(request),
            Line::Request(request) => self.handle(serving, request),
            Line::Malformed(e) => {
                eprintln!("error: host request: {e}");
                self.malformed = true;
                self.stop(serving);
            }
        }
    }
}

impl HostDriver {
    /// Writes one line to the parent. A failed write (the parent gone) is a
    /// stop.
    fn write(&mut self, serving: &mut Serving<Line>, value: &impl serde::Serialize) {
        let written = serde_json::to_string(value)
            .map_err(std::io::Error::from)
            .and_then(|line| writeln!(self.control, "{line}"));
        if let Err(e) = written {
            eprintln!("error: host reply: {e}");
            self.stop(serving);
        }
    }

    /// A stop of the host's own (EOF is the reader's [`Trigger::Stop`]).
    fn stop(&mut self, serving: &mut Serving<Line>) {
        if !self.stopping {
            self.stopping = true;
            if leaves(serving) {
                serving.leave();
            } else {
                serving.stop();
            }
        }
    }

    /// Answers `request`; a `close` of an open view answers once it ends.
    fn handle(&mut self, serving: &mut Serving<Line>, request: Request) {
        let reply = if self.stopping {
            Some(refused(
                RefusalKind::Draining,
                "the volume is draining: no more requests",
            ))
        } else if serving.domains.stopped() {
            self.stop(serving);
            Some(refused(
                RefusalKind::Stopped,
                "the volume has stopped after a recording failure; restart the host",
            ))
        } else {
            match request {
                Request::Open(request) => Some(self.open(serving, request)),
                Request::Close { at } => self.close(serving, &at),
                Request::Reconcile => {
                    let live: ClientSet = serving.views.values().cloned().collect();
                    let ended = serving.domains.reconcile(&live);
                    serving.domains.settle();
                    Some(Reply::Reconciled { ended })
                }
                Request::ForkCreate {
                    upstream,
                    key,
                    settings,
                } => Some(fork_create(serving, &upstream, key, settings)),
                Request::ForkEnd { key } => Some(fork_end(serving, &key)),
            }
        };
        if let Some(reply) = reply {
            self.write(serving, &reply);
        }
    }

    /// `open`: the view of an open tree at `at`, its client rebound or
    /// attached with the request's pod context, stacked over a stale mount
    /// of its own source.
    fn open(&mut self, serving: &mut Serving<Line>, request: ViewRequest) -> Reply {
        let tree = match resolve(serving, &request.tree) {
            Ok(tree) => tree,
            Err(refusal) => return refusal,
        };
        let access = if request.read_only {
            Access::ReadOnly
        } else {
            Access::ReadWrite
        };
        if let Some(open) = self.open.get(&request.at) {
            return if open.tree == tree && open.label == request.label && open.access == access {
                Reply::Opened {
                    tree,
                    stacked: false,
                }
            } else {
                refused(
                    RefusalKind::Occupied,
                    format!(
                        "a view of {} is open at {} with another label or access",
                        open.tree,
                        request.at.display()
                    ),
                )
            };
        }
        let binding =
            match serving
                .domains
                .bind(tree, request.label.clone(), access, request.pod.clone())
            {
                Ok(binding) => binding,
                Err(refusal) => return client_refusal(&refusal),
            };
        let client = binding.client.clone();
        let options = MountOptions {
            allow_other: true,
            over_stale: true,
        };
        match serving.open(&request.at, binding, options) {
            Ok((view, stacked)) => {
                self.open.insert(
                    request.at,
                    OpenView {
                        view,
                        tree,
                        label: request.label,
                        access,
                    },
                );
                Reply::Opened { tree, stacked }
            }
            Err(e) => {
                serving.domains.detach_all(&[client]);
                let kind = match e {
                    ViewError::Stale { .. } => RefusalKind::Stale,
                    ViewError::MountFailed(_) | ViewError::UnknownSession => {
                        RefusalKind::MountFailed
                    }
                };
                refused(kind, format!("mounting {}: {e}", request.at.display()))
            }
        }
    }

    /// `close`: an unknown mount point is closed already; an open view's
    /// reply waits for its end.
    fn close(&mut self, serving: &mut Serving<Line>, at: &Path) -> Option<Reply> {
        let Some(open) = self.open.get(at) else {
            return Some(Reply::Closed);
        };
        self.closing = Some(open.view);
        serving.close(open.view);
        None
    }
}

/// The tree a request names: the root, or the open fork under a key (never
/// made here).
fn resolve(serving: &mut Serving<Line>, tree: &TreeRef) -> Result<TreeId, Reply> {
    let key = match tree {
        TreeRef::Root => return Ok(TreeId::Root),
        TreeRef::Fork(key) => key,
    };
    match serving.domains.keyed(key) {
        Some((tree, ForkPhase::Open)) => Ok(tree),
        Some(_) => Err(refused(
            RefusalKind::KeyEnded,
            format!("the fork under {key} has ended; use a new key"),
        )),
        None => Err(refused(
            RefusalKind::KeyUnknown,
            format!("no fork under {key}"),
        )),
    }
}

/// `fork_create`: the open fork under `key`, or a new fork of `upstream`.
fn fork_create(
    serving: &mut Serving<Line>,
    upstream: &TreeRef,
    key: ForkKey,
    settings: ForkSettings,
) -> Reply {
    let upstream = match resolve(serving, upstream) {
        Ok(tree) => tree,
        Err(refusal) => return refusal,
    };
    match serving.domains.fork(upstream, Some(key), settings) {
        Ok(tree) => Reply::Forked { tree },
        Err(refusal) => tree_refusal(&refusal),
    }
}

/// `fork_end`: the keyed fork ends, and drops once nothing holds it.
fn fork_end(serving: &mut Serving<Line>, key: &ForkKey) -> Reply {
    match serving.domains.end_keyed(key) {
        Ok(()) => {
            serving.domains.settle();
            Reply::Ended
        }
        Err(refusal) => tree_refusal(&refusal),
    }
}

/// A Trees refusal as the parent reads it.
fn tree_refusal(refusal: &TreeError) -> Reply {
    let kind = match refusal {
        TreeError::NestedNotBuilt => RefusalKind::NestedNotBuilt,
        TreeError::ApprovalNotBuilt => RefusalKind::ApprovalNotBuilt,
        TreeError::Ended => RefusalKind::KeyEnded,
        TreeError::KeyUnknown | TreeError::UpstreamUnknown | TreeError::ForkUnknown => {
            RefusalKind::KeyUnknown
        }
        // A row that could not be recorded: nothing more is, until the
        // volume is reopened.
        TreeError::NotPermitted | TreeError::Io(_) | TreeError::Stopped => RefusalKind::Stopped,
    };
    refused(kind, refusal)
}

/// A Clients refusal as the parent reads it.
fn client_refusal(refusal: &ClientError) -> Reply {
    let kind = match refusal {
        ClientError::ForkTaken => RefusalKind::ForkTaken,
        ClientError::TreeUnknown => RefusalKind::KeyEnded,
        ClientError::Draining => RefusalKind::Draining,
        ClientError::Stopped | ClientError::NotDurable(_) => RefusalKind::Stopped,
        // The view could not be served: no client was bound.
        ClientError::NotPermitted | ClientError::UnknownClient | ClientError::NotItsLauncher => {
            RefusalKind::MountFailed
        }
    };
    refused(kind, refusal)
}
