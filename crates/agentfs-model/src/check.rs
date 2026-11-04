//! The invariants (MODEL.md, "What must always hold"), read back from the
//! repository and the trees after every step. Each returns the first
//! violation found.
//!
//! git holds bytes only. The checks read versions through the oracle
//! (`content::Oracle`): every tracked line's bytes name the version that
//! wrote them, and the oracle records what each version was written over,
//! from the write calls, never from a merge.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::content::{File, tracked, writer_of};
use crate::git::{git, try_git};
use crate::world::{
    Client, Collision, CollisionScope, Direction, Entry, ForkId, HistoryPolicy, Kind, Life, Step,
    Take, Target, Tree, TxnEnd, TxnId, TxnOutcome, World, commit_parents,
};

/// Every invariant that holds after any step.
pub fn check(w: &mut World) -> Result<(), String> {
    read_collisions(w);
    let now = views(w);
    no_silent_loss(w, &now)?;
    no_flicker(w, &now)?;
    for (t, v) in &now {
        w.ever.entry(*t).or_default().extend(v.iter().cloned());
    }
    w.views = now;
    locks(w)?;
    parent_wins(w)?;
    syncs_deliver(w)?;
    transactions_end_as_told(w)?;
    let discards = w.grepped.0 != w.discarded.len();
    if w.refs_checked != Some(w.ref_moves) || discards {
        let all = w.all_commits();
        one_writer(w, &all)?;
        discarded_leave_no_trace(w, &all)?;
        originals_kept(w, true)?;
        w.refs_checked = Some(w.ref_moves);
    }
    history_setting(w)?;
    Ok(())
}

/// One step, with what only the step itself shows: a take waits or holds
/// its files; a sync reads the parent's head as it is then; a finished take
/// wrote no file open for writing there nor one it did not hold; a sync
/// records no collision and carries the head its take read.
pub fn step_checked(w: &mut World, s: &Step) -> Result<usize, String> {
    match s {
        Step::Next(t) => {
            let up = match t {
                Tree::Fork(f) if w.queue(*t).first() == Some(&Entry::Sync) => {
                    Some(w.head(w.forks[f].parent))
                }
                _ => None,
            };
            let taken = w.step(s);
            if taken == 0 {
                return match w.waits().get(t) {
                    Some(_) if w.holds().get(t).is_none() => Ok(0),
                    _ => Err(format!("{t:?}'s queue head neither waits nor is taken")),
                };
            }
            match (up, w.holds().get(t).map(|h| &h.take)) {
                (Some(up), Some(Take::Sync { up: read, .. })) if *read != up => Err(format!(
                    "a sync into {t:?} read {read:.7}, not the parent's head {up:.7}"
                )),
                (_, None) => Err(format!("{t:?} took {taken} entries and holds nothing")),
                _ => Ok(taken),
            }
        }
        Step::Finish(t) => {
            let hold = w.holds().get(t).cloned().ok_or("no take to finish")?;
            let open = w.write_open(*t);
            let before = w.head(*t);
            let (collisions, syncs) = (w.collisions.len(), w.syncs.len());
            w.taken = None;
            w.step(s);
            let after = w.taken.clone().ok_or("a finished take left no head")?;
            let wrote = if before == after {
                BTreeSet::new()
            } else {
                w.changed(&before, &after)
            };
            if let Some(p) = wrote.iter().find(|p| open.contains(*p)) {
                return Err(format!(
                    "a take into {t:?} wrote {p}, which a client there has open for writing"
                ));
            }
            if let Some(p) = wrote.iter().find(|p| !hold.files.contains(*p)) {
                return Err(format!(
                    "a take into {t:?} wrote {p}, which it did not hold"
                ));
            }
            let Take::Sync { up, .. } = &hold.take else {
                return Ok(0);
            };
            if w.collisions.len() != collisions {
                return Err(format!(
                    "a sync into {t:?} recorded a collision: only the parent's queue decides one"
                ));
            }
            match w.syncs.get(syncs) {
                Some((_, carried, _, _)) if carried != up => Err(format!(
                    "a sync into {t:?} carried {carried:.7}, not the head {up:.7} its take read"
                )),
                _ if !w.is_ancestor(up, &w.head(*t)) => {
                    Err(format!("a sync into {t:?} lacks the parent's head {up:.7}"))
                }
                _ => Ok(0),
            }
        }
        _ => Ok(w.step(s)),
    }
}

