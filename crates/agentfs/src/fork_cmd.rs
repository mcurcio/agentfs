//! `agentfs fork` (design §11.5, §17.1): create a fork of a volume,
//! finish and publish it, read its status, abandon it, and list the
//! published changes on a fork remote.
//!
//! - `fork create` makes the fork's state area and backing tree from the
//!   source's state area, read without the source's lock; the fork reads
//!   its settings from the source's included files
//!   ([`agentfs_core::fork::create::settings_files`]).
//! - `fork finish` takes the fork's state-area lock, finishes the fork
//!   once and publishes it to its fork remote, retrying with backoff;
//!   `--retry` clears a refused publish first.
//! - `fork status` reads the fork's state, record and publish result,
//!   with no lock and no network.
//! - `fork abandon` records the operator's discard, with the state lock.
//! - `fork list` runs in a receiver's git repository, with its own git and
//!   credentials, never a state area ([`agentfs_core::fork::listing`]).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use serde::Serialize;

use agentfs_core::config::Settings;
use agentfs_core::events::Event;
use agentfs_core::fork::PushTarget;
use agentfs_core::fork::create::{self, CreateRequest};
use agentfs_core::fork::finish::{
    self, AbandonedMark, FinishRecord, Finisher, ForkState, ForkStatus, PublishOutcome, RefusedMark,
};
use agentfs_core::fork::listing;
use agentfs_core::lock::StateLock;
use agentfs_core::patch::submit::GitCli;
use agentfs_core::record::Caller;
use agentfs_core::volume::StateDir;

use crate::{Pod, Session};

/// Where an operator reads what to do about a refused publish.
const REFUSED_RUNBOOK: &str = "docs/runbooks/csi-recovery.md";

#[derive(Subcommand)]
pub enum ForkCommand {
    /// Create a fork of the volume at --source-state in --state, its
    /// backing tree at --root (created if missing): base's tree written,
    /// nothing mounted, no lock on the source. Prints `fork <name> of
    /// <source> at <base>`.
    Create(CreateArgs),
    /// Finish the fork (once) and publish it to its fork remote, retrying
    /// with backoff: exit 0 published, 1 not yet at --timeout or refused,
    /// 75 when the state area is busy.
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
    pub refused: Option<RefusedMark>,
    pub abandoned: Option<AbandonedMark>,
}

impl ForkStatusDoc {
    fn new(status: &ForkStatus) -> Self {
        let record = status.record.as_ref();
        let finish = record.map(|r| r.finish.to_string());
        let mark = status.mark.as_ref();
        let names_finish = |f: &str| finish.as_deref() == Some(f);
        Self {
            version: 1,
            fork: status.identity.fork.clone(),
            source: status.identity.source.clone(),
            base: status.identity.base.to_string(),
            state: status.state.as_str(),
            change: record.and_then(|r| r.change).map(|c| c.to_string()),
            finish: finish.clone(),
            remote: record.map(|r| r.remote.clone()),
            published_at_ms: mark
                .and_then(|m| m.published.as_ref())
                .filter(|p| names_finish(&p.finish))
                .map(|p| p.at_ms),
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
            if args.retry {
                finisher.retry_refused()?;
            }
            let record = finisher.finish()?;
            println!("{}", describe(&record));
            let deadline = args.timeout.map(|t| Instant::now() + t);
            let code = match finisher.publish(deadline)? {
                PublishOutcome::Published => {
                    println!("published to {}", record.remote);
                    ExitCode::SUCCESS
                }
                PublishOutcome::Pending { kind, message } => {
                    eprintln!("not published yet ({kind}): {message}");
                    ExitCode::FAILURE
                }
                PublishOutcome::Refused { reason, message } => {
                    eprintln!(
                        "publishing is refused ({}): {message}; nothing retries it: fix the \
                         cause and run `agentfs fork finish --retry`, or `agentfs fork \
                         abandon` (see {REFUSED_RUNBOOK})",
                        reason.as_str()
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
        format!("published at {at} ms")
    } else if let Some(r) = &doc.refused {
        format!(
            "refused ({}) at {} ms by {}: {} (see {REFUSED_RUNBOOK})",
            r.reason.as_str(),
            r.at_ms,
            r.remote,
            r.message
        )
    } else if doc.finish.is_some() {
        "not published yet".to_owned()
    } else {
        "not finished".to_owned()
    };
    println!("publish:   {result}");
}

/// `fork create`: the line to print, or why it was refused.
fn create_cmd(args: &CreateArgs, state: &StateDir, root: &Path) -> Result<String> {
    let source = StateDir::new(&args.source_state);
    let includes = create::settings_files(&source)?;
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
    let identity = create::create(&CreateRequest {
        source: &source,
        fork: state,
        tree: root,
        name: &name,
        includes: &includes,
        push_target: PushTarget::Branch,
    })?;
    Ok(format!(
        "fork {} of {} at {}",
        identity.fork, identity.source, identity.base
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
