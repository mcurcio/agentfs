//! `agentfs patch` and the mount's patch phase (design §12, §17.1).
//!
//! - `patch submit` and `patch approve` are the producer side: they run in
//!   the producer's git repository and drive its `git`
//!   ([`agentfs_core::patch::submit`]).
//! - `patch apply` asks a running daemon first (the control socket's
//!   `apply`, bead agentfs-mia.5): a daemon always counts as attached, so
//!   it only judges the queue ([`Queue::judge`]), notifies, and answers
//!   "workload attached; lands at next mount" (exit 75). With no daemon it
//!   fetches and applies the queue with the volume stopped (it takes the
//!   state-area lock), as a mount does before it serves.
//! - Every queue run and every judging tells the integrator what changed
//!   ([`notify`]: the `patch-notify` hook, the notify webhook).
//! - `patch status` reads `meta/patch.json` and the status ref without
//!   the lock.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};
use serde::Serialize;

use agentfs_core::config::Settings;
use agentfs_core::confined::Dir;
use agentfs_core::events::{Event, Events};
use agentfs_core::history::History;
use agentfs_core::identity::Identity;
use agentfs_core::lfs::fetch::Fetcher;
use agentfs_core::lock::StateLock;
use agentfs_core::patch::accept;
use agentfs_core::patch::fetch::{FetchOutcome, fetch_inbox};
use agentfs_core::patch::integrate;
use agentfs_core::patch::notify::{self, ApplyAnswer};
use agentfs_core::patch::queue::{JudgeContext, Queue, QueueContext, QueueReport, record_failure};
use agentfs_core::patch::status::{PatchState, StatusDoc, read_status};
use agentfs_core::patch::status_ref;
use agentfs_core::patch::submit::{self, ApproveRequest, GitCli, Source, SubmitRequest};
use agentfs_core::patch::{BlockReason, PatchError, Verdict};
use agentfs_core::record::Caller;
use agentfs_core::telemetry::Telemetry;
use agentfs_core::volume::{Committer, Recorder, StateDir};

use crate::control::{self, ClientError, Reply, Request};
use crate::{EXIT_LOCKED, Pod, Session, current_gid, current_uid};

/// What `patch apply` is answered while a daemon holds the volume (design
/// §19.1 "No live landing yet").
pub const ATTACHED: &str = "workload attached; lands at next mount";

