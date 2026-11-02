//! The invariants (MODEL.md, "What must always hold"), read back from the
//! repository and the trees after every step. Each returns the first
//! violation found. Reads over git objects are cached in the [`World`]:
//! commits, their ancestry and blobs never change.

use std::collections::BTreeMap;

use crate::content::{Content, PATHS, SLOTS, line_token, writer_of};
use crate::git::{git, try_git};
use crate::world::{
    CollisionScope, Direction, HistoryPolicy, Step, Tree, TxnEnd, TxnOutcome, World,
};

/// Which checks run: all by default. `Squashed` breaks three (coherence
/// finding 2), so its random runs relax exactly those and keep every other
/// invariant: `real_collisions`, that each collision is between lines that
/// never saw each other; `no_stale_revert`, that no line is replaced by
/// one its writer saw; `flag_in_tree`, that a lost line is flagged by a
/// collision in its own tree or one upstream (relaxed: in any tree, as
/// when a squashed fork commit carries a collision the fork decided and
/// git can no longer tell the parent).
#[derive(Clone, Copy, Debug)]
pub struct Checks {
    pub real_collisions: bool,
    pub no_stale_revert: bool,
    pub flag_in_tree: bool,
}

impl Default for Checks {
    fn default() -> Self {
        Self {
            real_collisions: true,
            no_stale_revert: true,
            flag_in_tree: true,
        }
    }
}

impl Checks {
    /// Every check but the three `Squashed` breaks (finding 2).
    pub fn squashed() -> Self {
        Self {
            real_collisions: false,
            no_stale_revert: false,
            flag_in_tree: false,
        }
    }
}

/// Every invariant that holds after any step.
pub fn check(w: &mut World, checks: Checks) -> Result<(), String> {
    no_silent_loss(w, checks)?;
    incoming_wins(w, checks)?;
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
                    "{t:?}:{p} never converged: {:?} vs main's {:?}",
                    mine.render(),
                    main.render()
                ));
            }
        }
    }
    Ok(())
}

/// A collision in `t` or a tree upstream of it (any tree, without
/// `flag_in_tree`) names `token`, or a later token written over it
/// knowingly, as lost: a loss decided upstream reaches `t` as a plain
/// update.
fn flagged(w: &World, checks: Checks, t: Tree, token: &str) -> bool {
    let up = w.upstream(t);
    w.collisions.iter().any(|c| {
        (!checks.flag_in_tree || up.contains(&c.tree))
            && c.lost.iter().any(|l| w.lineage.descends(l, token))
    })
}

/// Main never silently loses a change, nor does any tree, and no stale
/// line reverts a newer one: (a) in every live tree a slot moves from
/// token a to token b only if b's writer saw a, or a recorded collision in
/// that tree or one upstream of it names a as lost; never if a's writer
/// saw b (b is stale: it was already superseded by a); (b) every token
/// that entered a tree (written there, or carried in by a landing, won or
/// lost) is still there or overwritten by one that saw it, or such a
/// collision names it.
fn no_silent_loss(w: &mut World, checks: Checks) -> Result<(), String> {
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
                if checks.no_stale_revert && a != b && w.lineage.descends(a, b) {
                    return Err(format!(
                        "stale revert in {t:?}:{p}[{i}]: {a} became {b}, which {a}'s writer had already seen"
                    ));
                }
                if a != b && !w.lineage.descends(b, a) && !flagged(w, checks, *t, a) {
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
        if !w.lineage.descends(s, &e.token) && !flagged(w, checks, *t, &e.token) {
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
/// parents are the tree's previous head and the incoming commit (a sync,
/// or a landing under `FullLineage`), or the previous head alone.
fn incoming_wins(w: &mut World, checks: Checks) -> Result<(), String> {
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
            CollisionScope::Hunk => c.slots.iter().all(|i| r.slot(*i) == win.slot(*i)),
            CollisionScope::File => r == win,
        };
        if !ok || !named || c.slots.is_empty() {
            return Err(format!("{c:?}: the incoming change did not win"));
        }
        let concurrent =
            |(l, x): (&String, &String)| !w.lineage.descends(l, x) && !w.lineage.descends(x, l);
        let mut pairs = c.lost.iter().zip(&c.won);
        let real = match w.policies.collision {
            CollisionScope::Hunk => pairs.all(concurrent),
            CollisionScope::File => pairs.any(concurrent),
        };
        if checks.real_collisions && !real {
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
        let parents = git(w.repo(), &["rev-list", "--parents", "-n1", &c.result]);
        let parents: Vec<&str> = parents.split(' ').skip(1).collect();
        let ok = match (c.direction, w.policies.history) {
            // A fast-forward: the fork's commit itself, made on the tree's
            // head.
            (Direction::Landing, HistoryPolicy::FullLineage) if c.result == c.winner => {
                parents.first() == Some(&c.loser.as_str())
            }
            (Direction::Landing, HistoryPolicy::Squashed) => parents == [c.loser.as_str()],
            _ => parents == [c.loser.as_str(), c.winner.as_str()],
        };
        if !ok {
            return Err(format!("{c:?}: result's parents are {parents:?}"));
        }
    }
    w.collisions_checked = w.collisions.len();
    Ok(())
}

/// Rule 5, as the program was told: a committed transaction's every
/// staged edit is in the fork's commit that carries it (or overwritten by
/// a later edit of its own); that commit then entered the fork, so
/// `no_silent_loss` holds it from there. A rejected or rolled-back one's
/// every staged token is discarded (`discarded_leave_no_trace` holds that);
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
            (TxnEnd::Commit(TxnOutcome::Rejected) | TxnEnd::Rollback, None) => {
                e.staged.iter().all(|s| w.discarded.contains(&s.token))
            }
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

/// A rolled-back or rejected transaction leaves no content in any tree,
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

/// Rule 7: under `FullLineage` a tree's ancestry reaches every commit that
/// landed in it; under `Squashed` none, and main's ancestry holds only
/// main's own commits.
fn history_setting(w: &mut World) -> Result<(), String> {
    let full = w.policies.history == HistoryPolicy::FullLineage;
    let repo = w.repo().to_path_buf();
    for (t, c) in w.landings.clone() {
        if !w.alive(t) {
            continue;
        }
        let head = w.head(t);
        let reach = w.reach.entry(head.clone()).or_insert_with(|| {
            git(&repo, &["rev-list", &head])
                .lines()
                .map(str::to_string)
                .collect()
        });
        if reach.contains(&c) != full {
            return Err(format!(
                "{t:?}'s ancestry reaches landed {c}: {}, under {:?}",
                !full, w.policies.history
            ));
        }
    }
    if !full && w.main_checked.as_deref() != Some(w.main_head.as_str()) {
        let range = match &w.main_checked {
            Some(prev) => format!("{prev}..{}", w.main_head),
            None => w.main_head.clone(),
        };
        let trees = git(
            &repo,
            &[
                "log",
                &range,
                "--format=%H %(trailers:key=Agentfs-Tree,valueonly)",
            ],
        );
        if let Some(l) = trees
            .lines()
            .find(|l| !l.is_empty() && !l.ends_with(" main"))
        {
            return Err(format!("Squashed: main's ancestry reaches {l}"));
        }
        w.main_checked = Some(w.main_head.clone());
    }
    Ok(())
}
