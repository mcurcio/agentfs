//! The invariants (MODEL.md, "What must always hold"), read back from the
//! repository and the trees after every step. Each returns the first
//! violation found. Reads over git objects are cached in the [`World`]:
//! commits, their ancestry and blobs never change.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::content::{Content, Line, PATHS, State, is_root, writer_of};
use crate::git::{git, try_git};
use crate::world::{
    By, Client, Collision, Decision, Direction, Entry, ForkId, Kind, Landed, Life, Step, Take,
    Target, Tree, TxnEnd, TxnId, TxnOutcome, World, commit_parents, decisions_in, name_key,
    origin_in,
};

/// `r`'s error named for `check`: planted/matrix.tsv records the first check tripped.
fn named<T>(check: &'static str, r: Result<T, String>) -> Result<T, String> {
    r.map_err(|e| format!("{check}: {e}"))
}

/// Every invariant that holds after any step.
pub fn check(w: &mut World) -> Result<(), String> {
    let now = views(w);
    named("no_silent_loss", no_silent_loss(w, &now))?;
    named("no_flicker", no_flicker(w, &now))?;
    w.views = now;
    named("locks", locks(w))?;
    named("upstream_wins", upstream_wins(w))?;
    named("changes_whole", changes_whole(w))?;
    named("syncs_deliver", syncs_deliver(w))?;
    named("transactions_end_as_told", transactions_end_as_told(w))?;
    named("transactions_undisturbed", transactions_undisturbed(w))?;
    let discards = w.grepped.0 != w.discarded.len();
    if w.refs_checked != Some(w.ref_moves) || discards {
        let all = w.all_commits();
        named("one_writer", one_writer(w, &all))?;
        named(
            "discarded_leave_no_trace",
            discarded_leave_no_trace(w, &all),
        )?;
        w.refs_checked = Some(w.ref_moves);
    }
    named("rename_overs_whole", rename_overs_whole(w))?;
    named("full_lineage", full_lineage(w))?;
    named("placed_and_unlinked", placed_and_unlinked(w))?;
    Ok(())
}

/// One step, with what only the step itself shows. A take (rule 9) is one
/// step (the owner, 2026-11-08, finding 49: "B"): it either waits, taking
/// nothing, or takes the first entry P1 lets go and writes it. A sync's take
/// reads the upstream's head as it is then (read here before the step, not
/// from the model's record). A landing locks every file its change touches,
/// read from git, a losing change's included (R9.1). A take writes no file a
/// client of that tree has open for writing, nor any file it did not lock; a
/// sync records no collision (only an upstream's queue decides one) and
/// carries the head its take read. Returns the queue entries taken.
pub fn step_checked(w: &mut World, s: &Step) -> Result<usize, String> {
    named("step_checked", step_rules(w, s))
}

fn step_rules(w: &mut World, s: &Step) -> Result<usize, String> {
    let Step::Next(t) = s else {
        return Ok(w.step(s));
    };
    let up = match t {
        Tree::Fork(f) => Some(w.head(w.forks[f].upstream)),
        Tree::Main => None,
    };
    // P1, read before the step: the entry it must act on is the first one no
    // earlier waiting entry shares a file or a source with (the files read
    // from git, not from the model's plan).
    let q = w.queue_numbered(*t);
    let Some(i) = p1_first(w, *t) else {
        return Err(format!("{t:?}: a queue step with no entry free to go"));
    };
    let n = q[i].0;
    let was = w.waits().get(t).is_some_and(|m| m.contains_key(&n));
    let ours = w.head(*t);
    let open = w.write_open(*t);
    let (collisions, syncs) = (w.collisions.len(), w.syncs.len());
    let taken = w.step(s);
    let up = up.filter(|_| q[i].1 == Entry::Sync);
    let waits = w.waits().get(t).is_some_and(|m| m.contains_key(&n)) && !was;
    let Some((_, take)) = w.last_take().filter(|(x, _)| x == t).cloned() else {
        // A landing, or a sync whose first commit would overwrite an open
        // file, waits and takes nothing.
        return match (waits, up) {
            (true, None) => Ok(0),
            (true, Some(head)) => {
                prefix_cut(w, *t, &ours, &head, &open, None)?;
                Ok(0)
            }
            _ => Err(format!("{t:?}'s entry {n} neither waits nor is taken")),
        };
    };
    // It took entry n, or (P2) a sync delivered a prefix and waits for the rest.
    match take.seq {
        Some(m) if m == n && taken == 1 && !waits => {}
        None if taken == 0 && waits && up.is_some() => {}
        x => {
            return Err(format!(
                "{t:?} took {x:?} ({taken} entries), not entry {n}, the first P1 lets go"
            ));
        }
    }
    // Rule 9, read from git: a landing locks every file its change touches, a
    // losing change's included (not only what its plan writes).
    if let Take::Landing { commit: c, .. } = &take.take
        && let Some(p) = change_files(w, c).difference(&take.files).next()
    {
        return Err(format!(
            "{t:?}: a landing locks {:?}, not {p}, which its change {c:.7} touches",
            take.files
        ));
    }
    let after = w.head(*t);
    let wrote: Vec<String> = if ours == after {
        Vec::new()
    } else {
        diff_files(w, &ours, &after).into_iter().collect()
    };
    if let Some(p) = wrote.iter().find(|p| open.contains(*p)) {
        return Err(format!(
            "a take into {t:?} wrote {p}, which a client there has open for writing"
        ));
    }
    if let Some(p) = wrote.iter().find(|p| !take.files.contains(*p)) {
        return Err(format!(
            "a take into {t:?} wrote {p}, which it did not lock"
        ));
    }
    let Take::Sync {
        up: to, head: read, ..
    } = &take.take
    else {
        return Ok(taken);
    };
    let Some(head) = up else {
        return Err(format!("{t:?}'s entry {n}, a landing, was taken as a sync"));
    };
    if *read != head {
        return Err(format!(
            "a sync into {t:?} read {read:.7}, not the upstream's head {head:.7}"
        ));
    }
    prefix_cut(w, *t, &ours, &head, &open, Some(to))?;
    if w.collisions.len() != collisions {
        return Err(format!(
            "a sync into {t:?} recorded a collision: only the upstream's queue decides one"
        ));
    }
    match w.syncs.get(syncs) {
        Some((_, carried, _, _)) if carried != to => Err(format!(
            "a sync into {t:?} carried {carried:.7}, not the head {to:.7} its take read"
        )),
        None if !w.is_ancestor(to, &after) => Err(format!(
            "a sync into {t:?} did nothing, but it lacks the upstream's head {to:.7}"
        )),
        _ => Ok(taken),
    }
}

/// P2, the prefix sync, read from git: a sync into fork `t` (at `ours`,
/// with `open` open for writing there) of the upstream's head `head`
/// delivers the upstream's first parents in order up to `to` (none: it
/// delivers nothing and waits), and if it holds any back, the first it
/// holds back touches a file open there: its own change (against its
/// first parent) or a collision record in it names that file, and its
/// version differs from the fork's (a necessary condition: the fork may
/// keep its own undecided lines over it). That it never delivers past
/// that commit is the take's write: it overwrites no open file
/// (`step_checked`), so no change to an open file is delivered.
fn prefix_cut(
    w: &World,
    t: Tree,
    ours: &str,
    head: &str,
    open: &BTreeSet<String>,
    to: Option<&String>,
) -> Result<(), String> {
    let mut chain = Vec::new();
    let mut c = head.to_string();
    while !w.is_ancestor(&c, ours) {
        chain.push(c.clone());
        c = git(w.repo(), &["rev-parse", &format!("{c}^1")]);
    }
    chain.reverse();
    let touches = |c: &str| -> Option<String> {
        let mut files = diff_files(w, &format!("{c}^1"), c);
        files.extend(
            decisions_in(w.repo(), &["-1", c])
                .into_iter()
                .map(|(_, x)| x.path),
        );
        files.into_iter().find(|f| {
            let (now, was) = (w.file(c, f), w.file(ours, f));
            now != was
                && (open.contains(f)
                    || [name_key(&now), name_key(&was)]
                        .iter()
                        .flatten()
                        .any(|k| open.contains(k)))
        })
    };
    let delivered = match to {
        None => 0,
        // It has them all already: nothing to deliver.
        Some(to) if chain.is_empty() && w.is_ancestor(to, ours) => 0,
        Some(to) => {
            1 + chain.iter().position(|c| c == to).ok_or(format!(
                "a sync into {t:?} delivers {to:.7}, not one of the upstream's commits it lacks"
            ))?
        }
    };
    match chain.get(delivered) {
        Some(c) if touches(c).is_none() => Err(format!(
            "a sync into {t:?} holds back {c:.7}, which touches no file open for writing there"
        )),
        _ => Ok(()),
    }
}

