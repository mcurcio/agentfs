//! The global invariants, checked against the git repository and the
//! model's trees after every step. Each returns the first violation found.

use std::collections::{BTreeMap, BTreeSet};

use crate::content::{PATHS, line_token, writer_of};
use crate::git::{git, try_git};
use crate::world::{HistoryPolicy, Life, Tree, World};

/// Which checks run. `fork_lines` adds the content-level one-writer check on
/// fork lines and squash chains, which design findings 1 to 3 break.
#[derive(Clone, Copy, Debug, Default)]
pub struct Checks {
    pub fork_lines: bool,
}

/// Every invariant that holds after any step.
pub fn check(w: &mut World, checks: Checks) -> Result<(), String> {
    no_silent_loss(w)?;
    landed_changes_stay(w)?;
    ordered_prefix(w)?;
    one_writer_per_commit(w, checks)?;
    rollback_leaves_no_trace(w)?;
    Ok(())
}

/// Once nothing can act (handles closed, landings settled): every fork
/// commit is landed, behind a held one, withdrawn, or gone with its fork;
/// no sealed transaction is left unsettled.
pub fn check_quiescent(w: &World) -> Result<(), String> {
    for f in w.forks.values() {
        if f.life == Life::Dropped {
            continue;
        }
        if f.is_txn() {
            if f.txn == Some(crate::world::TxnState::Sealed) {
                return Err(format!("t{}: sealed but its run never settled", f.id));
            }
            continue;
        }
        if f.landed < f.commits.len() && f.held.is_none() && !f.withdrawn {
            return Err(format!(
                "f{}: {} of {} commits landed, none held or withdrawn",
                f.id,
                f.landed,
                f.commits.len()
            ));
        }
    }
    if !w.pending().is_empty() {
        return Err(format!("still pending: {:?}", w.pending()));
    }
    Ok(())
}

/// system.md I3, I4 and A8: in every tree, a slot moves from token a to
/// token b only if b's writer saw a (or something that did). Main never
/// silently loses a landed change; a sync never silently drops a fork's own
/// edit. Checked on main and every editable fork; a transaction fork is the
/// client's private staging.
fn no_silent_loss(w: &mut World) -> Result<(), String> {
    let mut now: BTreeMap<Tree, BTreeMap<String, crate::content::Content>> = BTreeMap::new();
    let mut trees = vec![Tree::Main];
    trees.extend(
        w.forks
            .values()
            .filter(|f| f.life != Life::Dropped && !f.is_txn())
            .map(|f| Tree::Fork(f.id)),
    );
    for t in trees {
        let files = PATHS
            .iter()
            .map(|p| (p.to_string(), w.read(t, p)))
            .collect();
        now.insert(t, files);
    }
    for (t, files) in &now {
        let Some(before) = w.views.get(t) else {
            continue;
        };
        for (p, c) in files {
            let old = &before[p];
            for i in 0..crate::content::SLOTS {
                let (a, b) = (old.slot(i), c.slot(i));
                if a != b && !w.lineage.descends(b, a) {
                    return Err(format!(
                        "silent loss in {t:?}:{p}[{i}]: {a} became {b}, whose writer never saw it"
                    ));
                }
            }
        }
    }
    w.views = now;
    Ok(())
}

/// system.md I3: a landed change stays in the tree it landed in, or is
/// replaced only by a change that saw it.
fn landed_changes_stay(w: &World) -> Result<(), String> {
    for l in &w.landed {
        if let Tree::Fork(f) = l.tree
            && w.forks[&f].life == Life::Dropped
        {
            continue;
        }
        let v = w.read(l.tree, &l.edit.path);
        let now = v.slot(l.edit.slot);
        if !w.lineage.descends(now, &l.edit.token) {
            return Err(format!(
                "landed {} in {:?}:{}[{}] is gone: {now} replaced it unseen",
                l.edit.token, l.tree, l.edit.path, l.edit.slot
            ));
        }
    }
    Ok(())
}

