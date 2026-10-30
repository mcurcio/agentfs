//! `agentfs fork` (design §11.5, §17.1): create a fork of a volume,
//! finish and publish it, read its status, abandon it, and list the
//! published changes on a fork remote.
//!
//! - `fork create` makes the fork's state area and backing tree from the
//!   source's state area, read without the source's lock; the fork reads
//!   its settings from the source's included files
//!   ([`agentfs_core::fork::create::settings_files`]). `--link unlinked`
//!   makes an independent fork, which never delivers to its source and
//!   pushes only to the remotes in its own `--remotes` file (none:
//!   local-only).
//! - `fork finish` takes the fork's state-area lock, finishes the fork
//!   once and publishes it to its fork remote, retrying with backoff;
//!   `--retry` clears a refused publish first. A fork of a local-only
//!   source publishes into the source's history instead (F8 S3): through
//!   the source's running daemon ([`serve_receive`]), else under the
//!   source's lock (exit 75 while another command holds it).
//! - `fork status` reads the fork's state, record and publish result,
//!   with no lock and no network.
//! - `fork abandon` records the operator's discard, with the state lock.
//! - `fork list` runs in a receiver's git repository, with its own git and
//!   credentials, never a state area ([`agentfs_core::fork::listing`]).
//! - `fork deliver` runs there too: it reads the fork's published change,
//!   checked as `fork list` checks it, and delivers it by one method
//!   ([`agentfs_core::fork::deliver`]).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;

use agentfs_core::config::Settings;
use agentfs_core::events::Event;
use agentfs_core::fork::create::{self, CreateRequest};
use agentfs_core::fork::deliver::{self, DEFAULT_PREFIX, Delivered, DeliveryMethod, Shape};
use agentfs_core::fork::finish::{
    self, AbandonedMark, FinishError, FinishRecord, Finisher, ForkState, ForkStatus,
    PublishOutcome, RefusedMark, RefusedReason,
};
use agentfs_core::fork::identity::{ForkIdentity, ForkLink, full_commit};
use agentfs_core::fork::listing;
use agentfs_core::fork::receive::{
    self, ReceiveError, Receiver, ReceiverError, SOURCE_HISTORY, UnderLock,
};
use agentfs_core::lock::StateLock;
use agentfs_core::patch::submit::GitCli;
use agentfs_core::record::Caller;
use agentfs_core::volume::StateDir;

use crate::{Pod, Session, control};

/// Where an operator reads what to do about a refused publish: one section
/// per [`RefusedReason`], headed by its name.
const REFUSED_RUNBOOK: &str = "docs/runbooks/fork-refused.md";
/// The runbook section for a fork of a fork whose parent finished first.
const PARENT_FINISHED_RUNBOOK: &str = "docs/runbooks/patch-queue.md#9-forks-of-forks";

/// What to fix for a publish refused for `reason`, and the runbook
/// section that says how.
fn refused_hint(reason: RefusedReason) -> String {
    let fix = match reason {
        RefusedReason::AnotherWriter => {
            "a change or finish ref on the fork remote holds another commit, which agentfs \
             never moves: find who wrote it"
        }
        RefusedReason::TooLarge => {
            "a commit or an LFS object is over the fork remote's agentfsMaxPushSize or \
             agentfsMaxLfsObjectSize"
        }
        RefusedReason::Rejected => {
            "the fork remote refuses refs/agentfs/<fork>/change and /finish (a hook or a \
             protection rule)"
        }
        RefusedReason::RemoteGone => {
            "the fork's settings no longer have the fork remote its finish names"
        }
        RefusedReason::Unsafe => {
            "the local-only source refuses what the fork brings: a ref that is the source's own, \
             a base off the source's branch, or an LFS object that is not its name"
        }
    };
    format!("{fix}; see {REFUSED_RUNBOOK}#{}", reason.as_str())
}