/// Rule 9's locks, after every step and call. A take is one step, so no
/// take holds a file between steps and calls (what it locks is checked in
/// `step_checked`). No lock outlives its holder: every open file's client
/// is alive (and its transaction open), every wait's tree alive, and no
/// call is left that neither ran nor was deferred (R5.3). Every lock wait
/// ends on an event: a waiting queue head waits only while one of its files
/// is still open for writing there (the last close wakes it). No timer is
/// modelled.
fn locks(w: &World) -> Result<(), String> {
    let queued = |t: Tree, n: u64| w.queue_numbered(t).iter().any(|(m, _)| *m == n);
    for h in w.handles() {
        let alive = match &h.client {
            Client::Main(n) => !w.ended_main.contains(n),
            Client::Fork(f) | Client::Outside(f) => w.forks[f].life == Life::Open,
        };
        let held = match h.target {
            Target::Txn(f, n) => w.forks[&f].txn.as_ref().is_some_and(|x| x.id == n),
            // R5.3: a deferred call lives only behind its open transaction.
            Target::Tree(Tree::Fork(f)) if h.deferred => {
                w.forks[&f].txn.is_some() && w.forks[&f].deferred.contains(&h.id)
            }
            Target::Tree(_) => h.kind == Kind::Write,
        };
        if !alive {
            return Err(format!("h{} outlives its client {:?}", h.id, h.client));
        }
        if !held {
            // No program's call ever waits (R9.2, and a take is one step).
            return Err(format!(
                "h{} ({:?} {:?}:{}) waits, or outlives its transaction",
                h.id, h.kind, h.target, h.file
            ));
        }
    }
    for (t, m) in w.waits() {
        let open = w.write_open(*t);
        for (n, x) in m {
            // The same for a waiting landing: it locks every file its
            // change touches, read from git.
            let q = w.queue_numbered(*t);
            if let Some((_, e @ Entry::Landing { .. })) = q.iter().find(|(m, _)| m == n)
                && let Some(p) = entry_files(w, *t, e).difference(&x.files).next()
            {
                return Err(format!(
                    "{t:?}'s waiting entry {n} locks {:?}, not {p}, which its change touches",
                    x.files
                ));
            }
            if !w.alive(*t) || !queued(*t, *n) || x.until.is_disjoint(&open) {
                return Err(format!(
                    "{t:?}'s entry {n} waits for {:?}, none open for writing: no close woke it",
                    x.until
                ));
            }
            if let Some(p) = x.until.difference(&x.files).next() {
                return Err(format!(
                    "{t:?}'s entry {n} waits for {p}, which it does not lock"
                ));
            }
        }
        // P1: an entry no waiting one shares a file or a source with goes.
        if p1_first(w, *t).is_some() && !w.pending().contains(&Step::Next(*t)) {
            return Err(format!("{t:?}: an entry free to go is not offered"));
        }
    }
    Ok(())
}

/// P1 (git-model.md §6), the checks' own reading: the first entry of `t`'s
/// queue that no earlier waiting entry (or one waiting behind it) shares a
/// file or a source with. Files are read from git ([`entry_files`]); a
/// waiting entry's, the files it locks.
pub fn p1_first(w: &World, t: Tree) -> Option<usize> {
    let waiting = w.waits().get(&t);
    let mut files: BTreeSet<String> = BTreeSet::new();
    let mut sources = BTreeSet::new();
    for (i, (n, e)) in w.queue_numbered(t).iter().enumerate() {
        if let Some(x) = waiting.and_then(|m| m.get(n)) {
            files.extend(x.files.iter().cloned());
            sources.insert(e.source());
            continue;
        }
        let mine = entry_files(w, t, e);
        if sources.contains(&e.source()) || !mine.is_disjoint(&files) {
            files.extend(mine);
            sources.insert(e.source());
            continue;
        }
        return Some(i);
    }
    None
}

/// The files queue entry `e` of `t` may touch, read from git: a landing's
/// commit's own changes ([`change_files`]), a sync's every file its tree
/// and its upstream's head differ in.
pub fn entry_files(w: &World, t: Tree, e: &Entry) -> BTreeSet<String> {
    match (e, t) {
        (Entry::Landing { commit, .. }, _) => change_files(w, commit),
        (Entry::Sync, Tree::Fork(f)) => {
            let (ours, up) = (w.head(t), w.head(w.forks[&f].upstream));
            lock_keys(w, &ours, &up)
        }
        (Entry::Sync, Tree::Main) => BTreeSet::new(),
    }
}

/// What a fork commit's change locks, read from git, against its first parent.
pub fn change_files(w: &World, commit: &str) -> BTreeSet<String> {
    lock_keys(w, &format!("{commit}^1"), commit)
}

/// The files that differ from `a` to `b`, and each one's name on each side (R9.1).
fn lock_keys(w: &World, a: &str, b: &str) -> BTreeSet<String> {
    let files = diff_files(w, a, b);
    let names: Vec<String> = files
        .iter()
        .flat_map(|f| [name_key(&w.file(a, f)), name_key(&w.file(b, f))])
        .flatten()
        .collect();
    files.into_iter().chain(names).collect()
}

fn diff_files(w: &World, a: &str, b: &str) -> BTreeSet<String> {
    git(w.repo(), &["diff-tree", "-r", "--name-only", a, b])
        .lines()
        .map(str::to_string)
        .collect()
}

/// Once writes stop and every queue has drained: no step is left but
/// drops, and every live fork's tree equals main's. Stronger than "each
/// fork's untouched files equal main and touched files equal main or the
/// fork's newer non-colliding edit": once the queues drain, a fork has no
/// newer edit left.
pub fn check_converged(w: &World) -> Result<(), String> {
    named("check_converged", drained(w))?;
    named("losers_pushed", losers_pushed(w))?;
    named("check_converged", trees_equal_main(w))
}

fn drained(w: &World) -> Result<(), String> {
    if let Some(s) = w
        .pending()
        .iter()
        .find(|s| matches!(s, Step::Next(_) | Step::Replicate))
    {
        return Err(format!("not drained: {s:?} still pending"));
    }
    if let Some(h) = w.handles().next() {
        return Err(format!("h{} still open at the end", h.id));
    }
    if !w.waits().is_empty() {
        return Err(format!("a lock outlives the run: waits {:?}", w.waits()));
    }
    Ok(())
}

fn trees_equal_main(w: &World) -> Result<(), String> {
    for t in w.live_trees() {
        for p in PATHS {
            let (mine, main) = (w.file(&w.head(t), p), w.file(&w.main_head, p));
            if mine != main {
                return Err(format!(
                    "{t:?}:{p} never converged: {mine:?} vs main's {main:?}"
                ));
            }
        }
    }
    Ok(())
}

type Views = BTreeMap<Tree, BTreeMap<String, Content>>;

/// What every live tree holds now, file by file (by identity), deleted
/// versions and unlinked files included.
fn views(w: &World) -> Views {
    w.live_trees()
        .into_iter()
        .map(|t| {
            let head = w.head(t);
            (
                t,
                PATHS
                    .iter()
                    .map(|p| (p.to_string(), w.file(&head, p)))
                    .collect(),
            )
        })
        .collect()
}

/// Each line whose version changed in a live tree since the last check:
/// (tree, file, line identity, before, now). A line identity never leaves
/// a tree: a delete is a version.
fn moves<'a>(w: &'a World, now: &'a Views) -> Vec<(Tree, &'a str, &'a str, &'a Line, &'a Line)> {
    let mut out = Vec::new();
    for (t, files) in now {
        let Some(before) = w.views.get(t) else {
            continue;
        };
        for (p, c) in files {
            for r in c.records() {
                if let Some(b) = before[p].member(&r.line)
                    && b.id != r.id
                {
                    out.push((*t, p.as_str(), r.line.as_str(), b, r));
                }
            }
        }
    }
    out
}

/// A collision in `trees` names `token`, or a later version written over
/// it, as lost on line `line` of file `path`.
fn lost_in(w: &World, trees: &[Tree], path: &str, line: &str, token: &str) -> bool {
    w.collisions
        .iter()
        .any(|c| trees.contains(&c.tree) && names_lost(w, c, path, line, token))
}

