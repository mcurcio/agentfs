//! `agentfs fork` (design §11.5, §17.1): create a fork of a volume, and
//! finish and publish it.
//!
//! - `fork create` makes the fork's state area and backing tree from the
//!   source's state area, read without the source's lock; the fork reads
//!   its settings from the source's included files
//!   ([`agentfs_core::fork::create::settings_files`]).
//! - `fork finish` takes the fork's state-area lock, finishes the fork
//!   once and publishes it to its fork remote, retrying with backoff.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::{Args, Subcommand};

use agentfs_core::config::Settings;
use agentfs_core::fork::PushTarget;
use agentfs_core::fork::create::{self, CreateRequest};
use agentfs_core::fork::finish::{FinishRecord, Finisher, PublishOutcome};
use agentfs_core::lock::StateLock;
use agentfs_core::volume::StateDir;

use crate::{Pod, Session};

#[derive(Subcommand)]
pub enum ForkCommand {
    /// Create a fork of the volume at --source-state in --state, its
    /// backing tree at --root (created if missing): base's tree written,
    /// nothing mounted, no lock on the source. Prints `fork <name> of
    /// <source> at <base>`.
    Create(CreateArgs),
    /// Finish the fork (once) and publish it to its fork remote, retrying
    /// with backoff: exit 0 published, 1 not yet at --timeout, 75 when the
    /// state area is busy.
    Finish(FinishArgs),
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
    #[command(flatten)]
    pub pod: Pod,
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
                    eprintln!("publishing is refused ({reason:?}): {message}");
                    ExitCode::FAILURE
                }
            };
            drop(finisher);
            session.finish();
            Ok(code)
        }
    }
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
