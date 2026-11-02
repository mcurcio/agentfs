//! The invariants (MODEL.md, "What must always hold"), read back from the
//! repository and the trees after every step. Each returns the first
//! violation found. Reads over git objects are cached in the [`World`]:
//! commits, their ancestry and blobs never change.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::content::{Content, PATHS, SLOTS, line_token, writer_of};
use crate::git::{git, try_git};
use crate::world::{
    CollisionScope, Direction, HistoryPolicy, Step, Tree, TxnEnd, TxnOutcome, World,
};

/// Every invariant that holds after any step.
pub fn check(w: &mut World) -> Result<(), String> {
    no_silent_loss(w)?;
    incoming_wins(w)?;
    syncs_deliver(w)?;
    transactions_end_as_told(w)?;
    let discards = w.grepped.0 != w.discarded.len();
    if w.refs_checked != Some(w.ref_moves) || discards {
        let all = w.all_commits();
        one_writer(w, &all)?;
        discarded_leave_no_trace(w, &all)?;
        w.refs_checked = Some(w.ref_moves);
    }
    history_setting(w)?;
    Ok(())
}

/// Once writes stop and every queue has drained: no step is left but
/// drops, and every live fork's tree equals main's. Stronger than "each
/// fork's untouched files equal main and touched files equal main or the
/// fork's newer non-colliding edit": once the queues drain, a fork has no
/// newer edit left.
pub fn check_converged(w: &World) -> Result<(), String> {
    if let Some(s) = w.pending().iter().find(|s| matches!(s, Step::Next(_))) {
        return Err(format!("not drained: {s:?} still pending"));
    }
    for t in w.live_trees() {
        for p in PATHS {
            let (mine, main) = (w.read(t, p), w.read(Tree::Main, p));
            if mine != main {
                return Err(format!(
                    "{t:?}:{p} never converged: {mine:?} vs main's {main:?}"
                ));
            }
        }
    }
    Ok(())
}

/// A collision in `trees` names `token`, or a later token written over it
/// knowingly, as lost on slot `slot` of `path`.
fn lost_in(w: &World, trees: &[Tree], path: &str, slot: usize, token: &str) -> bool {
    w.collisions.iter().any(|c| {
        trees.contains(&c.tree)
            && c.path == path
            && c.slots
                .iter()
                .zip(&c.lost)
                .any(|(s, l)| *s == slot && w.lineage.descends(l, token))
    })
}

/// [`lost_in`] `t` or a tree upstream of it: a loss decided upstream
/// reaches `t` as a plain update.
fn flagged(w: &World, t: Tree, path: &str, slot: usize, token: &str) -> bool {
    lost_in(w, &w.upstream(t), path, slot, token)
}

/// Main never silently loses a change, nor does any tree, and no stale
/// line reverts a newer one: (a) in every live tree a slot moves from
/// token a to token b only if b's writer saw a, or a recorded collision in
/// that tree or one upstream of it names a as lost; never if a's writer
/// saw b (b is stale: it was already superseded by a); (b) every token
/// that entered a tree (written there, or carried in by a landing, a sync
/// or a transaction, won or lost) is still there or overwritten by one that
/// saw it, or such a collision names it.
fn no_silent_loss(w: &mut World) -> Result<(), String> {
    let mut now: BTreeMap<Tree, BTreeMap<String, Content>> = BTreeMap::new();
    for t in w.live_trees() {
        now.insert(
            t,
            PATHS
                .iter()
                .map(|p| (p.to_string(), w.read(t, p)))
                .collect(),
        );
    }
    for (t, files) in &now {
        let Some(before) = w.views.get(t) else {
            continue;
        };
        for (p, c) in files {
            for i in 0..SLOTS {
                let (a, b) = (before[p].slot(i), c.slot(i));
                if a != b && w.lineage.descends(a, b) {
                    return Err(format!(
                        "stale revert in {t:?}:{p}[{i}]: {a} became {b}, which {a}'s writer had already seen"
                    ));
                }
                if a != b && !w.lineage.descends(b, a) && !flagged(w, *t, p, i, a) {
                    return Err(format!(
                        "silent loss in {t:?}:{p}[{i}]: {a} became {b}, whose writer never saw it, and no collision names it"
                    ));
                }
            }
        }
    }
    for (t, e) in &w.entered {
        let Some(files) = now.get(t) else {
            continue;
        };
        let s = files[&e.path].slot(e.slot);
        if !w.lineage.descends(s, &e.token) && !flagged(w, *t, &e.path, e.slot, &e.token) {
            return Err(format!(
                "{} entered {t:?}:{}[{}] and is gone ({s} there), with no collision naming it",
                e.token, e.path, e.slot
            ));
        }
    }
    w.views = now;
    Ok(())
}