/// Collision `c` names `token`, or a later version written over it, as
/// lost on line `line` of file `path`.
fn names_lost(w: &World, c: &Collision, path: &str, line: &str, token: &str) -> bool {
    c.path == path
        && c.lines
            .iter()
            .zip(&c.lost)
            .any(|(l, x)| l == line && w.lineage.descends(x, token))
}

/// [`lost_in`] `t` or a tree upstream of it, by a decision that has
/// reached `t` ([`delivered`]): a loss decided upstream reaches `t` as a
/// plain update, never before the commit that carries it.
fn flagged(w: &World, t: Tree, path: &str, line: &str, token: &str) -> bool {
    let trees = w.upstream(t);
    w.collisions.iter().any(|c| {
        trees.contains(&c.tree) && delivered(w, t, c) && names_lost(w, c, path, line, token)
    })
}

/// R10.3: a delete of line `line` of `path` that was written over version
/// `token` (or is it) entered `t` or a tree upstream of it, and no
/// collision that reached `t` names it lost. Another delete of that line
/// then agrees with it: `token`'s content is gone by that delete first.
fn deleted_over(w: &World, t: Tree, path: &str, line: &str, token: &str) -> bool {
    let trees = w.upstream(t);
    w.entered.iter().any(|(u, d)| {
        trees.contains(u)
            && d.gone
            && d.path == path
            && d.line == line
            && w.lineage.descends(&d.token, token)
            && !flagged(w, t, path, line, &d.token)
    })
}

/// Neither version was written over the other.
fn concurrent(w: &World, a: &str, b: &str) -> bool {
    !w.lineage.descends(a, b) && !w.lineage.descends(b, a)
}

/// The change collision `c` names really collided in that take: a
/// same-line or file-rule record of it ([`By::Line`], [`By::File`]) has a
/// lost version and a winner that are concurrent.
fn really_collided(w: &World, c: &Collision) -> bool {
    w.collisions.iter().any(|d| {
        d.tree == c.tree
            && d.result == c.result
            && d.loser == c.loser
            && d.by != By::Whole
            && d.lost.iter().zip(&d.won).any(|(x, y)| concurrent(w, x, y))
    })
}

/// [`flagged`], by a real decision: a collision in which the lost version
/// and its winner really collided (neither was written over the other), or
/// a version of a change that lost whole because another of its versions
/// really collided (rule 3e, [`By::Whole`]). That decision may replace the
/// lost version with any version, the one it was written over included (an
/// unlink that lost to a line it never saw gives the file back; a line of a
/// losing save goes back to the upstream's). A version lost by no real
/// collision of its change is no such decision.
fn decided_against(w: &World, t: Tree, path: &str, line: &str, token: &str) -> bool {
    let trees = w.upstream(t);
    w.collisions.iter().any(|c| {
        trees.contains(&c.tree)
            && delivered(w, t, c)
            && c.path == path
            && c.lines
                .iter()
                .zip(c.lost.iter().zip(&c.won))
                .any(|(l, (x, won))| {
                    l == line
                        && w.lineage.descends(x, token)
                        && match c.by {
                            By::Whole => really_collided(w, c),
                            By::Line | By::File => concurrent(w, x, won),
                        }
                })
    })
}

/// Whether collision `c`, decided in `t` or a tree upstream of it, has
/// reached `t` (causal order, P2), read from git. The commit that carries
/// it to `t` is the upstream's: for a decision of the upstream, its result; for
/// one further up, the upstream's commit that carried the losing change up,
/// which the upstream's head reaches (even `t`'s own commit counts only once
/// the upstream's commit carrying it is delivered). It has reached `t` unless
/// the upstream's head reaches that commit and no upstream's commit a sync
/// delivered into `t` (the sync's second parent, or the commit it
/// fast-forwarded to) does, nor, for a result, `t`'s head. So a
/// prefix sync that holds back
/// the upstream's landing that carried a line upstream has not delivered
/// main's decision against that line, nor a held-back commit's own.
fn delivered(w: &World, t: Tree, c: &Collision) -> bool {
    let Some(upstream) = w.upstream(t).get(1).copied() else {
        return true;
    };
    if c.tree == t {
        return true;
    }
    let up = w.head(upstream);
    let synced = |x: &str| {
        w.syncs
            .iter()
            .any(|(s, at, _, _)| *s == t && w.is_ancestor(x, at))
    };
    let carrier = if c.tree == upstream {
        &c.result
    } else {
        &c.loser
    };
    !w.is_ancestor(carrier, &up)
        || synced(carrier)
        || synced(&c.result)
        || w.is_ancestor(&c.result, &w.head(t))
}

/// Main never silently loses a change, nor does any tree, and no stale
/// version reverts a newer one: (a) in every live tree a line moves from
/// version a to b only if b was written over a, or both are deletes (they
/// agree, R10.3), or b is a delete agreeing with one written over a
/// ([`deleted_over`]), or a recorded collision in that tree or one upstream names a
/// as lost; never if a was written over b (b is stale), unless a collision
/// decided against a ([`decided_against`]); (b) every version that
/// entered a tree (written there, carried in by a landing or a
/// transaction, won or lost, or taken from the upstream by a sync) is still
/// its line's version there, or one written over it is, or a delete is
/// that agrees with it or with a delete written over it
/// ([`deleted_over`]), or such a collision names it, or the tree's last sync
/// hid its line under R10.6 ([`hidden_by_r10_6`]).
fn no_silent_loss(w: &World, now: &Views) -> Result<(), String> {
    for (t, p, l, a, b) in moves(w, now) {
        // excuse: R10.3 two deletes of one line agree: the line stays deleted
        if !a.live() && !b.live() {
            continue;
        }
        // excuse: R3b a collision decided against a: the upstream's version may replace it
        if w.lineage.descends(&a.id, &b.id) && !decided_against(w, t, p, l, &a.id) {
            return Err(format!(
                "stale revert in {t:?}:{p}[{l}]: {} became {}, which it was written over",
                a.id, b.id
            ));
        }
        // excuse: R3b a collision names a lost: it stays in history, named
        // excuse: R10.3 b agrees with a delete written over a
        if !w.lineage.descends(&b.id, &a.id)
            && !flagged(w, t, p, l, &a.id)
            && !(!b.live() && deleted_over(w, t, p, l, &a.id))
        {
            return Err(format!(
                "silent loss in {t:?}:{p}[{l}]: {} became {}, which was not written over it, and no collision names it",
                a.id, b.id
            ));
        }
    }
    for (t, e) in &w.entered {
        let Some(files) = now.get(t) else {
            continue;
        };
        let lost = flagged(w, *t, &e.path, &e.line, &e.token);
        let Some(s) = files[&e.path].member(&e.line) else {
            // A new line that lost never entered the tree's file.
            // excuse: R3b a new line that lost never entered the tree's file
            if lost {
                continue;
            }
            // excuse: R10.6 the tree's last sync hid the upstream's unfinished last line in front of an undecided line
            if hidden_by_r10_6(w, *t, &e.path, &e.line, &e.token) {
                continue;
            }
            return Err(format!("line {} of {} left {t:?}", e.line, e.path));
        };
        // excuse: R3b a collision names it lost
        // excuse: R10.3 s agrees with a delete written over e (or e itself)
        let deleted = !s.live() && deleted_over(w, *t, &e.path, &e.line, &e.token);
        if !lost && !w.lineage.descends(&s.id, &e.token) && !deleted {
            return Err(format!(
                "{} entered {t:?}:{}[{}] and is gone ({} there), with no collision naming it",
                e.token, e.path, e.line, s.id
            ));
        }
    }
    Ok(())
}

/// No flip-flop: a version, once replaced in a tree, never shows there
/// again, unless a collision decided against the version replacing it now
/// ([`decided_against`]: then it is that decision). So a fork's line
/// changes at most once per collision (its own x, then the upstream's
/// winning m, never x again).
fn no_flicker(w: &mut World, now: &Views) -> Result<(), String> {
    let mut left = Vec::new();
    for (t, p, l, a, b) in moves(w, now) {
        // excuse: R3a the upstream's queue decided against a: the line takes that decision
        if w.left
            .contains(&(t, p.to_string(), l.to_string(), b.id.clone()))
            && !decided_against(w, t, p, l, &a.id)
        {
            return Err(format!(
                "flicker in {t:?}:{p}[{l}]: {} shows again after it was replaced (now over {})",
                b.id, a.id
            ));
        }
        left.push((t, p.to_string(), l.to_string(), a.id.clone()));
    }
    w.left.extend(left);
    Ok(())
}