/// Rule 9's locks (unchanged by git's merge).
fn locks(w: &World) -> Result<(), String> {
    for (t, hold) in w.holds() {
        if !w.alive(*t) {
            return Err(format!("a take outlives {t:?}"));
        }
        if let Some(p) = w.write_open(*t).intersection(&hold.files).next() {
            return Err(format!(
                "{t:?}: a write-open of {p} succeeded while a take holds it"
            ));
        }
    }
    for h in w.handles() {
        let alive = match &h.client {
            Client::Main(n) => !w.ended_main.contains(n),
            Client::Fork(f) => w.forks[f].life == Life::Open,
        };
        let held = match h.target {
            Target::Txn(f, n) => !h.waiting && w.forks[&f].txn.as_ref().is_some_and(|x| x.id == n),
            Target::Tree(t) => {
                (!h.waiting && h.kind == Kind::Write)
                    || w.holds()
                        .get(&t)
                        .is_some_and(|x| h.files().iter().any(|f| x.files.contains(f)))
            }
        };
        if !alive {
            return Err(format!("h{} outlives its client {:?}", h.id, h.client));
        }
        if !held {
            return Err(format!(
                "h{} ({:?} {:?}:{:?}) waits with no take holding its file, or outlives its transaction",
                h.id, h.kind, h.target, h.file
            ));
        }
    }
    for (t, files) in w.waits() {
        let open = w.write_open(*t);
        if !w.alive(*t) || !files.iter().any(|p| open.contains(p)) {
            return Err(format!(
                "{t:?}'s queue waits for {files:?}, none open for writing: no close woke it"
            ));
        }
    }
    Ok(())
}

/// Once writes stop and every queue has drained: nothing pending but
/// drops, no lock left, every loser pushed, every live fork equal to main.
pub fn check_converged(w: &mut World) -> Result<(), String> {
    if let Some(s) = w
        .pending()
        .iter()
        .find(|s| matches!(s, Step::Next(_) | Step::Finish(_) | Step::Replicate))
    {
        return Err(format!("not drained: {s:?} still pending"));
    }
    if let Some(h) = w.handles().next() {
        return Err(format!("h{} still open at the end", h.id));
    }
    if !w.waits().is_empty() || !w.holds().is_empty() {
        return Err("a lock outlives the run".into());
    }
    losers_pushed(w)?;
    originals_kept(w, false)?;
    let main = w.files(&w.main_head);
    for t in w.live_trees() {
        let mine = w.files(&w.head(t));
        if mine != main {
            return Err(format!(
                "{t:?} never converged: {mine:?} vs main's {main:?}"
            ));
        }
    }
    Ok(())
}

type Views = BTreeMap<Tree, BTreeSet<String>>;

/// The tracked versions every live tree shows now.
fn views(w: &World) -> Views {
    w.live_trees()
        .into_iter()
        .map(|t| (t, w.shown(&w.head(t))))
        .collect()
}

/// Every tracked line's bytes in a commit, any file.
fn bytes(w: &World, at: &str) -> BTreeSet<String> {
    w.files(at)
        .into_values()
        .flatten()
        .filter(|l| tracked(l))
        .collect()
}

/// The delete written over version `v` that `at` reaches, if any.
fn delete_of(w: &World, at: &str, v: &str) -> Option<String> {
    w.oracle
        .deletes
        .get(v)?
        .iter()
        .find(|d| w.reached(at, &w.oracle.versions[*d].commit))
        .cloned()
}

/// Reads each new collision's hunk as versions: lost, the incoming
/// change's new lines and deletes in it that the result lacks; won, the
/// parent's new lines and deletes in it.
fn read_collisions(w: &mut World) {
    while w.collision_versions.len() < w.collisions.len() {
        let c = w.collisions[w.collision_versions.len()].clone();
        let (b, o, t, r) = (
            bytes(w, &c.base),
            bytes(w, &c.winner),
            bytes(w, &c.loser),
            bytes(w, &c.result),
        );
        let mut lost = Vec::new();
        let mut won = Vec::new();
        for x in c.theirs.iter().filter(|x| tracked(x)) {
            // Not in the result, and not deleted by the parent: lost, new
            // or carried (a renamed file's lines).
            let fresh = w.version_in(&c.loser, x) != w.version_in(&c.base, x);
            if !r.contains(x) && (fresh || !(b.contains(x) && !o.contains(x))) {
                lost.extend(w.version_in(&c.loser, x));
            }
            if b.contains(x)
                && !o.contains(x)
                && let Some(v) = w.version_in(&c.base, x)
            {
                won.extend(delete_of(w, &c.winner, &v));
            }
        }
        for x in c.ours.iter().filter(|x| tracked(x)) {
            if !b.contains(x) {
                won.extend(w.version_in(&c.winner, x));
            } else if !t.contains(x)
                && let Some(v) = w.version_in(&c.base, x)
            {
                lost.extend(delete_of(w, &c.loser, &v));
            }
        }
        w.collision_versions.push((lost, won));
    }
}

