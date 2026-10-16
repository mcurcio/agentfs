//! `agentfs patch` and the mount's patch phase (design §12, §17.1).
//!
//! - `patch submit` is the producer side: it runs in the producer's git
//!   repository and drives its `git` ([`agentfs_core::patch::submit`]).
//! - `patch apply` fetches and applies the queue with the volume stopped
//!   (it takes the state-area lock), as a mount does before it serves.
//! - `patch status` reads `meta/patch.json` and the status ref without
//!   the lock.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};
use serde::Serialize;

use agentfs_core::config::Settings;
use agentfs_core::confined::Dir;
use agentfs_core::events::{Event, Events};
use agentfs_core::history::History;
use agentfs_core::lfs::fetch::Fetcher;
use agentfs_core::lock::StateLock;
use agentfs_core::patch::BlockReason;
use agentfs_core::patch::fetch::{FetchOutcome, fetch_inbox};
use agentfs_core::patch::queue::{Queue, QueueContext, QueueReport, record_failure};
use agentfs_core::patch::status::{PatchState, StatusDoc, read_status};
use agentfs_core::patch::status_ref;
use agentfs_core::patch::submit::{self, GitCli, Source, SubmitRequest};
use agentfs_core::record::Caller;
use agentfs_core::telemetry::Telemetry;
use agentfs_core::volume::{Committer, Recorder, StateDir};

use crate::{Pod, Session, current_gid, current_uid};

#[derive(Subcommand)]
pub enum PatchCommand {
    /// Queue a patch for a volume (run in the producer's git repository):
    /// a given COMMIT whose parent is on the volume's branch, or one built
    /// on the remote branch head from --put/--delete. Pushed to the next
    /// free inbox number; a race with another submitter takes the next.
    Submit(SubmitArgs),
    /// Fetch and apply the volume's patch queue now, with the volume
    /// stopped (takes the state-area lock; exit 75 when it is busy). Exit
    /// 1 when the queue is blocked.
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
    /// The commit to queue (any revision); omit to build one from
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
}

pub fn run(
    command: PatchCommand,
    state: &StateDir,
    root: Option<&Path>,
    wait_lock: std::time::Duration,
) -> Result<ExitCode> {
    match command {
        PatchCommand::Submit(args) => submit_cmd(args),
        PatchCommand::Apply { no_fetch, pod } => {
            let Some(root) = root else {
                bail!("patch apply needs --root (or AGENTFS_ROOT): the backing tree");
            };
            crate::refuse_finished(state)?;
            let _lock = StateLock::acquire(state, "patch apply", wait_lock)?;
            let session = Session::start(state, &pod)?;
            let (recorder, committer, _) = session.open(state, root, &pod)?;
            let settings = committer.history().settings().clone();
            let report = apply_queue(
                state,
                &settings,
                &recorder,
                &committer,
                root,
                session.telemetry(),
                &session.events,
                !no_fetch,
            );
            // Telemetry is flushed on failure too: the alarm and gauges.
            session.finish();
            let report = report?;
            Ok(if report.blocked.is_some() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            })
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
    };
    let done = submit::submit(&GitCli::new(&args.repo), &request)?;
    println!(
        "queued patch {} as {} ({}, parent {})",
        done.inbox, done.ref_name, done.oid, done.parent
    );
    if done.raced > 0 {
        println!("{} number(s) taken by other submitters first", done.raced);
    }
    Ok(ExitCode::SUCCESS)
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
    apply_queue_inner(
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
    })
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
    let fetched: Option<FetchOutcome> = match settings.patch_remote() {
        Some(remote) if fetch && !settings.patch_fetch_timeout.is_zero() => {
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
        _ => None,
    };
    let tree = Dir::open(root).with_context(|| format!("opening {}", root.display()))?;
    let objects = Fetcher::new(state, settings).context("opening the LFS cache")?;
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
    match &report.blocked {
        Some(b) => eprintln!(
            "patches: blocked at {} ({}): {}; {} queued. {}",
            b.inbox,
            b.reason,
            b.message,
            report.queue_depth,
            b.hint()
        ),
        None if !report.applied.is_empty() || fetched.is_some() => eprintln!(
            "patches: through {} applied, none queued",
            report.last_applied
        ),
        None => {}
    }
    Ok(report)
}