/// Whether some file of commit `at` holds version `token`.
fn holds(w: &World, at: &str, token: &str) -> bool {
    PATHS.iter().any(|p| w.file(at, p).holds(token))
}

/// The upstream's version wins every collision, decided only at the
/// upstream's queue (a landing, or a transaction's commit into its fork:
/// [`Direction`] has no other case, and `sync` records none), and only a
/// real one: in each recorded collision the result holds none of the lost
/// versions and keeps the tree's own version of every lost line, as the
/// tree had it before, and holds each version named in `won`, as the tree
/// did before; the lost versions are the losing change's; each
/// lost and winning pair is concurrent (neither was written over the
/// other; for a loss decided by a file rule, at least one pair; for a line
/// lost only because its change lost whole, a real collision of that
/// change: [`really_collided`]); the losing change stays reachable under
/// the collision's ref; the result's parents are the tree's previous
/// head and the losing change. Where it was decided is read from
/// git, not the model's record: the result is the deciding tree's own
/// landing or transaction commit, and the losing commit was written below
/// that tree (a fork under it, or the transaction of that fork).
fn upstream_wins(w: &mut World) -> Result<(), String> {
    for c in &w.collisions[w.collisions_checked..] {
        if !decided_at_the_upstreams_queue(w, c) {
            return Err(format!(
                "{c:?}: not decided at the upstream's queue (result {} in {}, loser from {})",
                w.trailer(&c.result, "Agentfs-Cause"),
                w.trailer(&c.result, "Agentfs-Tree"),
                w.trailer(&c.loser, "Agentfs-Tree"),
            ));
        }
        let (r, win, lose) = (
            w.file(&c.result, &c.path),
            w.file(&c.winner, &c.path),
            w.file(&c.loser, &c.path),
        );
        let named = c.lines.len() == c.won.len()
            && c.lines.len() == c.lost.len()
            && c.won
                .iter()
                .all(|t| holds(w, &c.winner, t) && holds(w, &c.result, t));
        let kept = c
            .lines
            .iter()
            .zip(&c.lost)
            .all(|(l, x)| !r.holds(x) && r.member(l) == win.member(l));
        if !kept || !named || c.lines.is_empty() {
            return Err(format!("{c:?}: the upstream's version did not win"));
        }
        let mut pairs = c.lost.iter().zip(&c.won);
        let real = match c.by {
            By::Line => pairs.all(|(x, y)| concurrent(w, x, y)),
            // excuse: R11 a loss by a file rule needs one concurrent pair, not all
            By::File => pairs.any(|(x, y)| concurrent(w, x, y)),
            // excuse: R3e a line lost with its change needs a real collision of that change
            By::Whole => really_collided(w, c),
        };
        if !real {
            return Err(format!(
                "{c:?}: not a collision: one version was written over the other"
            ));
        }
        if let Some(t) = c.lost.iter().find(|t| !lose.holds(t)) {
            return Err(format!("{c:?}: lost {t} is not in the incoming commit"));
        }
        let at = git(w.repo(), &["rev-parse", &c.reference]);
        if at != c.loser {
            return Err(format!("{c:?}: its ref names {at}"));
        }
        // Recorded in git, in its result commit, which names the tree the
        // decision belongs to; the losing commit holds no decision of its own.
        let tree = w.trailer(&c.result, "Agentfs-Tree");
        if !decisions_in(w.repo(), &["-1", &c.result])
            .iter()
            .any(|(_, d)| *d == Decision::of(c) && d.tree == tree)
        {
            return Err(format!(
                "{c:?}: its result commit {:.7} in {tree} does not record it",
                c.result
            ));
        }
        let parents = commit_parents(w.repo(), &c.result);
        if parents != [c.winner.clone(), c.loser.clone()] {
            return Err(format!("{c:?}: result's parents are {parents:?}"));
        }
    }
    w.collisions_checked = w.collisions.len();
    Ok(())
}

/// Rule 3e, read from git: no partial save or transaction ever reaches an
/// upstream. The change a take judged (a save, a transaction's commit, a
/// landing commit, or a transaction's branch at its commit) either lost,
/// named by a collision of that take, or won, never both; the versions its
/// own edit set (its diff from its base, read from git) are, for one that
/// lost, none in the take's result (unless the tree held it before) and
/// each named lost by that change's collisions (unless the tree before
/// already held it, or a version written over it); for one that won,
/// every one in the result, or a version written over it.
fn changes_whole(w: &mut World) -> Result<(), String> {
    for l in &w.landings[w.landings_checked..] {
        let u = &l.change;
        let lost = w
            .collisions
            .iter()
            .any(|c| c.tree == l.tree && c.result == l.result && c.loser == *u);
        if lost == l.won {
            return Err(format!(
                "{u:.7} into {:?}: judged lost {lost}, among the winners {}",
                l.tree, !lost
            ));
        }
        for e in w.edits(&l.base, u) {
            let (r, b) = (w.file(&l.result, &e.path), w.file(&l.before, &e.path));
            let ends = |v: &Line| w.lineage.descends(&v.id, &e.token);
            if !lost {
                if !r.member(&e.line).is_some_and(ends) {
                    return Err(format!(
                        "{u:.7} won in {:?}, but its {} of {}[{}] is not in {:.7}: a partial change",
                        l.tree, e.token, e.path, e.line, l.result
                    ));
                }
                continue;
            }
            // excuse: R3e.2 a version the tree held before costs the change nothing
            if r.holds(&e.token) && !b.holds(&e.token) {
                return Err(format!(
                    "{u:.7} lost in {:?}, but its {} of {}[{}] reached {:.7}: a partial change",
                    l.tree, e.token, e.path, e.line, l.result
                ));
            }
            let named = w.collisions.iter().any(|c| {
                c.tree == l.tree
                    && c.result == l.result
                    && c.loser == *u
                    && c.path == e.path
                    && c.lines
                        .iter()
                        .zip(&c.lost)
                        .any(|(x, t)| *x == e.line && *t == e.token)
            });
            // excuse: R3e.2 the tree before held it, or a version written over it
            if !named && !b.member(&e.line).is_some_and(ends) {
                return Err(format!(
                    "{u:.7} lost in {:?}, but no collision names its {} of {}[{}]",
                    l.tree, e.token, e.path, e.line
                ));
            }
        }
    }
    w.landings_checked = w.landings.len();
    Ok(())
}

/// Every losing change is replicated to the shared remote, eventually, and
/// stays there attributable (owner, 2026-11-03: "yes, \"losing\" patches
/// should still be part of the history. i want to know who changed what
/// when"). Run once Replication has drained ([`check_converged`]). Read from
/// the remote alone: the commits of its main's history hold exactly the
/// run's collision records, each in its result commit, which places the
/// decision in main's order; each record's ref
/// names its losing commit; and each lost version is in its file there and was
/// written by a commit naming that version's writer (who, what). The commit's date is kept as its
/// audit "when" but never read to decide anything (rule 2), and not
/// checked here.
fn losers_pushed(w: &World) -> Result<(), String> {
    let remote = w.remote();
    let exists = try_git(
        remote,
        &["rev-parse", "--verify", "-q", "refs/heads/main"],
        None,
    );
    let records = if exists.code == 0 {
        decisions_in(remote, &["refs/heads/main"])
    } else {
        Vec::new()
    };
    let mut want: Vec<String> = w
        .collisions
        .iter()
        .map(|c| format!("{} {:?}", c.result, Decision::of(c)))
        .collect();
    let mut got: Vec<String> = records
        .iter()
        .map(|(on, n)| format!("{on} {n:?}"))
        .collect();
    want.sort();
    got.sort();
    if want != got {
        return Err(format!(
            "the remote main's collision records {got:?} are not the run's collisions {want:?}"
        ));
    }
    for (_, c) in &records {
        let at = try_git(remote, &["rev-parse", "--verify", "-q", &c.reference], None);
        if at.stdout.trim() != c.loser {
            return Err(format!(
                "{c:?}: the remote's {} is {:?}",
                c.reference,
                at.stdout.trim()
            ));
        }
        let file = Content::parse(&git(
            remote,
            &["cat-file", "-p", &format!("{}:{}", c.reference, c.path)],
        ))
        .ok_or_else(|| format!("{c:?}: unreadable on the remote"))?;
        for token in &c.lost {
            if !file.holds(token) {
                return Err(format!("{c:?}: on the remote, lost {token} is not there"));
            }
            let origin = origin_in(
                &c.reference,
                |x| commit_parents(remote, x),
                |x| {
                    let spec = format!("{x}:{}", c.path);
                    Content::parse(&git(remote, &["cat-file", "-p", &spec]))
                        .is_some_and(|f| f.holds(token))
                },
            );
            let by = git(
                remote,
                &[
                    "log",
                    "-1",
                    "--format=%(trailers:key=Agentfs-Writer,valueonly)",
                    &origin,
                ],
            );
            if by != writer_of(token) {
                return Err(format!(
                    "{c:?}: on the remote, lost {token} was written by a commit by {by}"
                ));
            }
        }
    }
    Ok(())
}

