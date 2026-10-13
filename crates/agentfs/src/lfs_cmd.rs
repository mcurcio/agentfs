//! `agentfs lfs` (design §7, §17.1): the local LFS object cache.
//!
//! - `fetch` downloads into `<state>/lfs/objects` the objects that
//!   commits reference: the branch head's tree by default, the trees of
//!   the revisions given, or every commit on the branch (`--all`), from
//!   the remotes in name order (or only those named). It takes no
//!   state-area lock, so it runs beside a daemon, and holds the cache lock
//!   shared. Exit 0 when every object is in the cache afterwards, 1
//!   otherwise.

use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use clap::Subcommand;

use agentfs_core::history::History;
use agentfs_core::lfs::fetch::{self, FetchReport};
use agentfs_core::lfs::{ObjectCache, Pointer, referenced};
use agentfs_core::paths::RepoPath;
use agentfs_core::volume::{self, StateDir};

#[derive(Subcommand)]
pub enum LfsCommand {
    /// Download into the local cache the LFS objects the branch head
    /// references (or those of REVISION..., or of every commit with
    /// --all), from the remotes in name order. Takes no state-area lock:
    /// safe beside a running daemon. Exit 1 unless every object is cached
    /// afterwards.
    Fetch {
        /// Commits whose trees' objects to fetch (any git revision syntax);
        /// default: the branch head.
        revisions: Vec<String>,
        /// Every object any commit on the branch references.
        #[arg(long, conflicts_with = "revisions")]
        all: bool,
        /// Fetch only from this remote; repeatable (default: every remote,
        /// in name order).
        #[arg(long = "remote", value_name = "NAME")]
        remotes: Vec<String>,
        /// Report what is missing from the cache; download nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

pub fn run(command: LfsCommand, state: &StateDir) -> Result<ExitCode> {
    match command {
        LfsCommand::Fetch {
            revisions,
            all,
            remotes,
            dry_run,
        } => fetch_cmd(state, &revisions, all, &remotes, dry_run),
    }
}

/// `n object(s), b byte(s)`.
fn amount(n: usize, bytes: u64) -> String {
    format!("{n} object(s), {bytes} byte(s)")
}

/// The objects to fetch, each with one path that references it.
fn wanted(history: &History, revisions: &[String], all: bool) -> Result<Vec<(Pointer, RepoPath)>> {
    let repo = history.repo();
    let head = history
        .head()
        .context("reading the branch")?
        .map(|c| c.id());
    let mut found = Vec::new();
    if all {
        if let Some(head) = head {
            found = referenced::in_history(repo, head, None)?;
        }
    } else if revisions.is_empty() {
        if let Some(head) = head {
            found = referenced::in_tree(repo, head)?;
        }
    } else {
        for revision in revisions {
            let commit = repo
                .revparse_single(revision)
                .and_then(|o| o.peel_to_commit())
                .with_context(|| format!("{revision} is not a commit in the history"))?;
            found.extend(referenced::in_tree(repo, commit.id())?);
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    found.retain(|(p, _)| seen.insert(p.oid));
    Ok(found)
}

fn fetch_cmd(
    state: &StateDir,
    revisions: &[String],
    all: bool,
    remotes: &[String],
    dry_run: bool,
) -> Result<ExitCode> {
    let (history, _) =
        volume::inspect(state).with_context(|| format!("opening {}", state.root().display()))?;
    let settings = history.settings().clone();
    for name in remotes {
        if !settings.remotes.iter().any(|r| r.name == *name) {
            match settings.invalid_remotes.iter().find(|r| r.name == *name) {
                Some(bad) => bail!("remote {name} is misconfigured: {}", bad.error),
                None => bail!("no remote named {name:?} is configured"),
            }
        }
    }
    let wanted = wanted(&history, revisions, all)?;
    drop(history);
    let pointers: Vec<Pointer> = wanted.iter().map(|(p, _)| *p).collect();
    if dry_run {
        let cache = ObjectCache::at(state.lfs()).context("opening the LFS cache")?;
        let missing: Vec<&Pointer> = pointers.iter().filter(|p| !cache.contains(p)).collect();
        println!(
            "lfs fetch: {} referenced, {} cached",
            amount(pointers.len(), pointers.iter().map(|p| p.size).sum()),
            pointers.len() - missing.len()
        );
        println!(
            "dry run: would fetch {}",
            amount(missing.len(), missing.iter().map(|p| p.size).sum())
        );
        return Ok(ExitCode::SUCCESS);
    }
    let names = (!remotes.is_empty()).then_some(remotes);
    let report =
        fetch::fetch(state, &settings, &pointers, names).context("fetching LFS objects")?;
    print_report(&report, &wanted);
    Ok(if report.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn print_report(report: &FetchReport, wanted: &[(Pointer, RepoPath)]) {
    println!(
        "lfs fetch: {} referenced, {} already cached",
        amount(report.objects, report.object_bytes),
        report.cached
    );
    for remote in &report.remotes {
        match &remote.unavailable {
            Some(why) => println!("  {}: not asked: {why}", remote.remote),
            None if remote.failed > 0 => println!(
                "  {}: fetched {}; {} it could not provide",
                remote.remote,
                amount(remote.fetched, remote.bytes),
                remote.failed
            ),
            None => println!(
                "  {}: fetched {}",
                remote.remote,
                amount(remote.fetched, remote.bytes)
            ),
        }
    }
    let (fetched, bytes) = report.fetched();
    println!("fetched {}", amount(fetched, bytes));
    for (pointer, problems) in &report.failures {
        let path = wanted
            .iter()
            .find(|(p, _)| p.oid == pointer.oid)
            .map(|(_, path)| path.as_str())
            .unwrap_or("?");
        let why = if problems.is_empty() {
            "no remote to fetch it from".to_owned()
        } else {
            problems.join("; ")
        };
        eprintln!(
            "error: LFS object {} ({path}, {} bytes) could not be fetched: {why}",
            pointer.oid, pointer.size
        );
    }
}