/// The incoming change wins every same-line collision in the tree it is
/// applied to, and only a real one: in each recorded collision the result
/// holds the incoming commit's lines on the lost slots (its whole file
/// under `File`), each named in `won`; the lost and winning lines are
/// concurrent (neither's writer saw the other); the lost tokens were the
/// tree's and stay reachable under the collision's ref; the result's
/// stored parents are the tree's previous head and the incoming commit (a
/// sync, a transaction, or a landing under `FullLineage`), or under
/// `Squashed` the previous head alone, with the incoming commit as the
/// graft's second parent.
fn incoming_wins(w: &mut World) -> Result<(), String> {
    for c in &w.collisions[w.collisions_checked..] {
        let (r, win, lose) = (
            w.file(&c.result, &c.path),
            w.file(&c.winner, &c.path),
            w.file(&c.loser, &c.path),
        );
        let named = c.slots.len() == c.won.len()
            && c.slots.len() == c.lost.len()
            && c.slots.iter().zip(&c.won).all(|(i, t)| r.slot(*i) == t);
        let ok = match w.policies.collision {
            CollisionScope::Line => c.slots.iter().all(|i| r.slot(*i) == win.slot(*i)),
            CollisionScope::File => r == win,
        };
        if !ok || !named || c.slots.is_empty() {
            return Err(format!("{c:?}: the incoming change did not win"));
        }
        let concurrent =
            |(l, x): (&String, &String)| !w.lineage.descends(l, x) && !w.lineage.descends(x, l);
        let mut pairs = c.lost.iter().zip(&c.won);
        let real = match w.policies.collision {
            CollisionScope::Line => pairs.all(concurrent),
            CollisionScope::File => pairs.any(concurrent),
        };
        if !real {
            return Err(format!(
                "{c:?}: not a collision: one side's writer saw the other's line"
            ));
        }
        if let Some(t) = c.lost.iter().find(|t| !lose.contains(t)) {
            return Err(format!("{c:?}: lost {t} is not in the losing commit"));
        }
        let at = git(w.repo(), &["rev-parse", &c.reference]);
        if at != c.loser {
            return Err(format!("{c:?}: its ref names {at}"));
        }
        let (stored, lineage) = (
            parents(w.repo(), &c.result, false),
            parents(w.repo(), &c.result, true),
        );
        let both = [c.loser.clone(), c.winner.clone()];
        let ok = match (c.direction, w.policies.history) {
            // A fast-forward: the fork's commit itself, made on the tree's
            // head.
            (Direction::Landing, HistoryPolicy::FullLineage) if c.result == c.winner => {
                stored.first() == Some(&c.loser)
            }
            (Direction::Landing, HistoryPolicy::Squashed) => {
                stored == [c.loser.clone()] && lineage == both
            }
            _ => stored == both,
        };
        if !ok {
            return Err(format!(
                "{c:?}: result's parents are {stored:?}, in lineage {lineage:?}"
            ));
        }
    }
    w.collisions_checked = w.collisions.len();
    Ok(())
}

/// A commit's parents, as stored or in lineage (with grafts).
fn parents(repo: &Path, c: &str, lineage: bool) -> Vec<String> {
    let mut args = vec!["rev-list", "--parents", "-n1", c];
    if !lineage {
        args.insert(0, "--no-replace-objects");
    }
    git(repo, &args)
        .split(' ')
        .skip(1)
        .map(str::to_string)
        .collect()
}

/// Rule 1 and rule 3 per sync, the moment it is taken: the fork now holds
/// every line of the parent's version it carried, except a line the fork
/// holds newer (its writer saw the parent's) or one the fork already lost
/// in a collision recorded in the fork (it took that decision before and
/// moved on: rule 3, never brought back).
fn syncs_deliver(w: &mut World) -> Result<(), String> {
    for (t, up, result) in &w.syncs[w.syncs_checked..] {
        for p in PATHS {
            let (u, r) = (w.file(up, p), w.file(result, p));
            for i in 0..SLOTS {
                let (u, r) = (u.slot(i), r.slot(i));
                if r != u && !w.lineage.descends(r, u) && !lost_in(w, &[*t], p, i, u) {
                    return Err(format!(
                        "a sync of {up:.7} into {t:?} left {p}[{i}] at {r}, not the parent's {u}"
                    ));
                }
            }
        }
    }
    w.syncs_checked = w.syncs.len();
    Ok(())
}