/// The collision's result is a `Landing` (or the transaction's) commit made
/// on `c.tree`, and its losing commit was made in a fork strictly below
/// `c.tree` (a nested fork's commit can reach it by fast-forward), or on
/// the branch of a transaction of fork `c.tree`.
fn decided_at_the_upstreams_queue(w: &World, c: &Collision) -> bool {
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

/// Whether the upstream's line `x`, which a sync left out of `r`, stands in front
/// of the fork's line `o`, read from the upstream's file `u` and the fork's
/// `ours` before the sync. A line `u` holds keeps its place there (R10.7), so
/// where `u` holds `o`, by `u`'s order; where `ours` holds both, by its order.
/// Otherwise a sync places `o` past lines the fork never saw, up to the next
/// line it did see, so `x` stands behind `o` only if `u` has such a line before
/// `x` that `r` holds after `o`.
fn in_front_of(u: &Content, ours: &Content, r: &Content, x: &Line, o: &Line) -> bool {
    let at = |c: &Content, line: &str| c.lines.iter().position(|l| l.line == line);
    if let (Some(a), Some(b)) = (at(u, &x.line), at(u, &o.line)) {
        return a < b;
    }
    if let (Some(a), Some(b)) = (at(ours, &x.line), at(ours, &o.line)) {
        return a < b;
    }
    let Some(from) = at(r, &o.line) else {
        return false;
    };
    let after: BTreeSet<&str> = r.lines[from + 1..]
        .iter()
        .map(|l| l.line.as_str())
        .collect();
    u.lines
        .iter()
        .take_while(|l| l.line != x.line)
        .all(|l| !after.contains(l.line.as_str()))
}

/// Whether a line ends without a newline; false for a deleted one.
fn ends_open(l: &Line) -> bool {
    l.text().is_some_and(|t| !t.ends_with('\n'))
}

/// The versions `upstream` had taken within the first `entered` records.
fn taken_by(w: &World, upstream: Tree, entered: usize) -> BTreeSet<&str> {
    w.entered[..entered]
        .iter()
        .filter(|(x, _)| *x == upstream)
        .map(|(_, e)| e.token.as_str())
        .collect()
}

/// P2: a version `upstream` took only in takes a sync of its commit `up` holds
/// back (the take's result commit is not in `up`, read from git).
fn held_back(w: &World, upstream: Tree, up: &str, id: &str) -> bool {
    taken_only_past(w, up, id, |l| l.tree == upstream)
}

/// Whether version `id` was taken in at least one of `in_scope`'s takes, and
/// only in takes whose result commit is not in `up`.
fn taken_only_past(w: &World, up: &str, id: &str, in_scope: impl Fn(&Landed) -> bool) -> bool {
    let mut takes = w
        .landings
        .iter()
        .filter(|l| in_scope(l))
        .filter(|l| w.edits(&l.base, &l.change).iter().any(|e| e.token == id))
        .peekable();
    takes.peek().is_some() && takes.all(|l| !w.is_ancestor(&l.result, up))
}

/// R9.3 with R9.9, whoever wrote the line (finding 50): the decision on
/// version `id` is in the upstream's head, in a take of any tree (a
/// fast-forward brings a tree further up's takes there), and not in the commit
/// `up` this sync delivered; and what stopped the sync at `up` is a lock, the
/// first commit it held back touching a file open in fork `t`
/// ([`prefix_cut`]). The owner, 2026-11-08, option A: the rest arrives when the
/// file closes.
fn decision_held_back_by_lock(
    w: &World,
    t: Tree,
    upstream: Tree,
    up: &str,
    result: &str,
    id: &str,
) -> bool {
    let head = w.head(upstream);
    let Some(ours) = commit_parents(w.repo(), result).into_iter().next() else {
        return false;
    };
    taken_only_past(w, up, id, |l| w.is_ancestor(&l.result, &head))
        && prefix_cut(w, t, &ours, &head, &w.write_open(t), Some(&up.to_string())).is_ok()
}

/// R10.6 at one sync: `x`, the upstream's version in `u` of a line the sync's
/// result `r` left out, is the upstream's unfinished last line and stands in front
/// of a live line of `r` still undecided at that sync.
fn r10_6_hides(
    u: &Content,
    ours: Option<&Content>,
    r: &Content,
    x: &Line,
    undecided: impl Fn(&Line) -> bool,
) -> bool {
    ends_open(x)
        && u.live().last() == Some(x)
        && r.live()
            .filter(|o| undecided(o))
            .any(|o| ours.is_some_and(|m| in_front_of(u, m, r, x, o)))
}

/// Line `line` of `path`, whose version `token` entered `t`, is out of `t`'s
/// file because `t`'s last sync hid it under R10.6: that sync's result lacks it,
/// and the upstream's version in the commit it delivered was written over `token`
/// and is hidden by [`r10_6_hides`]. So the next sync, which delivers the
/// upstream's decision, must bring the line back or fail here, and a line that
/// left any other way (a save, a sync with no such reason, an upstream's version
/// not written over `token`) fails. The owner, 2026-11-06, finding 35: "A is fine".
fn hidden_by_r10_6(w: &World, t: Tree, path: &str, line: &str, token: &str) -> bool {
    let Tree::Fork(f) = t else {
        return false;
    };
    let Some((_, up, result, entered)) = w.syncs.iter().rev().find(|s| s.0 == t) else {
        return false;
    };
    let (u, r) = (w.file(up, path), w.file(result, path));
    let Some(x) = u.member(line) else {
        return false;
    };
    if r.member(line).is_some() || !w.lineage.descends(&x.id, token) {
        return false;
    }
    let upstream = w.forks[&f].upstream;
    let took = taken_by(w, upstream, *entered);
    let ours = commit_parents(w.repo(), result)
        .first()
        .map(|c| w.file(c, path));
    r10_6_hides(&u, ours.as_ref(), &r, x, |o| {
        !is_root(&o.id) && (!took.contains(o.id.as_str()) || held_back(w, upstream, up, &o.id))
    })
}

/// Rule 3 per sync, as of the moment its take began: the fork now holds
/// the upstream's version of every line, except where it holds its own
/// version the upstream's queue had not decided then (that version had not
/// entered the upstream), or had decided only in a take a prefix sync holds
/// back (P2: the take's result commit is not in the upstream commit the
/// sync delivered, read from git). A sync carries the upstream's decided
/// state: a fork version the upstream already took is replaced by the
/// upstream's version, whichever won. Two upstream's lines may be absent while
/// the fork's own line is undecided, each only where the rule says: its
/// unfinished last line in front of that line (R10.6), and a line behind that
/// line when it is unfinished (R10.9), which the fork never held or holds
/// only as its own delete (finding 43).
fn syncs_deliver(w: &mut World) -> Result<(), String> {
    for (t, up, result, entered) in &w.syncs[w.syncs_checked..] {
        let Tree::Fork(f) = t else {
            return Err("main synced".into());
        };
        let upstream = w.forks[f].upstream;
        let took = taken_by(w, upstream, *entered);
        let held_back = |id: &str| held_back(w, upstream, up, id);
        let decided = |id: &str| is_root(id) || took.contains(id);
        // Finding 11: the upstream gave file `p` a path that the fork's own
        // undecided name of another file holds there, or (a chain) a file
        // shown under its previous name by this same exception holds
        // there; the fork keeps showing `p` under the name it had (the one
        // exception). Grown from the undecided names, so no cycle of
        // files excuses itself.
        // excuse: R11.1 the fork shows a file under its previous name until decided
        let mut kept: BTreeSet<&str> = PATHS
            .iter()
            .copied()
            .filter(|q| {
                let id = &w.file(result, q).name.id;
                !decided(id) || held_back(id)
            })
            .collect();
        loop {
            let more: Vec<&str> = PATHS
                .iter()
                .copied()
                .filter(|p| !kept.contains(p))
                .filter(|p| {
                    w.file(up, p).path().is_some_and(|path| {
                        kept.iter()
                            .any(|q| q != p && w.file(result, q).path() == Some(path))
                    })
                })
                .collect();
            if more.is_empty() {
                break;
            }
            kept.extend(more);
        }
        for p in PATHS {
            let (u, r) = (w.file(up, p), w.file(result, p));
            let clash = u.path().is_some_and(|path| {
                PATHS
                    .iter()
                    .any(|q| *q != p && kept.contains(q) && w.file(result, q).path() == Some(path))
            });
            for x in r.records() {
                let theirs = u.member(&x.line);
                // excuse: R11.1 the name line of a file shown under its previous name
                if clash && x.line == u.name.line {
                    continue;
                }
                // excuse: R3d the fork keeps its own lines the upstream's queue has not decided
                // excuse: R9.3 a prefix sync holds back the commits from the first to an open file
                // excuse: R9.3 and with them any line whose decision a lock holds back, whoever wrote it
                if theirs.is_none_or(|u| u.id != x.id)
                    && decided(&x.id)
                    && !held_back(&x.id)
                    && !decision_held_back_by_lock(w, *t, upstream, up, result, &x.id)
                {
                    return Err(format!(
                        "a sync of {up:.7} into {t:?} left {p}[{}] at {}, which {upstream:?} already took, not its {theirs:?}",
                        x.line, x.id
                    ));
                }
            }
            // The fork's lines still undecided, for what a sync may hide.
            let own_writer = w.forks[f].writer.as_str();
            let undecided = |o: &Line| !decided(&o.id) || held_back(&o.id);
            let ours = commit_parents(w.repo(), result)
                .first()
                .map(|c| w.file(c, p));
            let in_front =
                |x: &Line, o: &Line| ours.as_ref().is_some_and(|m| in_front_of(&u, m, &r, x, o));
            let hidden = |x: &Line| {
                // excuse: R10.6 the upstream's unfinished last line in front of an undecided line
                let before_own = r10_6_hides(&u, ours.as_ref(), &r, x, undecided);
                // excuse: R10.9 the upstream's lines after an undecided unfinished line, which the fork never held or holds only as its own delete
                let after_own = x.live()
                    && ours.as_ref().is_some_and(|m| {
                        m.member(&x.line)
                            .is_none_or(|o| !o.live() && writer_of(&o.id) == own_writer)
                    })
                    && r.live()
                        .filter(|o| undecided(o))
                        .any(|o| ends_open(o) && !in_front(x, o));
                before_own || after_own
            };
            if let Some(x) = u
                .records()
                .find(|x| r.member(&x.line).is_none() && !hidden(x))
            {
                return Err(format!(
                    "a sync of {up:.7} into {t:?} dropped line {} of {p}",
                    x.line
                ));
            }
        }
    }
    w.syncs_checked = w.syncs.len();
    Ok(())
}

/// Rule 5, as the program was told, whole (rule 3e): a committed
/// transaction's every staged edit is in the fork's commit that carries it
/// (or overwritten by a later edit of its own), and no collision names
/// that commit; a transaction that lost has none of its staged versions in
/// that commit, a collision in the fork that really collided, and its
/// every staged edit named in the answer (by its last staged version of
/// that line), with a collision in the fork naming it and the version
/// named as kept in the commit; the answer names every line the commit's collisions lost. A
/// rolled-back one's every staged token is discarded
/// (`discarded_leave_no_trace` holds that); an empty one staged nothing.
fn transactions_end_as_told(w: &mut World) -> Result<(), String> {
    for e in &w.ended[w.ended_checked..] {
        let fork = Tree::Fork(e.fork);
        // Whether the transaction created `s`'s file (a new file) and its
        // last staged version of the file's name line ended it: then the
        // file changed nothing anyone sees (an atomic save's new file), so
        // none of its staged versions is in the commit (`World::edits`).
        // excuse: R11.2 a file a transaction created and ended changed nothing anyone sees
        let created_and_ended = |s: &crate::world::Edit| {
            let names = || {
                e.staged
                    .iter()
                    .filter(|x| x.path == s.path && x.line == s.path)
            };
            crate::content::UNBORN.contains(&s.path.as_str())
                && names().any(|x| !x.gone)
                && names().next_back().is_some_and(|x| x.gone)
        };
        let ok = match (&e.how, &e.result) {
            (TxnEnd::Commit(TxnOutcome::Committed), Some(r)) => {
                let none_lost = !w.collisions.iter().any(|c| c.result == *r);
                let staged = e.staged.iter().filter(|s| !created_and_ended(s)).all(|s| {
                    w.file(r, &s.path)
                        .member(&s.line)
                        .is_some_and(|v| w.lineage.descends(&v.id, &s.token))
                        && w.entered.iter().any(|(t, x)| {
                            *t == fork
                                && x.path == s.path
                                && x.line == s.line
                                && w.lineage.descends(&x.token, &s.token)
                        })
                });
                none_lost && staged
            }
            (TxnEnd::Commit(TxnOutcome::Lost { lines }), Some(r)) => {
                let mine: Vec<&Collision> =
                    w.collisions.iter().filter(|c| c.result == *r).collect();
                let real = mine
                    .iter()
                    .any(|c| c.by != By::Whole && really_collided(w, c));
                let told = lines.iter().all(|o| {
                    lost_in(w, &[fork], &o.path, &o.line, &o.staged) && holds(w, r, &o.kept)
                });
                let all_told = mine.iter().map(|c| c.lost.len()).sum::<usize>() == lines.len();
                let none_applied = e.staged.iter().all(|s| !w.file(r, &s.path).holds(&s.token));
                let named = e.staged.iter().filter(|s| !created_and_ended(s)).all(|s| {
                    lines.iter().any(|o| {
                        o.path == s.path
                            && o.line == s.line
                            && w.lineage.descends(&o.staged, &s.token)
                    })
                });
                real && told && all_told && none_applied && named
            }
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

/// R5.3, read from git: while a transaction is open, no save reaches its
/// fork's tree on a file the transaction had saved at the last check. Other
/// I/O on such a file is deferred. A sync or a landing is not I/O of a client:
/// it moves the tree with a commit whose cause is not `Direct`, or one made
/// in another tree.
fn transactions_undisturbed(w: &mut World) -> Result<(), String> {
    let mut seen = BTreeMap::new();
    for f in w.forks.values().filter(|f| f.txn.is_some()) {
        let txn = f.txn.as_ref().expect("open");
        let (tree, head) = (World::name(Tree::Fork(f.id)), f.head.clone());
        if let Some((at, files)) = w.txn_files_seen.get(&txn.id) {
            let files: &BTreeSet<String> = files;
            let mut c = head.clone();
            while !w.is_ancestor(&c, at) {
                let direct = w.trailer(&c, "Agentfs-Cause") == "Direct"
                    && w.trailer(&c, "Agentfs-Tree") == tree;
                if direct && let Some(p) = change_files(w, &c).intersection(files).next() {
                    return Err(format!(
                        "{c:.7} saved {p} in fork {} while transaction t{} had it open: R5.3 defers that",
                        f.id, txn.id
                    ));
                }
                c = git(w.repo(), &["rev-parse", &format!("{c}^1")]);
            }
        }
        seen.insert(txn.id, (head, diff_files(w, &txn.start, &txn.head)));
    }
    w.txn_files_seen = seen;
    Ok(())
}

/// Rule 6: every commit names exactly one writer; a non-merge commit's
/// change (the records its diff from its parent adds) is that writer's
/// alone, and rule 8's `O_APPEND` in every save ([`appends_exact`]); a merge originates nothing: every
/// record it adds over its first parent is in one of its parents, and it
/// names the writer of its second parent (a landing or a transaction's
/// commit), or `agentfs` if it is a sync. So every version traces through
/// ancestry to the one non-merge commit that wrote it, which names its
/// writer. Each commit is checked once: commits never change.
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
        "log".to_string(),
        "--no-walk=unsorted".into(),
        "--diff-merges=first-parent".into(),
        "-p".into(),
        "--format=%x01%H%x1f%P%x1f%(trailers:key=Agentfs-Writer,valueonly,separator=%x1e)%x1f%(trailers:key=Agentfs-Cause,valueonly)%x1f%(trailers:key=Agentfs-Open,valueonly,separator=%x1e)%x02".into(),
    ];
    args.extend(new.iter().cloned());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let log = git(w.repo(), &args);
    for block in log.split('\x01').skip(1) {
        let (header, diff) = block.split_once('\x02').unwrap_or((block, ""));
        let fields: Vec<&str> = header.split('\x1f').collect();
        let [commit, parents, writers, cause, opens] = fields[..] else {
            return Err(format!("unparsable log header {header:?}"));
        };
        // git-model.md §3: a save from an open names that open, once; no
        // open is saved twice (a crash's replay of the journal included).
        let opens: Vec<&str> = opens
            .split('\x1e')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if w.save_calls.contains_key(commit) != (opens.len() == 1) || opens.len() > 1 {
            return Err(format!("{commit} names opens {opens:?}"));
        }
        for o in opens {
            if let Some(c) = w.opens_saved.insert(o.to_string(), commit.to_string()) {
                return Err(format!("open {o} saved twice: {c:.7} and {commit:.7}"));
            }
        }
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
        let added = added(diff);
        for (_, rec) in &added {
            if let State::Replaced(by) = &rec.state {
                let v = w.file(commit, by).name.id;
                w.rename_overs
                    .entry(rec.id.clone())
                    .or_insert((by.clone(), v));
            }
        }
        if cause.trim() == "Direct" && parents.len() == 1 {
            let paths: BTreeSet<&String> = added.iter().map(|(p, _)| p).collect();
            for path in paths {
                let (base, now) = (w.file(parents[0], path), w.file(commit, path));
                save_aligned(&base, &now).map_err(|e| format!("{commit}: save of {path}: {e}"))?;
                if let Some(calls) = w.save_calls.get(commit) {
                    appends_exact(&base, &now, calls)
                        .map_err(|e| format!("{commit}: save of {path}: {e}"))?;
                }
            }
        }
        for (path, rec) in added {
            // git's diff may show a record it kept, but whose neighbours
            // moved, as removed and added: only a record no parent holds
            // is new.
            let first = w.file(parents.first().copied().unwrap_or(commit), &path);
            if !parents.is_empty() && first.records().any(|l| *l == rec) {
                continue;
            }
            let bad = match parents.len() {
                0 => false,
                1 => writer_of(&rec.id) != writers[0],
                _ => !parents
                    .iter()
                    .any(|p| w.file(p, &path).records().any(|l| *l == rec)),
            };
            if bad {
                return Err(format!(
                    "{commit} ({}, parents {}) names {} but adds {} in {path}",
                    cause.trim(),
                    parents.len(),
                    writers[0],
                    rec.id
                ));
            }
        }
    }
    w.checked_commits.extend(new);
    Ok(())
}

/// Rule 11: a rename-over is one call, so it applies whole or not at all.
/// In every live tree, a file unlinked by a rename-over has the renamed
/// file at that save's name version or a later one: never the unlink
/// without the rename. (The rename without the unlink would leave two
/// files at one path, which the model refuses outright.) One exception, a
/// cost: a fork that holds a name version of the renamed file other than
/// its upstream's (it has not synced since the upstream's queue decided it), or
/// a fork's version main has not taken (a sync kept it as undecided, in
/// this fork or a tree above it that this fork synced from); main has no
/// such exception. A delete of the renamed file that agrees with one written
/// over the rename is not half a rename-over (R10.3, [`deleted_over`]).
fn rename_overs_whole(w: &World) -> Result<(), String> {
    for (t, files) in &w.views {
        for (x, c) in files {
            let Some((by, v)) = w.rename_overs.get(&c.name.id) else {
                continue;
            };
            let n = &files[by].name;
            // excuse: R10.3 a delete agreeing with one written over the rename is not half of it
            if !n.live() && deleted_over(w, *t, by, by, v) {
                continue;
            }
            // excuse: R3a a fork keeps its own undecided name version until the upstream decides
            let undecided = match t {
                Tree::Fork(f) => {
                    w.file(&w.head(w.forks[f].upstream), by).name.id != n.id
                        || (writer_of(&n.id).starts_with('c')
                            && w.file(&w.main_head, by).name.id != n.id)
                }
                Tree::Main => false,
            };
            if !undecided && !w.lineage.descends(&n.id, v) {
                return Err(format!(
                    "{t:?}: {x} was unlinked by renaming {by} over it, but {by}'s name is {}, \
                     not that rename's {v} or later",
                    n.id
                ));
            }
        }
    }
    Ok(())
}

/// Rule 10, read from the two blobs alone, not from the replay that wrote
/// them: a save keeps every line identity its first parent holds, in the
/// same order, each either unchanged or one new version written over the
/// parent's version; every other line is new (its identity its own id,
/// written over nothing, live). So an alignment that drops, duplicates or
/// reorders a line, or pairs a changed region out of order, fails here.
pub fn save_aligned(base: &Content, now: &Content) -> Result<(), String> {
    for b in base.records() {
        let n = now
            .member(&b.line)
            .ok_or_else(|| format!("line {} vanished", b.line))?;
        if n != b && n.over.as_deref() != Some(b.id.as_str()) {
            return Err(format!(
                "{} is not written over the parent's {} of line {}",
                n.id, b.id, b.line
            ));
        }
    }
    for n in now.records() {
        if base.member(&n.line).is_none() && (n.line != n.id || n.over.is_some() || !n.live()) {
            return Err(format!("new line {} is not a fresh live line", n.id));
        }
    }
    let order = |c: &Content| -> Vec<String> {
        c.lines
            .iter()
            .filter(|l| base.member(&l.line).is_some())
            .map(|l| l.line.clone())
            .collect()
    };
    if order(now) != order(base) {
        return Err(format!(
            "the parent's lines changed order: {:?} became {:?}",
            order(base),
            order(now)
        ));
    }
    Ok(())
}

/// Rule 8's `O_APPEND`, read from a save's two blobs and the byte calls its
/// program made ([`World::save_calls`], recorded as made, not the replay's
/// output). The appends after the save's last other call wrote the file's
/// tail at close: the save's bytes are the earlier calls' bytes `b0`
/// (computed here from the parent's bytes) followed by the appended bytes.
/// In a save of appends only, every line starting in the appended bytes is a
/// new line (its own identity, written over nothing) after every line of the
/// parent, so it wrote over no line, another writer's append included. If the
/// parent ends in a partial line, the appended bytes finish it: that line's
/// bytes start with the partial line, a new version written over the parent's
/// last line, not a new line ("foo" and an append of "bar" is one line,
/// "foobar"). A save with any other call is a rewrite, aligned as a whole
/// (finding 37; the owner, 2026-11-07, "2. B"), so only its bytes are checked
/// here.
fn appends_exact(
    base: &Content,
    now: &Content,
    calls: &[crate::world::Write],
) -> Result<(), String> {
    use crate::world::Write;
    let k = calls
        .iter()
        .rposition(|c| !matches!(c, Write::Append(_)))
        .map_or(0, |i| i + 1);
    let tail: String = calls[k..]
        .iter()
        .map(|c| match c {
            Write::Append(x) => x.as_str(),
            _ => "",
        })
        .collect();
    if tail.is_empty() {
        return Ok(());
    }
    let b0 = bytes_after(&base.bytes(), &calls[..k]);
    if now.bytes() != format!("{b0}{tail}") {
        return Err(format!(
            "its bytes {:?} are not its earlier calls' {b0:?} and then its appends' {tail:?}",
            now.bytes()
        ));
    }
    if k != 0 {
        return Ok(());
    }
    let mut offset = 0;
    for (i, l) in now.lines.iter().enumerate() {
        let Some(t) = l.text() else {
            continue;
        };
        let start = offset;
        offset += t.len();
        if start >= b0.len() {
            let fresh = base.member(&l.line).is_none() && l.line == l.id && l.over.is_none();
            let after_all = now.lines[i..]
                .iter()
                .all(|x| base.member(&x.line).is_none());
            if !fresh || !after_all {
                return Err(format!(
                    "append's line {} wrote over a line, or not after the end",
                    l.id
                ));
            }
        } else if offset > b0.len() {
            // The line the appends finished: b0's partial last line.
            let partial = &b0[b0.rfind('\n').map_or(0, |n| n + 1)..];
            if !t.starts_with(partial) {
                return Err(format!(
                    "append finished line {} as {t:?}, not after {partial:?}",
                    l.id
                ));
            }
            let last = base.live().last();
            if last.is_none_or(|p| p.line != l.line || l.over.as_ref() != Some(&p.id)) {
                return Err(format!(
                    "an append onto the partial last line {last:?} made {l:?}, not a version of it"
                ));
            }
        }
    }
    Ok(())
}

/// The check's own account of byte calls on `bytes`: kept apart from the
/// model's replay ([`crate::world::Write::apply_all`]).
fn bytes_after(bytes: &str, calls: &[crate::world::Write]) -> String {
    use crate::world::Write;
    let mut b: Vec<u8> = bytes.as_bytes().to_vec();
    for c in calls {
        match c {
            Write::Append(x) => b.extend_from_slice(x.as_bytes()),
            Write::Pwrite { offset, bytes } => {
                for (j, x) in bytes.bytes().enumerate() {
                    let at = offset + j;
                    while b.len() <= at {
                        b.push(0);
                    }
                    b[at] = x;
                }
            }
            Write::Truncate(n) => {
                b.truncate(*n);
                while b.len() < *n {
                    b.push(0);
                }
            }
        }
    }
    String::from_utf8(b).expect("ASCII bytes")
}

/// `(file, record)` of every added record line in a unified diff.
fn added(diff: &str) -> Vec<(String, Line)> {
    let mut out = Vec::new();
    let mut path = String::new();
    for line in diff.lines() {
        if let Some(p) = line.strip_prefix("+++ b/") {
            path = p.to_string();
        } else if let Some(rest) = line.strip_prefix('+')
            && !line.starts_with("+++")
            && let Some(rec) = Line::parse(rest)
        {
            out.push((path.clone(), rec));
        }
    }
    out
}

/// A rolled-back transaction leaves no line in any tree, and no commit
/// reachable from any ref. A program that read its staged lines and writes
/// their bytes back after the rollback makes new lines, its own write
/// (rule 8). Each commit is grepped once,
/// until a new discard adds tokens.
fn discarded_leave_no_trace(w: &mut World, all: &[String]) -> Result<(), String> {
    if w.discarded.is_empty() {
        return Ok(());
    }
    for t in w.live_trees() {
        for p in PATHS {
            let f = w.file(&w.head(t), p);
            if let Some(l) = f.records().find(|l| w.discarded.contains(&l.id)) {
                return Err(format!("discarded {} shows in {t:?}:{p}", l.id));
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
    // Versions, by id: a later rewrite's new line may hold a discarded
    // line's bytes, a new write by its program with its own id.
    let mut args: Vec<String> = vec!["grep".into(), "-l".into(), "-E".into()];
    for t in &w.discarded {
        args.extend(["-e".into(), format!("^{}\t", t.replace('.', "\\."))]);
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

/// Rule 7: a tree's history reaches every change it took, won or lost. Read
/// by ancestry alone (`git rev-list <head>`, never a `from..to` range, whose
/// walk stops early by commit date).
fn full_lineage(w: &mut World) -> Result<(), String> {
    let repo = w.repo().to_path_buf();
    let landed: Vec<(Tree, String)> = w
        .landings
        .iter()
        .filter(|l| l.direction == Direction::Landing && w.alive(l.tree))
        .map(|l| (l.tree, l.change.clone()))
        .collect();
    for (t, c) in &landed {
        let head = w.head(*t);
        let history = w
            .reach
            .entry(head.clone())
            .or_insert_with(|| reach(&repo, &head));
        if !history.contains(c) {
            return Err(format!("{t:?}'s history lost landed {c}"));
        }
    }
    Ok(())
}

/// Rules 10 and 11 in every take (a landing or a transaction's commit),
/// read from git: the take's head before, its result, and each change's
/// own file. Rule 10's placement: a new line a winning change added
/// (an identity its base lacks) that the take placed is put after the
/// nearest line before it in the change's file that the result holds,
/// past only lines the change never saw (an upstream's concurrent new lines:
/// of two inserts at one place the upstream's comes first), and no line the
/// upstream added that the change never saw directly follows it. Rule 11: a
/// change that edits a file the tree had unlinked before the take (and does
/// not unlink it too, nor create it: an unborn file) loses, by a file rule. Counts each in the world, so
/// a run can show it met them.
fn placed_and_unlinked(w: &mut World) -> Result<(), String> {
    let mut met = (0, 0);
    for l in &w.landings[w.placements_checked..] {
        let (u, base) = (&l.change, &l.base);
        // R10.4, read from git: a landing leaves no line after the upstream's
        // unfinished last line.
        if l.direction == Direction::Landing {
            for p in PATHS {
                let b = w.file(&l.before, p);
                let Some(q) = b
                    .live()
                    .last()
                    .filter(|x| x.text().is_some_and(|t| !t.ends_with('\n')))
                else {
                    continue;
                };
                if let Some(x) = w
                    .file(&l.result, p)
                    .live()
                    .skip_while(|x| x.id != q.id)
                    .nth(1)
                {
                    return Err(format!(
                        "{u:.7}'s line {} in {p} comes after the upstream's unfinished last line {}",
                        x.id, q.id
                    ));
                }
            }
        }
        for p in PATHS {
            let b = w.file(&l.before, p);
            let edits: Vec<_> = w
                .edits(base, u)
                .into_iter()
                .filter(|e| e.path == p)
                .collect();
            if edits.is_empty() {
                continue;
            }
            let unlinks = edits.iter().any(|e| e.line == b.name.line && e.gone);
            // A create: its name version written over the tree's deleted
            // one (read from the change's line map), which only a new
            // file's create can be.
            let name = w.file(u, p).name;
            let creates = edits.iter().any(|e| e.line == b.name.line && !e.gone)
                && name.over.as_deref() == Some(b.name.id.as_str());
            let applies = edits.iter().any(|e| {
                b.member(&e.line)
                    .is_none_or(|v| v.id != e.token && !w.lineage.descends(&v.id, &e.token))
            });
            // excuse: R11 a change that unlinks the file too, or creates it, is no edit of an unlinked file
            if !b.name.live() && !unlinks && !creates && applies {
                met.1 += 1;
                let lost = w.collisions.iter().any(|c| {
                    c.tree == l.tree
                        && c.result == l.result
                        && c.loser == *u
                        && c.path == p
                        && c.by == By::File
                });
                if !lost {
                    return Err(format!(
                        "{u:.7} edits {p}, which {:?} had unlinked, and did not lose by a file rule",
                        l.tree
                    ));
                }
            }
            if !l.won {
                continue;
            }
            let (src, r, mine) = (w.file(u, p), w.file(&l.result, p), w.file(base, p));
            let saw = |line: &str| src.lines.iter().any(|x| x.line == line);
            for (k, n) in src.lines.iter().enumerate() {
                if mine.member(&n.line).is_some() || b.member(&n.line).is_some() {
                    continue;
                }
                let Some(i) = r.lines.iter().position(|x| x.line == n.line) else {
                    continue;
                };
                let anchor = src.lines[..k]
                    .iter()
                    .rev()
                    .find(|x| r.lines.iter().any(|y| y.line == x.line))
                    .map(|x| x.line.as_str());
                let mut j = i;
                while j > 0 && !saw(&r.lines[j - 1].line) {
                    j -= 1;
                }
                let prev = j.checked_sub(1).map(|j| r.lines[j].line.as_str());
                if prev != anchor {
                    return Err(format!(
                        "{u:.7}'s new line {} in {p} is placed after {prev:?}, not after {anchor:?}",
                        n.id
                    ));
                }
                if let Some(x) = r.lines.get(i + 1)
                    && b.member(&x.line).is_some()
                    && !saw(&x.line)
                {
                    return Err(format!(
                        "{u:.7}'s new line {} in {p} comes before the upstream's {} at its place",
                        n.id, x.id
                    ));
                }
                if j < i && r.lines[j..i].iter().any(|x| b.member(&x.line).is_some()) {
                    met.0 += 1;
                }
            }
        }
    }
    w.placements_checked = w.landings.len();
    w.inserts_met += met.0;
    w.unlinked_edits += met.1;
    Ok(())
}

/// Every commit `head` reaches.
fn reach(repo: &Path, head: &str) -> BTreeSet<String> {
    git(repo, &["rev-list", head])
        .lines()
        .map(str::to_string)
        .collect()
}
