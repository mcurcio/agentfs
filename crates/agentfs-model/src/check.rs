//! The global invariants, checked against the git repository and the
//! model's trees after every step. Each returns the first violation found.
//! Reads over git objects are cached in the [`World`]: commits, their
//! ancestry and blobs never change, so the cache cannot disagree with git.

use std::collections::{BTreeMap, BTreeSet};

use crate::content::{PATHS, SLOTS, line_token, writer_of};
use crate::git::{git, try_git};
use crate::world::{Base, HistoryPolicy, Life, RollbackCause, SealConflict, Tree, TxnState, World};

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
    unlanded_stays_out(w)?;
    ordered_prefix(w)?;
    one_writer_per_commit(w, checks)?;
    rollback_leaves_no_trace(w)?;
    seals_settle(w)?;
    handles_have_trees(w)?;
    bases_resolve(w)?;
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
            if f.txn == Some(TxnState::Sealed) {
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
/// token b only if b's writer saw a (or something that did), or the tree's
/// own client knowingly took b over a (`World::accepted`). Main never
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
            for i in 0..SLOTS {
                let (a, b) = (old.slot(i), c.slot(i));
                // An accepted change shows in the tree that made it and every
                // tree reading through it.
                if a != b
                    && !w.lineage.descends(b, a)
                    && !w.reads_through(*t).into_iter().any(|by| {
                        w.accepted
                            .contains(&(by, p.clone(), i, a.to_string(), b.to_string()))
                    })
                {
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
/// replaced only by a change that saw it (or one that tree's client took
/// over it knowingly).
fn landed_changes_stay(w: &World) -> Result<(), String> {
    for l in &w.landed {
        if let Tree::Fork(f) = l.tree
            && w.forks[&f].life == Life::Dropped
        {
            continue;
        }
        let v = w.read(l.tree, &l.edit.path);
        let now = v.slot(l.edit.slot);
        let chain = w.reads_through(l.tree);
        let taken = w.accepted.iter().any(|(t, p, i, from, _)| {
            chain.contains(t) && *p == l.edit.path && *i == l.edit.slot && *from == l.edit.token
        });
        if !w.lineage.descends(now, &l.edit.token) && !taken {
            return Err(format!(
                "landed {} in {:?}:{}[{}] is gone: {now} replaced it unseen",
                l.edit.token, l.tree, l.edit.path, l.edit.slot
            ));
        }
    }
    Ok(())
}

/// system.md I3 and A8 ("nothing lands past one that cannot merge") and I4
/// ("a tree sees another tree's content only after it lands there"), in
/// content: no token of a fork's unlanded commits shows in its parent's
/// tree, nor a transaction's staged write before its seal lands.
fn unlanded_stays_out(w: &World) -> Result<(), String> {
    for f in w.forks.values().filter(|f| f.life != Life::Dropped) {
        for c in &f.commits[f.landed..] {
            for e in &c.edits {
                if w.read(f.parent, &e.path).contains(&e.token) {
                    return Err(format!(
                        "f{}'s unlanded {} shows in {:?}:{}",
                        f.id, e.token, f.parent, e.path
                    ));
                }
            }
        }
    }
    Ok(())
}

/// system.md I3, I4, A8 and R12: a fork's landed commits are a prefix of
/// its commits, held only at the first unlanded one; the parent's line
/// (main's with its uncommitted tail) reaches exactly that prefix under
/// `FullLineage`, and no fork-line commit under `Squashed`; main's ancestry
/// never reaches a transaction's commits.
fn ordered_prefix(w: &mut World) -> Result<(), String> {
    let repo = w.repo().to_path_buf();
    let mut reach = std::mem::take(&mut w.reach);
    let mut reachable = |tips: &[String]| -> BTreeSet<String> {
        let mut all = BTreeSet::new();
        for tip in tips {
            let r = reach.entry(tip.clone()).or_insert_with(|| {
                git(&repo, &["rev-list", tip])
                    .lines()
                    .map(str::to_string)
                    .collect()
            });
            all.extend(r.iter().cloned());
        }
        all
    };
    let mut result = Ok(());
    for f in w.forks.values().filter(|f| !f.is_txn()) {
        if let Some(h) = f.held
            && (h != f.landed || f.landed >= f.commits.len())
        {
            result = Err(format!(
                "f{}: held at {h} with {} of {} landed",
                f.id,
                f.landed,
                f.commits.len()
            ));
            break;
        }
        let parent_alive = match f.parent {
            Tree::Main => true,
            Tree::Fork(p) => w.forks[&p].life != Life::Dropped,
        };
        if !parent_alive {
            continue;
        }
        let r = reachable(&w.line_tips(f.parent));
        let wrong = f.commits.iter().enumerate().find_map(|(i, c)| {
            let expected = w.policies.history == HistoryPolicy::FullLineage && i < f.landed;
            (r.contains(&c.git) != expected).then_some((i, c.git.clone(), expected))
        });
        if let Some((i, c, expected)) = wrong {
            result = Err(format!(
                "f{} commit {i} ({c}): reached by its parent's line: {}, should be: {expected}",
                f.id, !expected
            ));
            break;
        }
    }
    w.reach = reach;
    result?;
    let range = match &w.main_checked {
        Some(prev) => format!("{prev}..{}", w.main_head),
        None => w.main_head.clone(),
    };
    let trees = git(
        &repo,
        &[
            "log",
            &range,
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
    w.main_checked = Some(w.main_head.clone());
    Ok(())
}

/// system.md I2 and R6: every commit names exactly one writer, and every
/// change a commit makes (its diff from its first parent) is that writer's.
/// The root commit is agentfs's own whole content. A resolve is "an actor's
/// merge, named for that actor" (landing.md §3), so it carries the
/// sandbox's lines by design. A transaction's staging branch is not history
/// (it has no line). Each commit is checked once: commits never change.
fn one_writer_per_commit(w: &mut World, checks: Checks) -> Result<(), String> {
    let all = git(w.repo(), &["rev-list", "--all"]);
    let new: Vec<&str> = all
        .lines()
        .filter(|c| !w.checked_commits.contains(*c))
        .collect();
    if new.is_empty() {
        return Ok(());
    }
    let mut args = vec![
        "log",
        "--no-walk=unsorted",
        "--diff-merges=first-parent",
        "-p",
        "--format=%x01%H%x1f%(trailers:key=Agentfs-Writer,valueonly,separator=%x1e)%x1f%(trailers:key=Agentfs-Cause,valueonly,separator=%x1e)%x1f%(trailers:key=Agentfs-Tree,valueonly,separator=%x1e)%x02",
    ];
    args.extend(new.iter().copied());
    let log = git(w.repo(), &args);
    let mut passed = Vec::new();
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
            cause != "Root" && !cause.starts_with("Landing Resolve")
        } else {
            checks.fork_lines && (tree.starts_with("fork/") || tree.starts_with("squash/"))
        };
        if content_checked {
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
        passed.push(commit.to_string());
    }
    w.checked_commits.extend(passed);
    Ok(())
}

/// A13, I3: a rolled-back transaction leaves no content in any tree and no
/// commit reachable from any ref. Each commit is grepped once, until a new
/// rollback adds tokens.
fn rollback_leaves_no_trace(w: &mut World) -> Result<(), String> {
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
    if w.grepped.0 != w.rolled_back.len() {
        w.grepped = (w.rolled_back.len(), BTreeSet::new());
    }
    let revs = git(w.repo(), &["rev-list", "--all"]);
    let new: Vec<String> = revs
        .lines()
        .filter(|c| !w.grepped.1.contains(*c))
        .map(str::to_string)
        .collect();
    if new.is_empty() {
        return Ok(());
    }
    let mut args: Vec<String> = vec!["grep".into(), "-l".into(), "-F".into()];
    for t in &w.rolled_back {
        args.push("-e".into());
        args.push(t.clone());
    }
    args.extend(new.iter().cloned());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = try_git(w.repo(), &args, None);
    match out.code {
        1 => {
            w.grepped.1.extend(new);
            Ok(())
        }
        0 => Err(format!(
            "rolled-back content reachable in history:\n{}",
            out.stdout
        )),
        _ => Err(format!("git grep failed: {}", out.stderr)),
    }
}

/// system.md §3.6, "Seal" and the open question: a sealed transaction
/// still waits (`Sealed`) or ended `Landed`; `Held` only under the `Held`
/// policy; rolled back only by its own conflict under `Rollback`, or by an
/// actor's withdraw of a held seal. Never by its program exiting,
/// abandoning, or anything else.
fn seals_settle(w: &World) -> Result<(), String> {
    let policy = w.policies.seal_conflict;
    for t in w.forks.values().filter(|f| f.is_txn() && f.sealed) {
        let ok = match t.txn {
            Some(TxnState::Sealed | TxnState::Landed) => true,
            Some(TxnState::Held) => policy == SealConflict::Held,
            Some(TxnState::RolledBack) => matches!(
                (policy, t.rollback_cause),
                (SealConflict::Rollback, Some(RollbackCause::SealConflict))
                    | (SealConflict::Held, Some(RollbackCause::Withdrawn))
            ),
            Some(TxnState::Open) | None => false,
        };
        if !ok {
            return Err(format!(
                "t{}: sealed, then {:?} ({:?}) under {policy:?}",
                t.id, t.txn, t.rollback_cause
            ));
        }
    }
    Ok(())
}

/// No write-open outlives the tree it marks: a seal enters `Writing` only
/// after T's last write-open closes (§3.6), and a dropped fork has none.
fn handles_have_trees(w: &World) -> Result<(), String> {
    for (h, t) in w.handle_trees() {
        if let Tree::Fork(f) = t
            && w.forks[&f].life == Life::Dropped
        {
            return Err(format!("h{h} is open in dropped {t:?}"));
        }
    }
    Ok(())
}

/// system.md I11, forks.md F3: a moved base never names what grouping or
/// truncation removes. A `Kept` base is one of the fork's own fork commits
/// (its end, on its line); a captured one is an object in the repository.
fn bases_resolve(w: &mut World) -> Result<(), String> {
    let mut blobs = Vec::new();
    for f in w.forks.values().filter(|f| f.life != Life::Dropped) {
        for (p, entries) in &f.bases {
            for b in entries.values() {
                match b {
                    Base::Kept { commit } => {
                        if !f.commits.iter().any(|c| c.git == *commit) {
                            return Err(format!(
                                "f{}:{p}: base {commit} is not one of its fork commits",
                                f.id
                            ));
                        }
                    }
                    Base::Captured { blob } if !w.blobs_ok.contains(blob) => {
                        blobs.push((f.id, p.clone(), blob.clone()));
                    }
                    Base::Captured { .. } | Base::Trimmed => {}
                }
            }
        }
    }
    for (f, p, blob) in blobs {
        if try_git(w.repo(), &["cat-file", "-e", &blob], None).code != 0 {
            return Err(format!("f{f}:{p}: captured base {blob} is gone"));
        }
        w.blobs_ok.insert(blob);
    }
    Ok(())
}