/// Rule 5, as the program was told: a committed transaction's every
/// staged edit is in the fork's commit that carries it (or overwritten by
/// a later edit of its own); that commit then entered the fork, so
/// `no_silent_loss` holds it from there. A rolled-back one's every staged
/// token is discarded (`discarded_leave_no_trace` holds that);
/// an empty one staged nothing.
fn transactions_end_as_told(w: &mut World) -> Result<(), String> {
    for e in &w.ended[w.ended_checked..] {
        let ok = match (e.how, &e.result) {
            (TxnEnd::Commit(TxnOutcome::Committed), Some(r)) => e.staged.iter().all(|s| {
                w.lineage
                    .descends(w.file(r, &s.path).slot(s.slot), &s.token)
                    && w.entered.iter().any(|(t, x)| {
                        *t == Tree::Fork(e.fork)
                            && x.path == s.path
                            && x.slot == s.slot
                            && w.lineage.descends(&x.token, &s.token)
                    })
            }),
            (TxnEnd::Commit(TxnOutcome::Empty), None) => e.staged.is_empty(),
            (TxnEnd::Rollback, None) => e.staged.iter().all(|s| w.discarded.contains(&s.token)),
            _ => false,
        };
        if !ok {
            return Err(format!(
                "t{} on f{} ended {:?} but its staged edits {:?} are not where that says",
                e.txn, e.fork, e.how, e.staged
            ));
        }
    }
    w.ended_checked = w.ended.len();
    Ok(())
}

/// Rule 6: every commit names exactly one writer; a non-merge commit's
/// change (its diff from its parent) is that writer's alone; a merge
/// originates nothing: every line it adds over its first parent comes from
/// another parent, and it names the writer of its second parent (a landing
/// or a transaction's commit), or `agentfs` if it is a sync. So every token
/// traces through ancestry to the one non-merge commit that wrote it, which
/// names its writer. Each commit is checked once: commits never change.
fn one_writer(w: &mut World, all: &[String]) -> Result<(), String> {
    let new: Vec<String> = all
        .iter()
        .filter(|c| !w.checked_commits.contains(*c))
        .cloned()
        .collect();
    if new.is_empty() {
        return Ok(());
    }
    let mut args = vec![
        "--no-replace-objects".to_string(),
        "log".to_string(),
        "--no-walk=unsorted".into(),
        "--diff-merges=first-parent".into(),
        "-p".into(),
        "--format=%x01%H%x1f%P%x1f%(trailers:key=Agentfs-Writer,valueonly,separator=%x1e)%x1f%(trailers:key=Agentfs-Cause,valueonly)%x02".into(),
    ];
    args.extend(new.iter().cloned());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let log = git(w.repo(), &args);
    for block in log.split('\x01').skip(1) {
        let (header, diff) = block.split_once('\x02').unwrap_or((block, ""));
        let fields: Vec<&str> = header.split('\x1f').collect();
        let [commit, parents, writers, cause] = fields[..] else {
            return Err(format!("unparsable log header {header:?}"));
        };
        let parents: Vec<&str> = parents.split_whitespace().collect();
        let writers: Vec<&str> = writers.split('\x1e').filter(|s| !s.is_empty()).collect();
        if writers.len() != 1 {
            return Err(format!("{commit} names writers {writers:?}"));
        }
        if parents.len() > 1 {
            let expect = match cause.trim() {
                "Sync" => "agentfs".to_string(),
                _ => w.writer_of_commit(parents[1]),
            };
            if writers[0] != expect {
                return Err(format!(
                    "{commit} ({}) names {} but its second parent's writer is {expect}",
                    cause.trim(),
                    writers[0]
                ));
            }
        }
        for (path, slot, token) in added(diff) {
            let bad = match parents.len() {
                0 => false,
                1 => writer_of(&token) != writers[0],
                _ => !parents[1..]
                    .iter()
                    .any(|p| w.file(p, &path).slot(slot) == token),
            };
            if bad {
                return Err(format!(
                    "{commit} ({}, parents {}) names {} but adds {token} in {path}[{slot}]",
                    cause.trim(),
                    parents.len(),
                    writers[0]
                ));
            }
        }
    }
    w.checked_commits.extend(new);
    Ok(())
}

