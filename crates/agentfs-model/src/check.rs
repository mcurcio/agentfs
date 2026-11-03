//! The invariants (MODEL.md, "What must always hold"), read back from the
//! repository and the trees after every step. Each returns the first
//! violation found. Reads over git objects are cached in the [`World`]:
//! commits, their ancestry and blobs never change.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::content::{Content, EMPTY, PATHS, SLOTS, line_token, writer_of};
use crate::git::{git, try_git};
use crate::world::{
    Client, Collision, CollisionScope, Direction, Entry, ForkId, HistoryPolicy, Life, Step, Take,
    Target, Tree, TxnEnd, TxnId, TxnOutcome, World, blame_in,
};

/// Every invariant that holds after any step.
pub fn check(w: &mut World) -> Result<(), String> {
    let now = views(w);
    no_silent_loss(w, &now)?;
    no_flicker(w, &now)?;
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
        w.refs_checked = Some(w.ref_moves);
    }
    history_setting(w)?;
    Ok(())
}

/// One step, with what only the step itself shows. A take (rule 9) that
/// begins either waits, taking nothing, or holds its files; a sync's take
/// reads the parent's head as it is then (read here before the step, not
/// from the model's record). A take that ends writes no file a client of
/// that tree has open for writing, nor any file it did not hold; a sync
/// records no collision (only a parent's queue decides one) and carries
/// the head its take read. Returns the queue entries taken.
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
            w.step(s);
            let after = w.head(*t);
            let wrote: Vec<String> = if before == after {
                Vec::new()
            } else {
                git(
                    w.repo(),
                    &["diff-tree", "-r", "--name-only", &before, &after],
                )
                .lines()
                .map(str::to_string)
                .collect()
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
                None if !w.is_ancestor(up, &w.head(*t)) => Err(format!(
                    "a sync into {t:?} did nothing, but it lacks the parent's head {up:.7}"
                )),
                _ => Ok(0),
            }
        }
        _ => Ok(w.step(s)),
    }
}