#[derive(Subcommand)]
pub enum PatchCommand {
    /// Queue a patch for a volume (run in the producer's git repository):
    /// a branch, given by its tip COMMIT, whose commits since the volume's
    /// branch are the patch, or one commit built on the remote branch head
    /// from --put/--delete. Pushed to the next free inbox number; a race
    /// with another submitter takes the next.
    Submit(SubmitArgs),
    /// Approve queued patch N for a volume whose agentfs.patchAccept is
    /// `approval` (run in the producer's git repository): its tip as the
    /// remote lists it goes to refs/agentfs/<volume>/approved/<N>. A
    /// replaced patch needs approving again.
    Approve(ApproveArgs),
    /// Fetch and apply the volume's patch queue now, with the volume
    /// stopped (takes the state-area lock; exit 75 when it is busy). Exit
    /// 1 when the queue is blocked or a patch was set aside. While a daemon
    /// holds the volume, it judges the queue instead and nothing lands:
    /// "workload attached; lands at next mount", exit 75.
    Apply {
        /// Apply what is already fetched; do not contact the patch remote.
        #[arg(long)]
        no_fetch: bool,
        #[command(flatten)]
        pod: Pod,
    },
    /// Show the patch queue as the last run left it (no lock, no network).
    Status {
        /// The status document (`status.json`) and the local cache, as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args)]
pub struct SubmitArgs {
    /// The tip of the branch to queue (any revision); its commits since the
    /// volume's branch are the patch. Omit to build one commit from
    /// --put/--delete on the remote branch head.
    commit: Option<String>,
    /// The volume.
    #[arg(long, env = "AGENTFS_VOLUME")]
    volume: String,
    /// The remote (name or URL) the volume pushes to.
    #[arg(long, default_value = "origin")]
    remote: String,
    /// The volume's branch (default: agentfs/<volume>).
    #[arg(long)]
    branch: Option<String>,
    /// The producer's git repository.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// Write PATH in the tree with the content of FILE; repeatable.
    #[arg(long = "put", value_name = "PATH=FILE")]
    puts: Vec<String>,
    /// Remove PATH from the tree; repeatable.
    #[arg(long = "delete", value_name = "PATH")]
    deletes: Vec<String>,
    /// The commit message of a built patch.
    #[arg(short = 'm', long = "message", default_value = "")]
    message: String,
    /// `Name <email>`, the author of a built patch (default: git's user).
    #[arg(long)]
    author: Option<String>,
    /// Replace queued patch N (it must still be what was listed).
    #[arg(long, value_name = "N")]
    replace: Option<u64>,
    /// Replace queued patch N with an empty patch, so the queue moves on.
    #[arg(long, value_name = "N", conflicts_with_all = ["commit", "puts", "deletes", "replace"])]
    skip: Option<u64>,
    /// Fetch only this many commits of history (shallow producers).
    #[arg(long)]
    depth: Option<u32>,
    /// Inbox numbers to try when other submitters race.
    #[arg(long, default_value_t = submit::DEFAULT_ATTEMPTS)]
    attempts: u32,
    /// Make the patch a barrier: while it is set aside, every later patch
    /// waits. A built or skip patch gets the `Agentfs-Barrier: yes`
    /// trailer; a given COMMIT must already carry it.
    #[arg(long)]
    barrier: bool,
}

#[derive(Args)]
pub struct ApproveArgs {
    /// The queued patch's inbox number.
    inbox: u64,
    /// The volume.
    #[arg(long, env = "AGENTFS_VOLUME")]
    volume: String,
    /// The remote (name or URL) the volume pushes to.
    #[arg(long, default_value = "origin")]
    remote: String,
    /// The producer's git repository.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// The tip you mean (a full commit id): refused if the queue holds
    /// another.
    #[arg(long = "patch", value_name = "OID")]
    patch: Option<String>,
}

pub fn run(
    command: PatchCommand,
    state: &StateDir,
    root: Option<&Path>,
    wait_lock: std::time::Duration,
) -> Result<ExitCode> {
    match command {
        PatchCommand::Submit(args) => submit_cmd(args),
        PatchCommand::Approve(args) => approve_cmd(args),
        PatchCommand::Apply { no_fetch, pod } => {
            crate::refuse_finished(state)?;
            if let Some(code) = apply_client(state, !no_fetch) {
                return Ok(code);
            }
            let Some(root) = root else {
                bail!("patch apply needs --root (or AGENTFS_ROOT): the backing tree");
            };
            let _lock = StateLock::acquire(state, "patch apply", wait_lock)?;
            let session = Session::start(state, &pod)?;
            let (recorder, committer, _) = session.open(state, root, &pod)?;
            let settings = committer.history().settings().clone();
            let report = run_queue(
                state,
                &settings,
                &recorder,
                &committer,
                root,
                session.telemetry(),
                &session.events,
                !no_fetch,
                true,
            );
            // Telemetry is flushed on failure too: the alarm and gauges.
            session.finish();
            let report = report?;
            Ok(
                if report.blocked.is_some() || !report.set_aside.is_empty() {
                    ExitCode::FAILURE
                } else {
                    ExitCode::SUCCESS
                },
            )
        }
        PatchCommand::Status { json } => status_cmd(state, json),
    }
}

fn submit_cmd(args: SubmitArgs) -> Result<ExitCode> {
    let source = if let Some(n) = args.skip {
        Source::Skip(n)
    } else if let Some(commit) = args.commit {
        if !args.puts.is_empty() || !args.deletes.is_empty() {
            bail!("give either COMMIT or --put/--delete, not both");
        }
        Source::Commit(commit)
    } else {
        if args.puts.is_empty() && args.deletes.is_empty() {
            bail!("nothing to submit: give COMMIT, --put/--delete, or --skip N");
        }
        let mut puts = Vec::new();
        for put in &args.puts {
            let Some((path, file)) = put.split_once('=') else {
                bail!("--put {put:?} is not PATH=FILE");
            };
            puts.push((path.to_owned(), PathBuf::from(file)));
        }
        Source::Build {
            puts,
            deletes: args.deletes,
            message: args.message,
            author: args.author,
        }
    };
    let request = SubmitRequest {
        branch: args
            .branch
            .unwrap_or_else(|| format!("agentfs/{}", args.volume)),
        remote: args.remote,
        volume: args.volume,
        source,
        replace: args.replace,
        attempts: args.attempts,
        depth: args.depth,
        barrier: args.barrier,
    };
    let done = submit::submit(&GitCli::new(&args.repo), &request)?;
    println!(
        "queued patch {} as {} ({}, {} commit(s) on {})",
        done.inbox, done.ref_name, done.oid, done.commits, done.base
    );
    if done.raced > 0 {
        println!("{} number(s) taken by other submitters first", done.raced);
    }
    Ok(ExitCode::SUCCESS)
}

fn approve_cmd(args: ApproveArgs) -> Result<ExitCode> {
    let request = ApproveRequest {
        remote: args.remote,
        volume: args.volume,
        inbox: args.inbox,
        expect: args.patch,
    };
    let done = submit::approve(&GitCli::new(&args.repo), &request)?;
    println!(
        "approved patch {} ({}) as {}",
        done.inbox, done.tip, done.ref_name
    );
    Ok(ExitCode::SUCCESS)
}

/// A verdict as `patch status` and `patch apply` print it: its label
/// (`held`, `waiting`, `awaiting`, `not judged`) and what follows the
/// patch's number.
fn verdict_parts(verdict: &Verdict) -> (&'static str, String) {
    match verdict {
        Verdict::Held {
            reason, message, ..
        } => ("held", format!(" ({reason}): {message}")),
        Verdict::Waiting { why, behind } => (
            "waiting",
            format!(
                " behind {} ({})",
                behind
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                why.as_str()
            ),
        ),
        Verdict::Awaiting { what } => ("awaiting", format!(" ({})", what.as_str())),
        Verdict::NotJudged { rule, message } => ("not judged", format!(" ({rule}): {message}")),
        Verdict::Ready => ("ready", String::new()),
    }
}

/// `patch apply` through a running daemon: its exit status, or `None`
/// when no daemon answers (no socket, a reply this build cannot read, or
/// a daemon from before `apply`), and the command takes the lock path.
fn apply_client(state: &StateDir, fetch: bool) -> Option<ExitCode> {
    let reply = match control::call(&state.control_dir(), &Request::apply(fetch)) {
        Ok(reply) => reply,
        Err(ClientError::NotRunning | ClientError::Protocol(_)) => return None,
        Err(ClientError::Io(e)) => {
            eprintln!(
                "error: a daemon seems to be running, but its control socket in {} cannot be \
                 used: {e}",
                state.control_dir().display()
            );
            eprintln!("hint: run as root or as the user the daemon runs as");
            return Some(ExitCode::FAILURE);
        }
    };
    Some(match reply {
        Reply::Attached { message, verdicts } => {
            for (n, verdict) in &verdicts {
                let (label, detail) = verdict_parts(verdict);
                eprintln!("patch {n} {label}{detail}");
            }
            eprintln!("patches: {message}");
            ExitCode::from(EXIT_LOCKED)
        }
        // A daemon from before `apply`: the lock path, which refuses.
        Reply::Error(message) if message.contains("unknown variant") => return None,
        Reply::Error(message) => {
            eprintln!("patch apply failed: {message}");
            ExitCode::FAILURE
        }
        Reply::Refused { message, hint, .. } => {
            eprintln!("patch apply refused: {message}");
            if let Some(hint) = hint {
                eprintln!("hint: {hint}");
            }
            ExitCode::FAILURE
        }
        Reply::Busy(message) => {
            eprintln!("error: {message}");
            ExitCode::from(EXIT_LOCKED)
        }
        Reply::Ok(_) | Reply::Pushed(_) | Reply::Received { .. } | Reply::Checkpoint { .. } => {
            eprintln!("patch apply failed: the daemon answered another request's reply");
            ExitCode::FAILURE
        }
    })
}

/// What a daemon judges an apply with (bead agentfs-mia.5): its alarms,
/// its telemetry and its committer identity, which signs the status.
#[derive(Clone)]
pub struct Applier {
    events: Events,
    telemetry: Telemetry,
    committer: Identity,
}

impl Applier {
    pub fn new(committer: &Committer, events: Events, telemetry: &Telemetry) -> Self {
        Self {
            events,
            telemetry: telemetry.clone(),
            committer: committer.committer_identity(),
        }
    }
}

/// One apply judged at a time: they write one status ref and one cursor.
static APPLYING: Mutex<()> = Mutex::new(());

/// The control socket's `apply` (bead agentfs-mia.5): a running daemon
/// always counts as attached (a front may attach mid-landing), so the
/// queue is fetched (unless the request says not to) and judged against
/// the head as it is, read only, with its own history handle
/// ([`Queue::judge`]); the status is written and the integrator notified
/// with `apply.kind = attached`. Nothing lands.
pub fn serve_apply(state: &StateDir, applier: &Applier, request: &Request) -> Reply {
    let _one = APPLYING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match judge_attached(state, applier, !request.no_fetch) {
        Ok(verdicts) => Reply::Attached {
            message: ATTACHED.to_owned(),
            verdicts,
        },
        Err(e) => Reply::Error(format!("judging the patch queue: {e:#}")),
    }
}

fn judge_attached(state: &StateDir, applier: &Applier, fetch: bool) -> Result<Vec<(u64, Verdict)>> {
    let settings = Settings::load(&state.git())
        .with_context(|| format!("reading {}", state.git().display()))?;
    let fetched = fetch_queue(state, &settings, &applier.events, fetch);
    // Opened after the fetch, so it sees the objects it brought.
    let history = History::open(&state.git())
        .with_context(|| format!("opening {}", state.root().display()))?;
    let settings = history.settings();
    let integrator = integrate::integrator(settings.patch_integrate, &state.git());
    let rule = accept::rule(
        settings.patch_accept,
        &state.git(),
        settings.patch_accept_timeout,
    );
    let judgement = Queue::judge(&JudgeContext {
        state,
        history: &history,
        integrator: integrator.as_ref(),
        rule: rule.as_ref(),
        events: &applier.events,
        telemetry: &applier.telemetry,
        committer: &applier.committer,
        fetch: fetched.as_ref(),
    })
    .context("judging patches")?;
    warn_unnotified(notify::tell(
        state,
        &history,
        judgement.status,
        &[],
        Some(ApplyAnswer::Attached {
            message: ATTACHED.to_owned(),
        }),
        &applier.events,
    ));
    Ok(judgement.verdicts)
}

/// A failed notify method is alarmed by [`notify::deliver`]; failing to
/// read or write the state is a warning: the queue's work stands.
fn warn_unnotified(result: std::result::Result<notify::Delivery, PatchError>) {
    if let Err(e) = result {
        eprintln!("warning: telling the integrator of the patch queue's changes: {e}");
    }
}

#[derive(Serialize)]
struct StatusJson {
    /// `meta/patch.json`.
    cache: PatchState,
    /// The local status ref's `status.json`, if there is one.
    status: Option<StatusDoc>,
}

fn status_cmd(state: &StateDir, json: bool) -> Result<ExitCode> {
    let history = History::open(&state.git())
        .with_context(|| format!("opening {}", state.root().display()))?;
    let cache = PatchState::load(&state.patch_state());
    let repo = history.repo();
    let status = repo
        .refname_to_id(&status_ref(&history.settings().volume))
        .ok()
        .and_then(|oid| read_status(repo, oid).ok());
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&StatusJson { cache, status })?
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!("patches:   {}", cache.summary());
    match history.settings().patch_remote() {
        Some(remote) => println!("remote:    {}", remote.name),
        None => println!("remote:    none (agentfs.patchRemote)"),
    }
    if let Some(status) = status {
        if let Some(fetch) = &status.fetch
            && !fetch.ok
        {
            println!(
                "fetch:     failed from {}: {}",
                fetch.remote,
                fetch.error.as_deref().unwrap_or("unknown")
            );
        }
        for q in &status.queue {
            println!("queued:    {} {}", q.inbox, q.patch);
        }
        for q in &status.queue {
            if let Some(verdict) = &q.verdict {
                let (label, detail) = verdict_parts(verdict);
                println!("{:<10} {}{detail}", format!("{label}:"), q.inbox);
            }
        }
        if let Some(b) = &status.blocked {
            println!("blocked:   {} ({}): {}", b.inbox, b.reason, b.message);
            println!("           {}", b.hint());
        }
        for i in &status.ignored {
            println!("ignored:   {} ({})", i.ref_name, i.why);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The patch phase (design §12, §17.1): with the volume open and nothing
/// serving the tree, fetch the inbox from the patch remote (best effort,
/// unless `fetch` is false or `agentfs.patchFetchTimeout` is 0) and apply
/// the queue. A blocked queue is an alarm, not an error: the volume
/// serves anyway. An `Err` is a failure to run the queue at all (reading
/// the history or the state area, opening the tree or the LFS cache): it
/// raises a `PatchBlocked` alarm (reason `failed`, inbox 0) and is recorded
/// as blocked in `meta/patch.json` and the status ref, so the gauges and
/// `agentfs status` show it.
#[allow(clippy::too_many_arguments)]
pub fn apply_queue(
    state: &StateDir,
    settings: &Settings,
    recorder: &Recorder,
    committer: &Committer,
    root: &Path,
    telemetry: &Telemetry,
    events: &Events,
    fetch: bool,
) -> Result<QueueReport> {
    run_queue(
        state, settings, recorder, committer, root, telemetry, events, fetch, false,
    )
}

/// [`apply_queue`], then the notice of what changed; `answering` when an
/// apply call ran it (the notice carries `apply.kind = landed`), not a
/// mount.
#[allow(clippy::too_many_arguments)]
fn run_queue(
    state: &StateDir,
    settings: &Settings,
    recorder: &Recorder,
    committer: &Committer,
    root: &Path,
    telemetry: &Telemetry,
    events: &Events,
    fetch: bool,
    answering: bool,
) -> Result<QueueReport> {
    let report = apply_queue_inner(
        state, settings, recorder, committer, root, telemetry, events, fetch,
    )
    .inspect_err(|e| {
        let message = format!("the patch queue could not run: {e:#}");
        record_failure(state, Some(committer), &message);
        events.emit(Event::PatchBlocked {
            inbox: 0,
            patch: None,
            reason: BlockReason::Failed.as_str(),
            message,
            queued: 0,
        });
    })?;
    warn_unnotified(notify::tell(
        state,
        committer.history(),
        report.status,
        &report.applied,
        answering.then(|| ApplyAnswer::Landed {
            landed: report.applied.iter().map(|(n, _)| *n).collect(),
        }),
        events,
    ));
    Ok(report)
}

/// Fetches the inbox from the patch remote, best effort, unless `fetch`
/// is false, there is no patch remote, or `agentfs.patchFetchTimeout` is
/// 0; a failure is alarmed (`PatchFetchFailed`) and the queue runs with
/// what it has.
fn fetch_queue(
    state: &StateDir,
    settings: &Settings,
    events: &Events,
    fetch: bool,
) -> Option<FetchOutcome> {
    let remote = settings.patch_remote()?;
    if !fetch || settings.patch_fetch_timeout.is_zero() {
        return None;
    }
    let outcome = fetch_inbox(
        &state.git(),
        remote,
        &settings.volume,
        settings.patch_fetch_timeout,
    );
    if !outcome.ok {
        events.emit(Event::PatchFetchFailed {
            remote: outcome.remote.clone(),
            kind: outcome
                .kind
                .unwrap_or(agentfs_core::events::PushFailureKind::Unreachable),
            message: outcome.message.clone().unwrap_or_default(),
        });
    }
    Some(outcome)
}

#[allow(clippy::too_many_arguments)]
fn apply_queue_inner(
    state: &StateDir,
    settings: &Settings,
    recorder: &Recorder,
    committer: &Committer,
    root: &Path,
    telemetry: &Telemetry,
    events: &Events,
    fetch: bool,
) -> Result<QueueReport> {
    let fetched = fetch_queue(state, settings, events, fetch);
    let tree = Dir::open(root).with_context(|| format!("opening {}", root.display()))?;
    let objects = Fetcher::new(state, settings).context("opening the LFS cache")?;
    let integrator = integrate::integrator(settings.patch_integrate, &state.git());
    let rule = accept::rule(
        settings.patch_accept,
        &state.git(),
        settings.patch_accept_timeout,
    );
    let mut queue = Queue::new(QueueContext {
        state,
        recorder,
        committer,
        tree: &tree,
        objects: &objects,
        telemetry,
        caller: Caller {
            uid: current_uid(),
            gid: current_gid(),
            pid: std::process::id(),
        },
        integrator: integrator.as_ref(),
        rule: rule.as_ref(),
    });
    let report = queue
        .process(fetched.as_ref())
        .context("applying patches")?;
    for (n, commit) in &report.applied {
        eprintln!("patch {n} applied as {commit}");
    }
    if !report.already.is_empty() {
        eprintln!(
            "patch(es) {} were already applied; their refs are moved",
            report
                .already
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    for ignored in &report.ignored {
        eprintln!(
            "patch queue: ignoring {} ({})",
            ignored.ref_name, ignored.why
        );
    }
    for (n, verdict) in &report.set_aside {
        let (label, detail) = verdict_parts(verdict);
        eprintln!("patch {n} {label}{detail}");
    }
    match &report.blocked {
        Some(b) => eprintln!(
            "patches: blocked at {} ({}): {}; {} queued. {}",
            b.inbox,
            b.reason,
            b.message,
            report.queue_depth,
            b.hint()
        ),
        None if !report.set_aside.is_empty() => eprintln!(
            "patches: through {} applied, {} queued ({} set aside)",
            report.last_applied,
            report.queue_depth,
            report.set_aside.len()
        ),
        None if !report.applied.is_empty() || fetched.is_some() => eprintln!(
            "patches: through {} applied, none queued",
            report.last_applied
        ),
        None => {}
    }
    Ok(report)
}