#[derive(Subcommand)]
pub enum ForkCommand {
    /// Create a fork of the volume at --source-state in --state, its
    /// backing tree at --root (created if missing): base's tree written,
    /// nothing mounted, no lock on the source. Prints `fork <name> of
    /// <source> at <base>`.
    Create(CreateArgs),
    /// Finish the fork (once) and publish it to its fork remote (a fork of
    /// a fork: submit its change to its parent's inbox there), retrying
    /// with backoff: exit 0 published or submitted, 1 not yet at --timeout
    /// or refused, 75 when the state area is busy.
    Finish(FinishArgs),
    /// The fork's state, record and publish result (no lock, no network).
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Discard the fork on record (state lock; exit 75 when busy): nothing
    /// agentfs runs pushes it again, and DeleteVolume may proceed.
    Abandon {
        #[arg(long, value_name = "TEXT")]
        reason: String,
    },
    /// Published changes on a fork remote, checked (run in the receiver's
    /// git repository).
    List(ListArgs),
    /// Deliver a published fork's change to a receiver, from the
    /// receiver's git repository; exit 0 delivered or already, 1 refused,
    /// 75 unavailable (retry).
    Deliver(DeliverArgs),
}

#[derive(Args)]
pub struct CreateArgs {
    /// The source volume's state area.
    #[arg(long, value_name = "DIR")]
    pub source_state: PathBuf,
    /// The fork's name before its random suffix (default: the source's
    /// name).
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// linked: its finish delivers the change back to the source;
    /// unlinked: an independent fork, nothing ever delivered (design
    /// §11.5). Fixed at create.
    #[arg(long, value_enum, default_value = "linked")]
    pub link: LinkArg,
    /// With --link unlinked: a config file of the fork's own `[remote
    /// "<name>"]` sections (agentfs.fork.remotes), read at every open; it
    /// never pushes to its source's remotes. Without it the fork is
    /// local-only, and removing its state area loses its history.
    #[arg(long, value_name = "FILE")]
    pub remotes: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LinkArg {
    Linked,
    Unlinked,
}

impl From<LinkArg> for ForkLink {
    fn from(arg: LinkArg) -> Self {
        match arg {
            LinkArg::Linked => Self::Linked,
            LinkArg::Unlinked => Self::Unlinked,
        }
    }
}

#[derive(Args)]
pub struct FinishArgs {
    /// Stop publishing after this long and exit 1 (default: until
    /// published).
    #[arg(long, value_parser = crate::parse_grace, value_name = "DURATION")]
    pub timeout: Option<Duration>,
    /// Clear a refused publish and try again.
    #[arg(long)]
    pub retry: bool,
    #[command(flatten)]
    pub pod: Pod,
}

#[derive(Args)]
pub struct ListArgs {
    /// The fork remote: a remote name or URL of the --repo repository.
    #[arg(long, default_value = "origin")]
    remote: String,
    /// Only the changes of forks of this source volume.
    #[arg(long, value_name = "VOLUME")]
    source: Option<String>,
    /// The receiver's git repository, whose git and credentials are used.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub struct DeliverArgs {
    /// The fork to deliver.
    pub fork: String,
    /// The fork remote (name or URL) the change is read from.
    #[arg(long, default_value = "origin")]
    pub from: String,
    /// The delivery method.
    #[arg(long, value_enum)]
    pub to: DeliverTo,
    /// inbox: the volume to land it in.
    #[arg(long)]
    pub volume: Option<String>,
    /// inbox and branch: where to push (default: --from).
    #[arg(long)]
    pub remote: Option<String>,
    /// branch: the branch is <prefix><fork>.
    #[arg(long, default_value = DEFAULT_PREFIX)]
    pub prefix: String,
    /// branch: one squashed commit, or the fork's history.
    #[arg(long, value_enum, default_value = "squash")]
    pub shape: ShapeArg,
    /// branch, squash: merge the change onto this branch's head.
    #[arg(long, value_name = "BRANCH")]
    pub onto: Option<String>,
    /// command: the program and its arguments, after `--`.
    #[arg(last = true)]
    pub command: Vec<String>,
    /// webhook: the http(s) URL to POST the payload to.
    #[arg(long)]
    pub url: Option<String>,
    /// webhook: a file whose contents are the bearer token.
    #[arg(long, value_name = "FILE")]
    pub credential_file: Option<PathBuf>,
    /// command and webhook: how long the receiver may take.
    #[arg(long, default_value = "60s", value_parser = crate::parse_grace)]
    pub timeout: Duration,
    /// The receiver's git repository, whose git and credentials are used.
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DeliverTo {
    Inbox,
    Branch,
    Command,
    Webhook,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum ShapeArg {
    Squash,
    History,
}

/// `fork status --json`, version 1.
#[derive(Serialize)]
pub struct ForkStatusDoc {
    pub version: u32,
    pub fork: String,
    pub source: String,
    pub base: String,
    pub state: &'static str,
    pub change: Option<String>,
    pub finish: Option<String>,
    pub remote: Option<String>,
    pub published_at_ms: Option<u64>,
    /// The parent fork that had finished first, so this fork of a fork
    /// published as a top-level fork (`ForkParentFinished`); omitted
    /// otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_finished: Option<String>,
    pub refused: Option<RefusedMark>,
    pub abandoned: Option<AbandonedMark>,
}

impl ForkStatusDoc {
    fn new(status: &ForkStatus) -> Self {
        let record = status.record.as_ref();
        let finish = record.map(|r| r.finish.to_string());
        let mark = status.mark.as_ref();
        let names_finish = |f: &str| finish.as_deref() == Some(f);
        let published = mark
            .and_then(|m| m.published.as_ref())
            .filter(|p| names_finish(&p.finish));
        Self {
            version: 1,
            fork: status.identity.fork.clone(),
            source: status.identity.source.clone(),
            base: status.identity.base.to_string(),
            state: status.state.as_str(),
            change: record.and_then(|r| r.change).map(|c| c.to_string()),
            finish: finish.clone(),
            remote: record.map(|r| r.remote.clone()),
            published_at_ms: published.map(|p| p.at_ms),
            parent_finished: published
                .and_then(|p| p.parent_finished.as_ref())
                .map(|p| p.parent.clone()),
            refused: mark
                .and_then(|m| m.refused.clone())
                .filter(|r| names_finish(&r.finish)),
            abandoned: mark.and_then(|m| m.abandoned.clone()),
        }
    }
}

pub fn run(
    command: ForkCommand,
    state: &StateDir,
    root: Option<&Path>,
    wait_lock: Duration,
) -> Result<ExitCode> {
    match command {
        ForkCommand::Create(args) => {
            let Some(root) = root else {
                bail!("fork create needs --root (or AGENTFS_ROOT): the fork's backing tree");
            };
            let _lock = StateLock::acquire(state, "fork create", wait_lock)?;
            Ok(match create_cmd(&args, state, root) {
                Ok(line) => {
                    println!("{line}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            })
        }
        ForkCommand::Finish(args) => {
            let Some(root) = root else {
                bail!("fork finish needs --root (or AGENTFS_ROOT): the fork's backing tree");
            };
            let lock = StateLock::acquire(state, "fork finish", wait_lock)?;
            let session = Session::start(state, &args.pod)?;
            let mut finisher = Finisher::open(
                state,
                root,
                args.pod.context(&session.volume),
                session.events.clone(),
                session.telemetry(),
                &lock,
            )?;
            finisher.set_receiver(Box::new(ThroughService));
            if args.retry {
                finisher.retry_refused()?;
            }
            let record = finisher.finish()?;
            println!("{}", describe(&record));
            let deadline = args.timeout.map(|t| Instant::now() + t);
            let outcome = match finisher.publish(deadline) {
                Ok(outcome) => outcome,
                // Exit 75, as every command meeting a held lock.
                Err(FinishError::SourceBusy(busy)) => return Err(busy.into()),
                Err(e) => return Err(e.into()),
            };
            let code = match outcome {
                PublishOutcome::Published if record.remote == SOURCE_HISTORY => {
                    println!("published into the history of volume {}", record.source);
                    ExitCode::SUCCESS
                }
                PublishOutcome::Published => {
                    println!("published to {}", record.remote);
                    ExitCode::SUCCESS
                }
                PublishOutcome::Submitted { inbox } => {
                    println!(
                        "submitted to the inbox of volume {} as patch {inbox}",
                        record.source
                    );
                    ExitCode::SUCCESS
                }
                PublishOutcome::Pending { kind, message } => {
                    eprintln!("not published yet ({kind}): {message}");
                    ExitCode::FAILURE
                }
                PublishOutcome::Refused { reason, message } => {
                    eprintln!(
                        "publishing is refused ({}): {message}; {}; nothing retries it: fix \
                         the cause and run `agentfs fork finish --retry`, or `agentfs fork \
                         abandon --reason TEXT`",
                        reason.as_str(),
                        refused_hint(reason)
                    );
                    ExitCode::FAILURE
                }
            };
            drop(finisher);
            session.finish();
            Ok(code)
        }
        ForkCommand::Status { json } => {
            let Some(status) = finish::status(state)? else {
                let unlinked = Settings::load(&state.git())
                    .ok()
                    .and_then(|s| s.fork)
                    .filter(|f| f.link == ForkLink::Unlinked);
                if let Some(f) = unlinked {
                    bail!(
                        "fork {} of {} at {} is unlinked: it delivers nothing to its source, so \
                         it has no fork status (its pushes are in `agentfs status`)",
                        f.fork,
                        f.source,
                        f.base
                    );
                }
                bail!("{} is not a fork", state.root().display());
            };
            let doc = ForkStatusDoc::new(&status);
            if json {
                println!("{}", serde_json::to_string_pretty(&doc)?);
            } else {
                print_status(&doc);
            }
            Ok(ExitCode::SUCCESS)
        }
        ForkCommand::Abandon { reason } => {
            let lock = StateLock::acquire(state, "fork abandon", wait_lock)?;
            let before = finish::status(state).ok().flatten().map(|s| s.state);
            let caller = Caller {
                uid: crate::current_uid(),
                gid: crate::current_gid(),
                pid: std::process::id(),
            };
            let status = match finish::abandon(state, &lock, &reason, caller) {
                Ok(status) => status,
                Err(e) => {
                    eprintln!("error: {e}");
                    return Ok(ExitCode::FAILURE);
                }
            };
            let fork = status.identity.fork.clone();
            let recorded = status
                .mark
                .and_then(|m| m.abandoned)
                .map_or_else(String::new, |a| a.reason);
            if before == Some(ForkState::Abandoned) {
                println!("fork {fork} was already abandoned: {recorded}");
            } else {
                let session = Session::start(state, &Pod::default())?;
                session.events.emit(Event::ForkAbandoned {
                    fork: fork.clone(),
                    reason: recorded.clone(),
                });
                session.finish();
                println!("abandoned fork {fork}: {recorded}");
            }
            Ok(ExitCode::SUCCESS)
        }
        ForkCommand::List(args) => {
            let git = GitCli::new(&args.repo);
            let listed = listing::list(&git, &args.remote, args.source.as_deref())?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&listed)?);
            } else {
                for c in &listed.changes {
                    println!(
                        "change {} of fork {} (source {}, base {}, finish {}, history {})",
                        c.change, c.fork, c.source, c.base, c.finish, c.history_ref
                    );
                }
                for s in &listed.skipped {
                    eprintln!("skipped {}: {}", s.ref_name, s.reason);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        ForkCommand::Deliver(args) => deliver_cmd(args),
    }
}

/// Exit status of a delivery to retry: the receiver or a remote is
/// unavailable (`EX_TEMPFAIL`, as a busy state area).
const EXIT_RETRY: u8 = 75;

/// `fork deliver`: read the change, checked; build the method; deliver.
fn deliver_cmd(args: DeliverArgs) -> Result<ExitCode> {
    let method = deliver_method(&args)?;
    let git = GitCli::new(&args.repo);
    let change = match listing::read(&git, &args.from, &args.fork) {
        Ok(Some(change)) => change,
        Ok(None) => {
            eprintln!(
                "error: fork {} is not published on {} (or its change fails a check: see                  `agentfs fork list`)",
                args.fork, args.from
            );
            return Ok(ExitCode::FAILURE);
        }
        Err(listing::ListError::Unreachable(detail)) => {
            eprintln!("error: {detail}");
            return Ok(ExitCode::from(EXIT_RETRY));
        }
        Err(e) => return Err(e.into()),
    };
    let result = deliver::delivery(&method, &git, &args.from).and_then(|d| d.deliver(&change));
    match result {
        Ok(done) => {
            println!("{}", describe_delivered(&done, &change.change));
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            let retry = e.is_retryable();
            eprintln!(
                "error: delivering change {} of fork {}: {e}{}",
                change.change,
                change.fork,
                if retry { " (try again)" } else { "" }
            );
            Ok(if retry {
                ExitCode::from(EXIT_RETRY)
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

/// The method `fork deliver`'s flags name. A flag that belongs to another
/// method is refused rather than ignored.
fn deliver_method(args: &DeliverArgs) -> Result<DeliveryMethod> {
    use DeliverTo::{Branch, Command, Inbox, Webhook};
    let stray = [
        ("--volume", args.volume.is_some(), &[Inbox][..]),
        ("--remote", args.remote.is_some(), &[Inbox, Branch][..]),
        ("--onto", args.onto.is_some(), &[Branch][..]),
        (
            "a command after --",
            !args.command.is_empty(),
            &[Command][..],
        ),
        ("--url", args.url.is_some(), &[Webhook][..]),
        (
            "--credential-file",
            args.credential_file.is_some(),
            &[Webhook][..],
        ),
    ];
    for (flag, given, methods) in stray {
        if given && !methods.contains(&args.to) {
            let to = args.to.to_possible_value().map(|v| v.get_name().to_owned());
            bail!("{flag} does not apply to --to {}", to.unwrap_or_default());
        }
    }
    let remote = || args.remote.clone().unwrap_or_else(|| args.from.clone());
    Ok(match args.to {
        Inbox => DeliveryMethod::Inbox {
            remote: remote(),
            volume: args
                .volume
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--to inbox needs --volume"))?,
            branch: None,
        },
        Branch => DeliveryMethod::Branch {
            remote: remote(),
            prefix: args.prefix.clone(),
            shape: match args.shape {
                ShapeArg::Squash => Shape::Squash,
                ShapeArg::History => Shape::History,
            },
            onto: args.onto.clone(),
        },
        Command => DeliveryMethod::Command {
            argv: args.command.clone(),
            timeout: args.timeout,
        },
        Webhook => DeliveryMethod::Webhook {
            url: args
                .url
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--to webhook needs --url"))?,
            credential_file: args.credential_file.clone(),
            timeout: args.timeout,
        },
    })
}

/// One line for a delivery.
fn describe_delivered(done: &Delivered, change: &str) -> String {
    match done {
        Delivered::Submitted { inbox } => format!("delivered change {change} as inbox {inbox}"),
        Delivered::Pushed { ref_name, commit } => {
            format!("delivered change {change} as {ref_name} at {commit}")
        }
        Delivered::Accepted => format!("delivered change {change}: accepted"),
        Delivered::AlreadyDelivered => format!("change {change} was already delivered"),
    }
}

/// `fork status` as text.
fn print_status(doc: &ForkStatusDoc) {
    let or_none = |v: &Option<String>| v.clone().unwrap_or_else(|| "none".to_owned());
    println!("fork:      {}", doc.fork);
    println!("source:    {}", doc.source);
    println!("base:      {}", doc.base);
    println!("state:     {}", doc.state);
    println!("change:    {}", or_none(&doc.change));
    println!("finish:    {}", or_none(&doc.finish));
    println!("remote:    {}", or_none(&doc.remote));
    let result = if let Some(a) = &doc.abandoned {
        format!(
            "abandoned at {} ms by uid {} while {}: {}",
            a.at_ms, a.uid, a.state, a.reason
        )
    } else if let Some(at) = doc.published_at_ms {
        match &doc.parent_finished {
            Some(parent) => format!(
                "published at {at} ms as a top-level fork: its parent {parent} had finished, \
                 so nothing lands the change (see {PARENT_FINISHED_RUNBOOK})"
            ),
            None => format!("published at {at} ms"),
        }
    } else if let Some(r) = &doc.refused {
        format!(
            "refused ({}) at {} ms by {}: {} (see {REFUSED_RUNBOOK}#{})",
            r.reason.as_str(),
            r.at_ms,
            r.remote,
            r.message,
            r.reason.as_str()
        )
    } else if doc.finish.is_some() {
        "not published yet".to_owned()
    } else {
        "not finished".to_owned()
    };
    println!("publish:   {result}");
}

/// The CLI's way into a local-only source (F8 S3): its running daemon,
/// which holds its lock, through the control socket; else
/// [`UnderLock`] in this process.
struct ThroughService;

impl Receiver for ThroughService {
    fn receive(
        &self,
        source: &StateDir,
        fork: &ForkIdentity,
        from: &StateDir,
    ) -> std::result::Result<(), ReceiverError> {
        let unavailable = |m: String| ReceiverError::Unavailable(m);
        let path = std::path::absolute(from.root())
            .map_err(|e| unavailable(format!("resolving {}: {e}", from.root().display())))?;
        let request = control::Request::receive_fork(path.display().to_string());
        let reply = match control::call(&source.control_dir(), &request) {
            Ok(reply) => reply,
            Err(control::ClientError::NotRunning) => return UnderLock.receive(source, fork, from),
            Err(e) => return Err(unavailable(format!("the source's daemon: {e}"))),
        };
        match reply {
            control::Reply::Received { change } => full_commit(&change)
                .map(|_| ())
                .ok_or_else(|| unavailable(format!("the source's daemon answered {change:?}"))),
            control::Reply::Refused { kind, message, .. } => {
                let refused = receive::RefusalKind::parse(&kind)
                    .and_then(|kind| ReceiveError::from_refusal(kind, message.clone()));
                Err(match refused {
                    Some(refused) => refused.into(),
                    None => unavailable(message),
                })
            }
            control::Reply::Busy(message) | control::Reply::Error(message) => {
                Err(unavailable(format!("the source's daemon: {message}")))
            }
            control::Reply::Ok(_)
            | control::Reply::Pushed(_)
            | control::Reply::Attached { .. }
            | control::Reply::Judged { .. }
            | control::Reply::Checkpoint { .. } => Err(unavailable(
                "the source's daemon answered another request's reply".into(),
            )),
        }
    }
}

/// The daemon's side of `receive_fork`: receives the finished fork whose
/// state area the request names into `state`, the local-only source this
/// daemon holds the lock of.
pub fn serve_receive(state: &StateDir, request: &control::Request) -> control::Reply {
    let refused = |kind: &str, message: String| control::Reply::Refused {
        kind: kind.into(),
        message,
        hint: None,
    };
    let Some(path) = request.path.as_deref() else {
        return control::Reply::Error("receive_fork names no fork state area".into());
    };
    let from = StateDir::new(path);
    let identity = match finish::status(&from) {
        Ok(Some(status)) => status.identity,
        Ok(None) => return refused("not_a_fork", format!("{path} is not a fork")),
        Err(e) => return control::Reply::Error(format!("reading the fork at {path}: {e}")),
    };
    match receive::receive_fork_change(state, &identity, &from) {
        Ok(change) => control::Reply::Received {
            change: change.to_string(),
        },
        Err(e) => match e.refusal() {
            Some((kind, message)) => refused(kind.as_str(), message),
            None => control::Reply::Error(e.to_string()),
        },
    }
}

/// `fork create`: the line to print, or why it was refused.
fn create_cmd(args: &CreateArgs, state: &StateDir, root: &Path) -> Result<String> {
    let source = StateDir::new(&args.source_state);
    let includes = create::settings_files(&source)?;
    let push_target = create::push_target(&source)?;
    // A fork already in this state area keeps its name: create is
    // idempotent per name.
    let existing = Settings::load(&state.git())
        .ok()
        .filter(|s| s.is_fork())
        .map(|s| s.volume);
    let name = match existing {
        Some(name) => name,
        None => {
            let prefix = match &args.name {
                Some(name) => name.clone(),
                None => Settings::load(&source.git())?.volume,
            };
            create::unique_name(&prefix)?
        }
    };
    let remotes = args
        .remotes
        .as_deref()
        .map(std::path::absolute)
        .transpose()?;
    let identity = create::create(&CreateRequest {
        source: &source,
        fork: state,
        tree: root,
        name: &name,
        includes: &includes,
        push_target,
        link: args.link.into(),
        remotes: remotes.as_deref(),
    })?;
    let own = Settings::load(&state.git())?.fork_remotes;
    Ok(format!(
        "fork {} of {} at {}{}",
        identity.fork,
        identity.source,
        identity.base,
        match (identity.link, own) {
            (ForkLink::Linked, _) => String::new(),
            (ForkLink::Unlinked, Some(own)) => format!(
                " (unlinked: nothing is delivered to the source; it pushes only to the remotes in \
                 {})",
                own.display()
            ),
            (ForkLink::Unlinked, None) => " (unlinked and local-only: nothing is delivered to the \
                 source and nothing is pushed; removing it loses its history)"
                .to_owned(),
        }
    ))
}

/// One line for a finish record.
fn describe(record: &FinishRecord) -> String {
    format!(
        "finished fork {} of {} at {}: change {}, finish {}, fork remote {}, {} unrecorded",
        record.fork,
        record.source,
        record.base,
        record
            .change
            .map_or_else(|| "none".to_owned(), |c| c.to_string()),
        record.finish,
        record.remote,
        record.unrecorded
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D5: every reason's hint links its own section of the refused
    /// runbook, and that section exists.
    #[test]
    fn each_refused_reason_links_its_runbook_section() {
        let runbook = include_str!("../../../docs/runbooks/fork-refused.md");
        for reason in RefusedReason::ALL {
            let hint = refused_hint(reason);
            let link = format!("{REFUSED_RUNBOOK}#{}", reason.as_str());
            assert!(hint.contains(&link), "{reason:?}: {hint}");
            let heading = format!("\n## `{}`\n", reason.as_str());
            assert!(
                runbook.contains(&heading),
                "{REFUSED_RUNBOOK} has no section {heading:?}"
            );
        }
    }

    /// A fork of a fork published because its parent had finished says
    /// so, from its published mark, and links a runbook section that
    /// exists; any other publish omits it (bead agentfs-rrm).
    #[test]
    fn status_shows_a_parent_that_finished_first() {
        use agentfs_core::fork::finish::{ForkMark, ParentFinishedMark, PublishedMark};
        use agentfs_core::fork::identity::PushTarget;
        let finish = git2::Oid::from_bytes(&[2; 20]).unwrap();
        let record = FinishRecord {
            fork: "src-1-1".into(),
            source: "src-1".into(),
            base: git2::Oid::from_bytes(&[1; 20]).unwrap(),
            finish,
            change: None,
            remote: "origin".into(),
            unrecorded: 0,
        };
        let status = |parent_finished: Option<ParentFinishedMark>| ForkStatus {
            identity: ForkIdentity {
                fork: record.fork.clone(),
                source: record.source.clone(),
                base: record.base,
                push_target: PushTarget::Branch,
                link: ForkLink::Linked,
            },
            state: ForkState::Published,
            record: Some(record.clone()),
            mark: Some(ForkMark {
                published: Some(PublishedMark {
                    finish: finish.to_string(),
                    change: None,
                    remote: "origin".into(),
                    inbox: None,
                    parent_finished,
                    at_ms: 5,
                }),
                ..ForkMark::empty()
            }),
        };
        let doc = ForkStatusDoc::new(&status(Some(ParentFinishedMark {
            parent: "src-1".into(),
            raised: false,
        })));
        assert_eq!(doc.parent_finished.as_deref(), Some("src-1"));
        let json = serde_json::to_value(&doc).unwrap();
        assert_eq!(json["parent_finished"], "src-1", "{json}");
        let plain = ForkStatusDoc::new(&status(None));
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("parent_finished").is_none(), "{json}");

        let runbook = include_str!("../../../docs/runbooks/patch-queue.md");
        let (_, anchor) = PARENT_FINISHED_RUNBOOK.split_once('#').unwrap();
        assert_eq!(anchor, "9-forks-of-forks");
        assert!(
            runbook.contains("\n## 9. Forks of forks\n"),
            "{PARENT_FINISHED_RUNBOOK} has no section"
        );
    }
}