/// Rule 9's locks, after every step and call. No write-open succeeds
/// while a take holds the file in that tree. No lock outlives its holder:
/// every open file's client is alive (and its transaction open), every
/// take's and every wait's tree alive. Every lock wait ends on an event:
/// a waiting queue head waits only while one of its files is still open
/// for writing there (the last close wakes it), a waiting open only while
/// a take holds its file (the release grants it). No timer is modelled.
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
                !h.waiting || w.holds().get(&t).is_some_and(|x| x.files.contains(&h.path))
            }
        };
        if !alive {
            return Err(format!("h{} outlives its client {:?}", h.id, h.client));
        }
        if !held {
            return Err(format!(
                "h{} ({:?}:{}) waits with no take holding its file, or outlives its transaction",
                h.id, h.target, h.path
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

/// Once writes stop and every queue has drained: no step is left but
/// drops, and every live fork's tree equals main's. Stronger than "each
/// fork's untouched files equal main and touched files equal main or the
/// fork's newer non-colliding edit": once the queues drain, a fork has no
/// newer edit left.
pub fn check_converged(w: &World) -> Result<(), String> {
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
        return Err(format!(
            "a lock outlives the run: waits {:?}, holds {:?}",
            w.waits(),
            w.holds().keys().collect::<Vec<_>>()
        ));
    }
    losers_pushed(w)?;
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

type Views = BTreeMap<Tree, BTreeMap<String, Content>>;

/// What every live tree shows now.
fn views(w: &World) -> Views {
    w.live_trees()
        .into_iter()
        .map(|t| {
            (
                t,
                PATHS
                    .iter()
                    .map(|p| (p.to_string(), w.read(t, p)))
                    .collect(),
            )
        })
        .collect()
}

/// Each slot that changed in a live tree since the last check:
/// (tree, file, slot, before, now).
fn moves<'a>(w: &'a World, now: &'a Views) -> Vec<(Tree, &'a str, usize, &'a str, &'a str)> {
    let mut out = Vec::new();
    for (t, files) in now {
        let Some(before) = w.views.get(t) else {
            continue;
        };
        for (p, c) in files {
            for i in 0..SLOTS {
                let (a, b) = (before[p].slot(i), c.slot(i));
                if a != b {
                    out.push((*t, p.as_str(), i, a, b));
                }
            }
        }
    }
    out
}

/// A collision in `trees` names `token`, or a later token written over it,
/// as lost on slot `slot` of `path`.
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
/// token a to token b only if b was written over a, or a recorded collision
/// in that tree or one upstream of it names a as lost; never if a was
/// written over b (b is stale); (b) every token that entered a tree
/// (written there, carried in by a landing or a transaction, won or lost,
/// or taken from the parent by a sync) is still there or overwritten by
/// one written over it, or such a collision names it.
fn no_silent_loss(w: &World, now: &Views) -> Result<(), String> {
    for (t, p, i, a, b) in moves(w, now) {
        if w.lineage.descends(a, b) {
            return Err(format!(
                "stale revert in {t:?}:{p}[{i}]: {a} became {b}, which {a} was written over"
            ));
        }
        if !w.lineage.descends(b, a) && !flagged(w, t, p, i, a) {
            return Err(format!(
                "silent loss in {t:?}:{p}[{i}]: {a} became {b}, which was not written over it, and no collision names it"
            ));
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
    Ok(())
}

/// No flip-flop: a line, once replaced in a tree, never shows there again.
/// So a fork's line changes at most once per collision (its own x, then the
/// parent's winning m, never x again).
fn no_flicker(w: &mut World, now: &Views) -> Result<(), String> {
    let mut left = Vec::new();
    for (t, p, i, a, b) in moves(w, now) {
        if w.left.contains(&(t, p.to_string(), i, b.to_string())) {
            return Err(format!(
                "flicker in {t:?}:{p}[{i}]: {b} shows again after it was replaced (now over {a})"
            ));
        }
        left.push((t, p.to_string(), i, a.to_string()));
    }
    w.left.extend(left);
    Ok(())
}

/// The parent's version wins every collision, decided only at the
/// parent's queue (a landing, or a transaction's commit into its fork:
/// [`Direction`] has no other case, and `sync` records none), and only a
/// real one: in each recorded collision the result keeps the tree's own
/// lines on the slots (its whole file under `File`), each named in `won`;
/// the lost lines are the incoming commit's; each lost and winning pair is
/// concurrent (neither was written over the other); the losing incoming
/// commit stays reachable under the collision's ref; the result's stored
/// parents are the tree's previous head and the incoming commit (a
/// transaction, or a landing under `FullLineage`), or under `Squashed` the
/// previous head alone, with the incoming commit as the graft's second
/// parent. Where it was decided is read from git, not the model's record:
/// the result is the deciding tree's own landing or transaction commit,
/// and the losing commit was written below that tree (a fork under it, or
/// the transaction of that fork).
fn parent_wins(w: &mut World) -> Result<(), String> {
    for c in &w.collisions[w.collisions_checked..] {
        if !decided_at_the_parents_queue(w, c) {
            return Err(format!(
                "{c:?}: not decided at the parent's queue (result {} in {}, loser from {})",
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
        let named = c.slots.len() == c.won.len()
            && c.slots.len() == c.lost.len()
            && c.slots.iter().zip(&c.won).all(|(i, t)| r.slot(*i) == t);
        let ok = match w.policies.collision {
            CollisionScope::Line => c.slots.iter().all(|i| r.slot(*i) == win.slot(*i)),
            CollisionScope::File => r == win,
        };
        if !ok || !named || c.slots.is_empty() {
            return Err(format!("{c:?}: the parent's version did not win"));
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
                "{c:?}: not a collision: one line was written over the other"
            ));
        }
        if let Some(t) = c.lost.iter().find(|t| !lose.contains(t)) {
            return Err(format!("{c:?}: lost {t} is not in the incoming commit"));
        }
        let at = git(w.repo(), &["rev-parse", &c.reference]);
        if at != c.loser {
            return Err(format!("{c:?}: its ref names {at}"));
        }
        let (stored, lineage) = (
            parents(w.repo(), &c.result, false),
            parents(w.repo(), &c.result, true),
        );
        let both = [c.winner.clone(), c.loser.clone()];
        let ok = match (c.direction, w.policies.history) {
            (Direction::Landing, HistoryPolicy::Squashed) => {
                stored == [c.winner.clone()] && lineage == both
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

/// Every losing change is replicated to the shared remote, eventually, and
/// stays there attributable (owner, 2026-11-03: "yes, \"losing\" patches
/// should still be part of the history. i want to know who changed what
/// when"). Run once Replication has drained ([`check_converged`]), over
/// every collision of the run, so a ref lost or moved since its push fails
/// too. Read from the remote alone, as stored (no grafts): each collision's
/// ref names its losing commit; each lost line is on its slot there and
/// blames to a commit naming that line's writer (who, what); and a
/// collision decided in main has its result in the remote main's history,
/// which places the decision in main's order. The commit's date is kept as
/// its audit "when" but never read to decide anything (rule 2), and not
/// checked here.
fn losers_pushed(w: &World) -> Result<(), String> {
    let remote = w.remote();
    for c in &w.collisions {
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
        let blame = blame_in(remote, &c.reference, &c.path, w.policies.layout, true);
        for (slot, token) in c.slots.iter().zip(&c.lost) {
            let by = git(
                remote,
                &[
                    "--no-replace-objects",
                    "log",
                    "-1",
                    "--format=%(trailers:key=Agentfs-Writer,valueonly)",
                    &blame[*slot],
                ],
            );
            if file.slot(*slot) != token || by != writer_of(token) {
                return Err(format!(
                    "{c:?}: on the remote, lost {token} on slot {slot} reads {} by {by}",
                    file.slot(*slot)
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

/// The collision's result is a `Landing` (or the transaction's) commit made
/// on `c.tree`, and its losing commit was made in a fork strictly below
/// `c.tree` (a nested fork's commit can reach it by fast-forward), or on
/// the branch of a transaction of fork `c.tree`.
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

/// Rule 3 per sync, as of the moment its take began: the fork now holds
/// the parent's line on every slot, except where it holds its own line the
/// parent's queue had not decided then (that line had not entered the
/// parent). A sync carries the parent's decided state: a fork line the
/// parent already took is replaced by the parent's line, whichever won.
fn syncs_deliver(w: &mut World) -> Result<(), String> {
    for (t, up, result, entered) in &w.syncs[w.syncs_checked..] {
        let Tree::Fork(f) = t else {
            return Err("main synced".into());
        };
        let parent = w.forks[f].parent;
        let took: BTreeSet<&str> = w.entered[..*entered]
            .iter()
            .filter(|(x, _)| *x == parent)
            .map(|(_, e)| e.token.as_str())
            .collect();
        for p in PATHS {
            let (u, r) = (w.file(up, p), w.file(result, p));
            for i in 0..SLOTS {
                let (u, r) = (u.slot(i), r.slot(i));
                let decided = r == EMPTY || took.contains(r);
                if r != u && decided {
                    return Err(format!(
                        "a sync of {up:.7} into {t:?} left {p}[{i}] at {r}, which {parent:?} already took, not its {u}"
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
/// a later edit of its own), or named in the commit's answer as
/// overwritten, with a collision in the fork naming it and the fork's
/// winning line in the commit; and the answer names every line the commit's
/// collisions lost. A rolled-back one's every staged token is discarded
/// (`discarded_leave_no_trace` holds that); an empty one staged nothing.
fn transactions_end_as_told(w: &mut World) -> Result<(), String> {
    for e in &w.ended[w.ended_checked..] {
        let fork = Tree::Fork(e.fork);
        let ok = match (&e.how, &e.result) {
            (TxnEnd::Commit(TxnOutcome::Committed { overwritten }), Some(r)) => {
                let told = overwritten.iter().all(|o| {
                    lost_in(w, &[fork], &o.path, o.slot, &o.staged)
                        && w.file(r, &o.path).slot(o.slot) == o.kept
                });
                let all_told = w
                    .collisions
                    .iter()
                    .filter(|c| c.result == *r)
                    .map(|c| c.lost.len())
                    .sum::<usize>()
                    == overwritten.len();
                let staged = e.staged.iter().all(|s| {
                    let applied = w
                        .lineage
                        .descends(w.file(r, &s.path).slot(s.slot), &s.token)
                        && w.entered.iter().any(|(t, x)| {
                            *t == fork
                                && x.path == s.path
                                && x.slot == s.slot
                                && w.lineage.descends(&x.token, &s.token)
                        });
                    applied
                        || overwritten.iter().any(|o| {
                            o.path == s.path
                                && o.slot == s.slot
                                && w.lineage.descends(&o.staged, &s.token)
                        })
                });
                told && all_told && staged
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

/// Rule 6: every commit names exactly one writer; a non-merge commit's
/// change (its diff from its parent) is that writer's alone, and rule 8's
/// `O_APPEND`: in a save (cause `Direct`), each line an append wrote
/// ([`World::appended`], recorded at the write call) sits at or after its
/// parent's end, so it wrote over no line, another writer's append
/// included (a landing places a line by its slot: §16's open "what a slot
/// is"); a merge
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
            if cause.trim() == "Direct" && w.appended.contains(&token) {
                let base = w.file(parents[0], &path);
                if slot < base.end() {
                    return Err(format!(
                        "{commit}: append {token} wrote over {path}[{slot}] ({}), not after the end",
                        base.slot(slot)
                    ));
                }
            }
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
    // Whole slot lines: a later rewrite's new line may hold a discarded
    // line's bytes (`<writer>.<n>=<bytes>`), a new write by its program.
    let mut args: Vec<String> = vec!["grep".into(), "-l".into(), "-E".into()];
    for t in &w.discarded {
        args.extend(["-e".into(), format!("^s[0-9]+: {}$", t.replace('.', "\\."))]);
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