/// A collision in `t` or upstream names a version descending from `v` lost.
fn flagged(w: &World, t: Tree, v: &str) -> bool {
    let trees = w.upstream(t);
    w.collisions
        .iter()
        .zip(&w.collision_versions)
        .any(|(c, (lost, _))| {
            trees.contains(&c.tree) && lost.iter().any(|x| w.oracle.descends(x, v))
        })
}

/// [`flagged`] by a real collision: the lost version and some winner in
/// its hunk were each written without seeing the other (or the hunk's
/// winners are untracked bytes).
fn decided_against(w: &World, t: Tree, v: &str) -> bool {
    let trees = w.upstream(t);
    w.collisions
        .iter()
        .zip(&w.collision_versions)
        .any(|(c, (lost, won))| {
            trees.contains(&c.tree)
                && lost.iter().any(|x| {
                    w.oracle.descends(x, v) && won.iter().any(|y| w.oracle.concurrent(x, y))
                })
        })
}

/// Whether tree `t` (at head `at`, showing `shown`) accounts for version
/// `v`, which it showed once: it shows it, or a version written over it;
/// a delete written over it (or over one written over it) has reached it;
/// or a collision in `t` or upstream names it (or one over it) lost.
fn accounted(w: &World, t: Tree, at: &str, shown: &BTreeSet<String>, v: &str) -> bool {
    shown.contains(v)
        || shown.iter().any(|u| w.oracle.descends(u, v))
        || w.oracle
            .over(v)
            .any(|d| w.oracle.is_delete(d) && w.reached(at, &w.oracle.versions[d].commit))
        || flagged(w, t, v)
        || follows(w, t, v)
}

/// A fork shows what its parent showed at its last sync: a version the
/// parent did not show then is accounted for in the fork as it is in the
/// parent (a nested fork follows its parent's pending drops).
fn follows(w: &World, t: Tree, v: &str) -> bool {
    let Tree::Fork(f) = t else { return false };
    let fork = &w.forks[&f];
    let (p, at) = (fork.parent, w.head(fork.parent));
    !w.shown(&fork.base).contains(v) && w.alive(p) && accounted(w, p, &at, &w.shown(&at), v)
}

/// No tree silently loses a change, and no stale version reverts a newer
/// one. (a) Every version a live tree ever showed is accounted for there
/// ([`accounted`]). (b) A version that leaves a tree is never replaced by
/// one it was written over, unless a real collision decided against it.
fn no_silent_loss(w: &World, now: &Views) -> Result<(), String> {
    for (t, shown) in now {
        let at = w.head(*t);
        for v in w.ever.get(t).into_iter().flatten() {
            if !accounted(w, *t, &at, shown, v) {
                return Err(format!(
                    "silent loss in {t:?}: {v} is gone, with no version over it, no delete of it and no collision naming it"
                ));
            }
        }
        let Some(before) = w.views.get(t) else {
            continue;
        };
        for v in before.difference(shown) {
            if let Some(u) = shown
                .difference(before)
                .find(|u| *u != v && w.oracle.descends(v, u))
                && !decided_against(w, *t, v)
            {
                return Err(format!(
                    "stale revert in {t:?}: {v} became {u}, which it was written over"
                ));
            }
        }
    }
    Ok(())
}

/// No flip-flop: a version, once gone from a tree, never shows there again,
/// unless what replaced it there (a version or delete over it) lost to a
/// real collision.
fn no_flicker(w: &mut World, now: &Views) -> Result<(), String> {
    let mut left = Vec::new();
    for (t, shown) in now {
        let Some(before) = w.views.get(t) else {
            continue;
        };
        for v in shown.difference(before) {
            if w.left.contains(&(*t, v.clone())) {
                let excused = w.oracle.over(v).any(|r| {
                    (w.ever.get(t).is_some_and(|e| e.contains(r)) || w.oracle.is_delete(r))
                        && decided_against(w, *t, r)
                });
                if !excused {
                    return Err(format!("flicker in {t:?}: {v} shows again after it left"));
                }
            }
        }
        left.extend(before.difference(shown).map(|v| (*t, v.clone())));
    }
    w.left.extend(left);
    Ok(())
}