/// `(path, slot, token)` of every added slot line in a unified diff.
fn added(diff: &str) -> Vec<(String, usize, String)> {
    let mut out = Vec::new();
    let mut path = String::new();
    for line in diff.lines() {
        if let Some(p) = line.strip_prefix("+++ b/") {
            path = p.to_string();
        } else if let Some(rest) = line.strip_prefix('+')
            && !line.starts_with("+++")
            && let Some(token) = line_token(rest)
            && let Some(slot) = rest
                .split_once(':')
                .and_then(|(k, _)| k.strip_prefix('s')?.parse().ok())
        {
            out.push((path.clone(), slot, token.to_string()));
        }
    }
    out
}

/// A rolled-back transaction leaves no content in any tree,
/// and no commit reachable from any ref. Each commit is grepped once,
/// until a new discard adds tokens.
fn discarded_leave_no_trace(w: &mut World, all: &[String]) -> Result<(), String> {
    if w.discarded.is_empty() {
        return Ok(());
    }
    for t in w.live_trees() {
        for p in PATHS {
            if let Some(tok) = w.read(t, p).tokens().find(|x| w.discarded.contains(*x)) {
                return Err(format!("discarded {tok} shows in {t:?}:{p}"));
            }
        }
    }
    if w.grepped.0 != w.discarded.len() {
        w.grepped = (w.discarded.len(), Default::default());
    }
    let new: Vec<String> = all
        .iter()
        .filter(|c| !w.grepped.1.contains(*c))
        .cloned()
        .collect();
    if new.is_empty() {
        return Ok(());
    }
    let mut args: Vec<String> = vec!["grep".into(), "-l".into(), "-F".into()];
    for t in &w.discarded {
        args.extend(["-e".into(), t.clone()]);
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
            "discarded content reachable in history:\n{}",
            out.stdout
        )),
        _ => Err(format!("git grep failed: {}", out.stderr)),
    }
}

/// Rule 7: a tree's lineage reaches every commit that landed in it; its
/// stored history does too under `FullLineage`, and under `Squashed` none,
/// main's holding only main's own commits. Read by ancestry alone (`git
/// rev-list <head>`, never a `from..to` range, whose walk stops early by
/// commit date).
fn history_setting(w: &mut World) -> Result<(), String> {
    let full = w.policies.history == HistoryPolicy::FullLineage;
    let repo = w.repo().to_path_buf();
    let mut heads: Vec<String> = w
        .landings
        .iter()
        .filter(|(t, _)| w.alive(*t))
        .map(|(t, _)| w.head(*t))
        .collect();
    heads.push(w.main_head.clone());
    for head in heads {
        w.reach
            .entry(head.clone())
            .or_insert_with(|| (reach(&repo, &head, false), reach(&repo, &head, true)));
    }
    for (t, c) in &w.landings {
        if !w.alive(*t) {
            continue;
        }
        let (history, lineage) = &w.reach[&w.head(*t)];
        if history.contains(c) != full {
            return Err(format!(
                "{t:?}'s history reaches landed {c}: {}, under {:?}",
                !full, w.policies.history
            ));
        }
        if !lineage.contains(c) {
            return Err(format!("{t:?}'s lineage lost landed {c}"));
        }
    }
    if full {
        return Ok(());
    }
    let new: Vec<String> = w.reach[&w.main_head]
        .0
        .iter()
        .filter(|c| !w.main_history.contains(*c))
        .cloned()
        .collect();
    if new.is_empty() {
        return Ok(());
    }
    let mut args = vec![
        "--no-replace-objects",
        "log",
        "--no-walk=unsorted",
        "--format=%H %(trailers:key=Agentfs-Tree,valueonly)",
    ];
    args.extend(new.iter().map(String::as_str));
    if let Some(l) = git(&repo, &args)
        .lines()
        .find(|l| !l.is_empty() && !l.ends_with(" main"))
    {
        return Err(format!("Squashed: main's history reaches {l}"));
    }
    w.main_history.extend(new);
    Ok(())
}

/// Every commit `head` reaches, as stored or in lineage (with grafts).
fn reach(repo: &Path, head: &str, lineage: bool) -> BTreeSet<String> {
    let mut args = vec!["rev-list", head];
    if !lineage {
        args.insert(0, "--no-replace-objects");
    }
    git(repo, &args).lines().map(str::to_string).collect()
}