/// system.md I3, I4, A8 and R12: a fork's landed commits are a prefix of
/// its commits, never past a held one; the parent's line reaches exactly
/// that prefix under `FullLineage`, and no fork-line commit under
/// `Squashed`; main's ancestry never reaches a transaction's commits.
fn ordered_prefix(w: &World) -> Result<(), String> {
    let mut reach: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let repo = w.repo().to_path_buf();
    let mut reachable = |head: &str| -> BTreeSet<String> {
        reach
            .entry(head.to_string())
            .or_insert_with(|| {
                git(&repo, &["rev-list", head])
                    .lines()
                    .map(str::to_string)
                    .collect()
            })
            .clone()
    };
    for f in w.forks.values().filter(|f| !f.is_txn()) {
        if let Some(h) = f.held
            && (f.landed >= f.commits.len() || h < f.landed)
        {
            return Err(format!("f{}: held, yet nothing waits", f.id));
        }
        let parent_alive = match f.parent {
            Tree::Main => true,
            Tree::Fork(p) => w.forks[&p].life != Life::Dropped,
        };
        if !parent_alive {
            continue;
        }
        let r = reachable(&w.head(f.parent));
        for (i, c) in f.commits.iter().enumerate() {
            let expected = w.policies.history == HistoryPolicy::FullLineage && i < f.landed;
            if r.contains(&c.git) != expected {
                return Err(format!(
                    "f{} commit {i} ({}): reached by its parent's line: {}, should be: {expected}",
                    f.id, c.git, !expected
                ));
            }
        }
    }
    let trees = git(
        &repo,
        &[
            "log",
            &w.main_head,
            "--format=%H %(trailers:key=Agentfs-Tree,valueonly,separator=%x2C)",
        ],
    );
    for line in trees.lines() {
        let (c, tree) = line.split_once(' ').unwrap_or((line, ""));
        let allowed = tree == "main"
            || (tree.starts_with("fork/") && w.policies.history == HistoryPolicy::FullLineage)
            || (tree.starts_with("squash/") && w.policies.history == HistoryPolicy::Squashed);
        if !allowed {
            return Err(format!("main's ancestry reaches {c} of {tree}"));
        }
    }
    Ok(())
}

/// system.md I2 and R6: every commit names exactly one writer, and every
/// change a commit makes (its diff from its first parent) is that writer's.
/// Restore and the root commit are an operator's and agentfs's own whole
/// content; a transaction's staging branch is not history (it has no line).
fn one_writer_per_commit(w: &World, checks: Checks) -> Result<(), String> {
    let log = git(
        w.repo(),
        &[
            "log",
            "--all",
            "--diff-merges=first-parent",
            "-p",
            "--format=%x01%H%x1f%(trailers:key=Agentfs-Writer,valueonly,separator=%x1e)%x1f%(trailers:key=Agentfs-Cause,valueonly,separator=%x1e)%x1f%(trailers:key=Agentfs-Tree,valueonly,separator=%x1e)%x02",
        ],
    );
    for block in log.split('\x01').skip(1) {
        let (header, diff) = block.split_once('\x02').unwrap_or((block, ""));
        let fields: Vec<&str> = header.split('\x1f').collect();
        let [commit, writers, cause, tree] = fields[..] else {
            return Err(format!("unparsable log header {header:?}"));
        };
        let writers: Vec<&str> = writers.split('\x1e').filter(|s| !s.is_empty()).collect();
        if writers.len() != 1 {
            return Err(format!("{commit} ({tree}) names writers {writers:?}"));
        }
        let writer = writers[0];
        let content_checked = if tree == "main" {
            !cause.starts_with("Restore") && cause != "Root"
        } else {
            checks.fork_lines && (tree.starts_with("fork/") || tree.starts_with("squash/"))
        };
        if !content_checked {
            continue;
        }
        for line in diff.lines() {
            if let Some(rest) = line.strip_prefix('+')
                && !line.starts_with("+++")
                && let Some(token) = line_token(rest)
                && writer_of(token) != writer
            {
                return Err(format!(
                    "{commit} ({tree}, {cause}) names {writer} but carries {token}"
                ));
            }
        }
    }
    Ok(())
}

/// A13, I3: a rolled-back transaction leaves no content in any tree and no
/// commit reachable from any ref.
fn rollback_leaves_no_trace(w: &World) -> Result<(), String> {
    if w.rolled_back.is_empty() {
        return Ok(());
    }
    for f in std::iter::once(Tree::Main).chain(
        w.forks
            .values()
            .filter(|f| f.life != Life::Dropped)
            .map(|f| Tree::Fork(f.id)),
    ) {
        for p in PATHS {
            let c = w.read(f, p);
            if let Some(t) = c.tokens().find(|t| w.rolled_back.contains(*t)) {
                return Err(format!("rolled-back {t} shows in {f:?}:{p}"));
            }
        }
    }
    let revs = git(w.repo(), &["rev-list", "--all"]);
    let mut args: Vec<String> = vec!["grep".into(), "-l".into(), "-F".into()];
    for t in &w.rolled_back {
        args.push("-e".into());
        args.push(t.clone());
    }
    args.extend(revs.lines().map(str::to_string));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = try_git(w.repo(), &args, None);
    match out.code {
        1 => Ok(()),
        0 => Err(format!(
            "rolled-back content reachable in history:\n{}",
            out.stdout
        )),
        _ => Err(format!("git grep failed: {}", out.stderr)),
    }
}
