//! `agentfs lfs` (design §7, §17.1): the local LFS object cache.
//!
//! - `fetch` downloads into `<state>/lfs/objects` the objects that
//!   commits reference: the branch head's tree by default, the trees of
//!   the revisions given, or every commit on the branch (`--all`), from
//!   the remotes in name order (or only those named). It takes no
//!   state-area lock, so it runs beside a daemon, and holds the cache lock
//!   shared. Exit 0 when every object is in the cache afterwards, 1
//!   otherwise.
//! - `prune` deletes from the cache the objects every remote's LFS
//!   endpoints confirm they hold ([`agentfs_core::lfs::prune`]); only an
//!   operator runs it, and nothing on a server is ever deleted. It takes
//!   the cache lock exclusive for the deletions, waiting `--wait-lock` for
//!   restores and fetches holding it, and exits 75 if they still do.

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::Subcommand;

use agentfs_core::config::parse_duration;
use agentfs_core::history::History;
use agentfs_core::lfs::fetch::{self, FetchReport};
use agentfs_core::lfs::prune::{self, PrunePlan};
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
    /// Delete from the local cache the LFS objects every remote's LFS
    /// endpoints confirm they hold (never anything on a server). Refuses
    /// when a remote is misconfigured or has no LFS endpoint. Waits
    /// --wait-lock for restores and fetches using the cache; exit 75 if
    /// they still do.
    Prune {
        /// Report what would be deleted; delete nothing.
        #[arg(long)]
        dry_run: bool,
        /// Keep objects cached less than DURATION ago (e.g. 24h).
        #[arg(long, value_name = "DURATION", value_parser = parse_age)]
        older_than: Option<Duration>,
    },
}

fn parse_age(value: &str) -> Result<Duration, String> {
    parse_duration(value).ok_or_else(|| format!("{value:?} is not a duration (use e.g. 30m, 24h)"))
}

/// Exit status when the cache lock is busy (`EX_TEMPFAIL`).
const EXIT_LOCKED: u8 = 75;

pub fn run(command: LfsCommand, state: &StateDir, wait_lock: Duration) -> Result<ExitCode> {
    match command {
        LfsCommand::Fetch {
            revisions,
            all,
            remotes,
            dry_run,
        } => fetch_cmd(state, &revisions, all, &remotes, dry_run),
        LfsCommand::Prune {
            dry_run,
            older_than,
        } => prune_cmd(state, dry_run, older_than, wait_lock),
    }
}

fn prune_cmd(
    state: &StateDir,
    dry_run: bool,
    older_than: Option<Duration>,
    wait_lock: Duration,
) -> Result<ExitCode> {
    let (history, _) =
        volume::inspect(state).with_context(|| format!("opening {}", state.root().display()))?;
    let settings = history.settings().clone();
    drop(history);
    let plan = prune::plan(state, &settings, older_than)?;
    print_plan(&plan, older_than);
    if dry_run {
        println!(
            "dry run: would prune {}",
            amount(plan.confirmed.len(), plan.confirmed_bytes())
        );
        return Ok(ExitCode::SUCCESS);
    }
    let Some(report) = prune::apply(state, &plan, wait_lock)? else {
        eprintln!(
            "error: the LFS cache is in use (a restore, revert, patch apply or fetch holds \
             {}/lock); nothing was pruned. Retry, or wait longer with --wait-lock",
            state.lfs().display()
        );
        return Ok(ExitCode::from(EXIT_LOCKED));
    };
    println!("pruned {}", amount(report.pruned, report.bytes));
    if report.partials > 0 {
        println!(
            "removed {} unfinished download(s), {} byte(s)",
            report.partials, report.partial_bytes
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn print_plan(plan: &PrunePlan, older_than: Option<Duration>) {
    println!(
        "lfs prune: {} cached",
        amount(plan.cached, plan.cached_bytes)
    );
    if plan.recent > 0 {
        println!(
            "  {} object(s) cached less than {:?} ago are kept",
            plan.recent,
            older_than.unwrap_or_default()
        );
    }
    if plan.no_remotes {
        println!("  no remote is configured: nothing can be confirmed, nothing is pruned");
    }
    for checked in &plan.checked {
        println!(
            "  {} ({}): {} confirmed at {}",
            checked.remote,
            checked.direction.operation(),
            checked.confirmed,
            checked.endpoint
        );
    }
    let kept_bytes: u64 = plan.kept.iter().map(|(p, _)| p.size).sum();
    if !plan.kept.is_empty() {
        let lacking: Vec<String> = plan
            .lacking()
            .into_iter()
            .map(|(remote, n)| format!("{remote} lacks {n}"))
            .collect();
        println!(
            "kept {} that not every remote holds ({})",
            amount(plan.kept.len(), kept_bytes),
            lacking.join(", ")
        );
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