/// The parent's side wins every collision, decided only at the parent's
/// queue: the result keeps the parent's lines of the hunk (in order), each
/// winning version, and none of the lost ones; the hunk is real (some lost
/// version and some winner were written without seeing each other), and
/// never the loss of a version written over every winner; the losing
/// commit stays reachable under the collision's ref; the result's parents
/// fit the history setting.
fn parent_wins(w: &mut World) -> Result<(), String> {
    for (i, c) in w.collisions.iter().enumerate().skip(w.collisions_checked) {
        if !decided_at_the_parents_queue(w, c) {
            return Err(format!("{c:?}: not decided at the parent's queue"));
        }
        let (lost, won) = &w.collision_versions[i];
        let result = w.file(&c.result, &c.path).unwrap_or_default();
        let (r, before) = (w.shown(&c.result), w.shown(&c.winner));
        // In a squashed run a later commit of the run may write over a
        // winner: it survives through a version over it.
        let survives = |y: &str| r.iter().any(|u| w.oracle.descends(u, y));
        let replaced = c.ours.iter().any(|l| {
            w.version_in(&c.winner, l)
                .is_some_and(|y| !r.contains(&y) && survives(&y))
        });
        // A later commit of a squashed run may have moved the file.
        let files = if result.is_empty() {
            w.files(&c.result).into_values().collect()
        } else {
            vec![result]
        };
        let kept = replaced
            || files.iter().any(|f| {
                if w.policies.collision == CollisionScope::File || c.by_file {
                    c.ours.iter().all(|l| f.contains(l))
                } else {
                    subsequence(&c.ours, f)
                }
            });
        let wins = won
            .iter()
            .all(|y| w.oracle.is_delete(y) || (survives(y) && before.contains(y)));
        let loses = lost.iter().all(|x| w.oracle.is_delete(x) || !r.contains(x));
        if !kept || !wins || !loses {
            return Err(format!(
                "{c:?}: the parent's side did not win ({lost:?} vs {won:?})"
            ));
        }
        if !lost.is_empty() && !won.is_empty() {
            let real = lost
                .iter()
                .any(|x| won.iter().any(|y| w.oracle.concurrent(x, y)));
            if !real {
                if let Some(x) = lost
                    .iter()
                    .find(|x| won.iter().all(|y| w.oracle.descends(x, y)))
                {
                    return Err(format!(
                        "{c:?}: {x} was written over every winner {won:?}, and lost"
                    ));
                }
                return Err(format!(
                    "{c:?}: no lost version {lost:?} was written without seeing a winner {won:?}: not a real collision"
                ));
            }
        }
        let at = git(w.repo(), &["rev-parse", &c.reference]);
        if at != c.loser {
            return Err(format!("{c:?}: its ref names {at}"));
        }
        let (stored, lineage) = (
            commit_parents(w.repo(), &c.result, true),
            commit_parents(w.repo(), &c.result, false),
        );
        let both = [c.winner.clone(), c.loser.clone()];
        let ok = match (c.direction, w.policies.history) {
            (Direction::Landing, HistoryPolicy::Squashed) => {
                stored == [c.winner.clone()] && lineage.len() >= 2 && lineage[0] == c.winner
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

fn subsequence(a: &File, b: &File) -> bool {
    let mut it = b.iter();
    a.iter().all(|x| it.any(|y| y == x))
}

/// Every losing change is replicated and stays: read on the shared remote
/// alone, each collision's ref names its losing commit; each lost line's
/// bytes are in it and were written by a commit naming that version's
/// writer; a collision decided in main has its result in the remote main.
fn losers_pushed(w: &World) -> Result<(), String> {
    let remote = w.remote();
    for (c, (lost, _)) in w.collisions.iter().zip(&w.collision_versions) {
        let at = try_git(remote, &["rev-parse", "--verify", "-q", &c.reference], None);
        if at.stdout.trim() != c.loser {
            return Err(format!(
                "{c:?}: the remote's {} is {:?}",
                c.reference,
                at.stdout.trim()
            ));
        }
        for v in lost.iter().filter(|v| !w.oracle.is_delete(v)) {
            let b = w.oracle.versions[v].bytes.clone().expect("a live version");
            let holds = |x: &str| {
                git(remote, &["--no-replace-objects", "ls-tree", x])
                    .lines()
                    .filter_map(|l| l.split(' ').nth(2).and_then(|r| r.split('\t').next()))
                    .any(|o| {
                        git(remote, &["cat-file", "blob", o])
                            .lines()
                            .any(|l| l == b)
                    })
            };
            if !holds(&c.loser) {
                return Err(format!(
                    "{c:?}: on the remote, lost {v} is not in the loser"
                ));
            }
            // A rewrite's or a move's version holds bytes another writer wrote
            // first: its own commit names its writer. Otherwise the first
            // commit back from the loser that holds the bytes.
            let mut x = c.loser.clone();
            if w.oracle.by_bytes.get(&b).is_some_and(|vs| vs.len() > 1) {
                x = w.oracle.versions[v].commit.clone();
                let o = w.originals.iter().find(|(_, o)| **o == x).map(|(c, _)| c);
                let reach = |y: &str| {
                    try_git(
                        remote,
                        &[
                            "--no-replace-objects",
                            "merge-base",
                            "--is-ancestor",
                            y,
                            &c.loser,
                        ],
                        None,
                    )
                    .code
                        == 0
                };
                if !reach(&x) {
                    x = o.filter(|y| reach(y)).cloned().unwrap_or(x);
                }
            } else {
                while let Some(q) = commit_parents(remote, &x, true)
                    .into_iter()
                    .find(|q| holds(q))
                {
                    x = q;
                }
            }
            let by = try_git(
                remote,
                &[
                    "--no-replace-objects",
                    "log",
                    "-1",
                    "--format=%(trailers:key=Agentfs-Writer,valueonly)",
                    &x,
                ],
                None,
            )
            .stdout
            .trim()
            .to_string();
            if by != writer_of(v) {
                return Err(format!(
                    "{c:?}: on the remote, lost {v} was written by a commit by {by}"
                ));
            }
        }
        if c.tree == Tree::Main
            && try_git(
                remote,
                &[
                    "--no-replace-objects",
                    "merge-base",
                    "--is-ancestor",
                    &c.result,
                    "refs/heads/main",
                ],
                None,
            )
            .code
                != 0
        {
            return Err(format!(
                "{c:?}: its result is not in the remote main's history"
            ));
        }
    }
    Ok(())
}

/// Who changed what, when, survives a sync's rebase: every rebased copy
/// names its original in `Agentfs-Rebased` and keeps its writer, and the
/// original stays reachable from a pre-rebase ref (`refs/rebased/<n>`) in
/// the local repository after each step (each copy once), and on the
/// remote, which Replication pushed, at the end.
fn originals_kept(w: &mut World, local: bool) -> Result<(), String> {
    let repo = if local { w.repo() } else { w.remote() }.to_path_buf();
    let repo = repo.as_path();
    let refs = git(
        repo,
        &["for-each-ref", "--format=%(objectname)", "refs/rebased/"],
    );
    let heads: Vec<&str> = refs.lines().collect();
    for (copy, orig) in &w.originals {
        if local && w.originals_checked.contains(copy) {
            continue;
        }
        let named = w.trailer(copy, "Agentfs-Rebased");
        if named != *orig {
            return Err(format!(
                "rebased copy {copy:.7} names {named:?}, not its original {orig:.7}"
            ));
        }
        if w.writer_of_commit(copy) != w.writer_of_commit(orig) {
            return Err(format!(
                "rebased copy {copy:.7} changed its original's writer"
            ));
        }
        let kept = heads.iter().any(|h| {
            try_git(
                repo,
                &[
                    "--no-replace-objects",
                    "merge-base",
                    "--is-ancestor",
                    orig,
                    h,
                ],
                None,
            )
            .code
                == 0
        });
        if !kept {
            return Err(format!(
                "rebased commit {orig:.7} is reachable from no refs/rebased/* in {}",
                repo.display()
            ));
        }
    }
    if local {
        let all: Vec<String> = w.originals.keys().cloned().collect();
        w.originals_checked.extend(all);
    }
    Ok(())
}

fn decided_at_the_parents_queue(w: &World, c: &Collision) -> bool {
    let cause = w.trailer(&c.result, "Agentfs-Cause");
    if w.trailer(&c.result, "Agentfs-Tree") != World::name(c.tree) {
        return false;
    }
    let from = w.trailer(&c.loser, "Agentfs-Tree");
    match c.direction {
        Direction::Landing => {
            cause == "Landing"
                && from
                    .strip_prefix("fork/")
                    .and_then(|f| f.parse::<ForkId>().ok())
                    .is_some_and(|f| w.upstream(Tree::Fork(f))[1..].contains(&c.tree))
        }
        Direction::Transaction => {
            cause.starts_with("Transaction ")
                && from
                    .strip_prefix("txn/")
                    .and_then(|n| n.parse::<TxnId>().ok())
                    .is_some_and(|n| {
                        w.ended
                            .iter()
                            .any(|e| e.txn == n && Tree::Fork(e.fork) == c.tree)
                    })
        }
    }
}

/// Every sync delivers the parent's decided state and decides nothing
/// (rule 3a): the fork then shows every version the parent shows, but
/// where its own undecided version (or delete) is over it; every version
/// it shows that the parent does not is its own and undecided (the
/// parent's head reaches neither its commit nor a copy of it); and every
/// own undecided version it showed before still shows, or one over it.
fn syncs_deliver(w: &mut World) -> Result<(), String> {
    for (t, up, before, after) in &w.syncs[w.syncs_checked..] {
        let (u, r) = (w.shown(up), w.shown(after));
        let mine = |v: &str| !w.reached(up, &w.oracle.versions[v].commit);
        if let Some(v) = w
            .shown(before)
            .difference(&r)
            .find(|v| mine(v) && !r.iter().any(|x| w.oracle.descends(x, v)))
        {
            return Err(format!(
                "a sync of {up:.7} into {t:?} dropped its own undecided {v}: only the parent's queue decides"
            ));
        }
        for v in u.difference(&r) {
            let over = w.oracle.over(v).any(|x| {
                mine(x)
                    && (r.contains(x)
                        || ((w.oracle.is_delete(x) || w.oracle.moves.contains(x))
                            && w.reached(after, &w.oracle.versions[x].commit)))
            });
            if !over {
                return Err(format!(
                    "a sync of {up:.7} into {t:?} dropped the parent's {v}"
                ));
            }
        }
        if let Some(v) = r.difference(&u).find(|v| !mine(v)) {
            return Err(format!(
                "a sync of {up:.7} into {t:?} kept {v}, which the parent already took"
            ));
        }
    }
    w.syncs_checked = w.syncs.len();
    Ok(())
}

/// Rule 5, as the program was told: a committed transaction's staged
/// versions are in the fork's commit (or one written over them is, or a
/// staged delete applied) or lost in one of that commit's collisions, and
/// the answer names each of those collisions; a rolled-back one's are
/// discarded; an empty one staged nothing.
fn transactions_end_as_told(w: &mut World) -> Result<(), String> {
    for e in &w.ended[w.ended_checked..] {
        let ok = match (&e.how, &e.result) {
            (TxnEnd::Commit(TxnOutcome::Committed { overwritten }), Some(r)) => {
                let mine: Vec<&(Vec<String>, Vec<String>)> = w
                    .collisions
                    .iter()
                    .zip(&w.collision_versions)
                    .filter(|(c, _)| c.result == *r)
                    .map(|(_, v)| v)
                    .collect();
                let shown = w.shown(r);
                let staged = e.staged.iter().all(|s| {
                    let applied = if w.oracle.is_delete(s) {
                        let gone = w.oracle.versions[s].over.clone().expect("a delete");
                        !shown.contains(&gone)
                    } else {
                        shown.iter().any(|u| w.oracle.descends(u, s))
                            || w.oracle
                                .over(s)
                                .any(|x| w.oracle.is_delete(x) && e.staged.iter().any(|y| y == x))
                    };
                    applied
                        || mine
                            .iter()
                            .any(|(lost, _)| lost.iter().any(|x| w.oracle.descends(x, s)))
                });
                staged && overwritten.len() == mine.len()
            }
            (TxnEnd::Commit(TxnOutcome::Empty), None) => e.staged.is_empty(),
            (TxnEnd::Rollback, None) => e.staged.iter().all(|s| w.discarded.contains(s)),
            _ => false,
        };
        if !ok {
            return Err(format!(
                "t{} on f{} ended {:?} but its staged versions {:?} are not where that says",
                e.txn, e.fork, e.how, e.staged
            ));
        }
    }
    w.ended_checked = w.ended.len();
    Ok(())
}

/// Rule 6: every commit names exactly one writer. A merge (a landing, a
/// transaction's commit, or a rebased copy of one) names its second
/// parent's writer and adds no bytes no parent holds. Any other commit
/// adds only versions of its own writer (a rebased copy keeps its
/// original's writer). A save's append lands after every line of its
/// parent's file.
fn one_writer(w: &mut World, all: &[String]) -> Result<(), String> {
    let new: Vec<String> = all
        .iter()
        .filter(|c| !w.checked_commits.contains(*c))
        .cloned()
        .collect();
    for c in &new {
        let writers = git(
            w.repo(),
            &[
                "--no-replace-objects",
                "log",
                "-1",
                "--format=%(trailers:key=Agentfs-Writer,valueonly,separator=%x1e)",
                c,
            ],
        );
        let writers: Vec<&str> = writers.split('\x1e').filter(|s| !s.is_empty()).collect();
        if writers.len() != 1 {
            return Err(format!("{c} names writers {writers:?}"));
        }
        let writer = writers[0];
        let parents = commit_parents(w.repo(), c, true);
        let Some(first) = parents.first() else {
            continue;
        };
        let (base, now) = (w.files(first), w.files(c));
        let had: BTreeSet<&String> = base.values().flatten().collect();
        if parents.len() > 1 {
            let expect = w.writer_of_commit(&parents[1]);
            if writer != expect {
                return Err(format!(
                    "{c} names {writer} but its second parent's writer is {expect}"
                ));
            }
            let theirs: BTreeSet<String> = parents[1..].iter().flat_map(|p| bytes(w, p)).collect();
            if let Some(b) = now
                .values()
                .flatten()
                .find(|b| tracked(b) && !had.contains(b) && !theirs.contains(*b))
            {
                return Err(format!("merge {c} adds {b}, which no parent holds"));
            }
            continue;
        }
        let save = w.trailer(c, "Agentfs-Cause") == "Direct";
        for (p, f) in &now {
            for (k, b) in f.iter().enumerate() {
                if !tracked(b) || had.contains(b) {
                    continue;
                }
                let v = w
                    .version_in(c, b)
                    .ok_or_else(|| format!("{c}: {b} is no version"))?;
                if writer_of(&v) != writer {
                    return Err(format!("{c} names {writer} but adds {v} in {p}"));
                }
                if save && w.appended.contains(b) {
                    let old = base.get(p).cloned().unwrap_or_default();
                    if old
                        .iter()
                        .any(|o| tracked(o) && f.contains(o) && !f[..k].contains(o))
                    {
                        return Err(format!("{c}: append {b} in {p} is not after the end"));
                    }
                }
            }
        }
    }
    w.checked_commits.extend(new);
    Ok(())
}

/// A rolled-back transaction leaves no line in any tree and no bytes in a
/// commit any ref reaches, but where a later rewrite of a read wrote those
/// bytes again (a new version of its own writer).
fn discarded_leave_no_trace(w: &mut World, all: &[String]) -> Result<(), String> {
    if w.discarded.is_empty() {
        return Ok(());
    }
    for t in w.live_trees() {
        if let Some(v) = w.shown(&w.head(t)).intersection(&w.discarded).next() {
            return Err(format!("discarded {v} shows in {t:?}"));
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
    let pats: Vec<String> = w
        .discarded
        .iter()
        .filter_map(|v| w.oracle.versions.get(v).and_then(|x| x.bytes.clone()))
        .filter(|b| w.oracle.by_bytes.get(b).is_some_and(|x| x.len() == 1))
        .collect();
    if new.is_empty() || pats.is_empty() {
        w.grepped.1.extend(new);
        return Ok(());
    }
    let mut args: Vec<String> = vec!["grep".into(), "-l".into(), "-E".into()];
    for b in &pats {
        args.extend(["-e".into(), format!("^{}$", b.replace('.', "\\."))]);
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
/// main's holding only main's own commits.
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

fn reach(repo: &Path, head: &str, lineage: bool) -> BTreeSet<String> {
    let mut args = vec!["rev-list", head];
    if !lineage {
        args.insert(0, "--no-replace-objects");
    }
    git(repo, &args).lines().map(str::to_string).collect()
}
