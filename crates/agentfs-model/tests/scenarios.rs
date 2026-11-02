//! One or more scenarios per rule of MODEL.md, named `r<N>_...` after the
//! rule they show. Every scenario checks every invariant after each step.
//! A test marked `#[ignore = "coherence finding N: ..."]` asserts what the
//! rules intend but do not deliver; run it with `--ignored`.

use agentfs_model::check::{check, check_converged};
use agentfs_model::content::Layout;
use agentfs_model::git::{git, with_reversed_dates};
use agentfs_model::world::{
    Client, CollisionScope, Direction, Entry, HistoryPolicy, Life, MergeRule, ModelError, Policies,
    Step, Tree, TxnOutcome, World,
};

const BOTH: [HistoryPolicy; 2] = [HistoryPolicy::FullLineage, HistoryPolicy::Squashed];

fn world(history: HistoryPolicy, collision: CollisionScope) -> World {
    World::new(Policies::new(history, collision))
}

/// Every invariant, now.
fn ok(w: &mut World) {
    if let Err(e) = check(w) {
        panic!("{e}\nlog:\n  {}", w.log.join("\n  "));
    }
}

/// One queued step, then every invariant.
fn next(w: &mut World, t: Tree) {
    w.step(&Step::Next(t));
    ok(w);
}

/// Every step until none is left, checking after each; then convergence.
fn drain(w: &mut World) {
    ok(w);
    while let Some(s) = w.pending().into_iter().next() {
        w.note_step(&s);
        w.step(&s);
        ok(w);
    }
    if let Err(e) = check_converged(w) {
        panic!("{e}\nlog:\n  {}", w.log.join("\n  "));
    }
}

fn m1() -> Client {
    Client::Main("m1".into())
}

fn has(w: &World, t: Tree, p: &str, token: &str) -> bool {
    w.read(t, p).contains(token)
}

fn show(w: &World, rev: &str, format: &str) -> String {
    git(w.repo(), &["log", "-1", &format!("--format={format}"), rev])
}

/// [`show`] on the commit as stored, without `Squashed`'s grafts.
fn stored(w: &World, rev: &str, format: &str) -> String {
    git(
        w.repo(),
        &[
            "--no-replace-objects",
            "log",
            "-1",
            &format!("--format={format}"),
            rev,
        ],
    )
}

/// The writers of main's first-parent commits, newest first.
fn main_writers(w: &World) -> Vec<String> {
    git(
        w.repo(),
        &[
            "log",
            "--first-parent",
            "--format=%(trailers:key=Agentfs-Writer,valueonly)",
            &w.main_head,
        ],
    )
    .lines()
    .filter(|l| !l.is_empty())
    .map(str::to_string)
    .collect()
}

// ── rule 1: like a Linux filesystem ─────────────────────────────────────

/// Rule 1: writes are never refused, not even on the same line at once;
/// each writer reads its own write at once; forks sync both ways.
#[test]
fn r1_writes_never_wait_and_forks_sync_both_ways() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let mine = w.save(&Client::Fork(f), "a", 0);
        let theirs = w.save(&m1(), "b", 0);
        let both = w.save(&m1(), "a", 1);
        ok(&mut w);
        assert!(has(&w, Tree::Fork(f), "a", &mine) && has(&w, Tree::Main, "b", &theirs));
        assert!(!has(&w, Tree::Main, "a", &mine), "landing is a queued step");
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &mine) && has(&w, Tree::Main, "a", &both));
        assert!(has(&w, Tree::Fork(f), "b", &theirs), "{history:?}");
    }
}

// ── rule 2: one landing queue per tree; no clock ────────────────────────

/// Rule 2: main's queue fixes the order its children's commits land in,
/// whatever order the scheduler visits the forks.
#[test]
fn r2_one_queue_fixes_the_landing_order() {
    let mut w = world(HistoryPolicy::Squashed, CollisionScope::Line);
    let (f, g) = (w.create_fork(Tree::Main), w.create_fork(Tree::Main));
    w.save(&Client::Fork(f), "a", 0);
    w.save(&Client::Fork(g), "b", 0);
    w.save(&Client::Fork(f), "c", 0);
    let order: Vec<String> = w
        .queue(Tree::Main)
        .iter()
        .map(|e| format!("{e:?}"))
        .collect();
    assert_eq!(order.len(), 3);
    for _ in 0..3 {
        next(&mut w, Tree::Main);
    }
    assert_eq!(main_writers(&w)[..3], ["c1", "c2", "c1"]);
    drain(&mut w);
}

/// Rule 2, "no clock is read anywhere": the same calls give the same
/// history, commit for commit, with git's dates pinned (`git.rs`); run
/// again with every commit dated before its parents, they decide the same
/// trees and collisions (an order read from commit dates would run
/// backwards); and the model's source names no clock.
#[test]
fn r2_no_clock_commit_dates_decide_nothing() {
    let run = || {
        let mut w = world(HistoryPolicy::FullLineage, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let g = w.create_fork(Tree::Fork(f));
        w.save(&Client::Fork(f), "a", 0);
        w.save(&Client::Fork(g), "c", 0);
        w.save(&m1(), "a", 0);
        next(&mut w, Tree::Fork(f));
        next(&mut w, Tree::Main);
        w.save(&Client::Fork(f), "a", 0);
        w.begin(f).unwrap();
        w.save(&Client::Fork(f), "b", 1);
        w.save(&m1(), "b", 1);
        w.commit_txn(f).unwrap();
        drain(&mut w);
        (
            git(
                w.repo(),
                &["for-each-ref", "--format=%(objectname) %(refname)"],
            ),
            w.fingerprint(),
        )
    };
    let pinned = run();
    assert_eq!(pinned, run());
    assert_eq!(pinned.1, with_reversed_dates(run).1);
    let src = concat!(
        include_str!("../src/world.rs"),
        include_str!("../src/check.rs"),
        include_str!("../src/content.rs"),
        include_str!("../src/git.rs")
    );
    for clock in ["SystemTime", "Instant", "chrono", "UNIX_EPOCH"] {
        assert!(!src.contains(clock), "the model reads a clock: {clock}");
    }
}

// ── rule 3: fast-forward, else line merge, the incoming change wins ────

/// Rule 3: an edit whose base is main's current version replaces it.
/// `FullLineage`: main's head becomes the fork's commit. `Squashed`: one
/// single-parent commit with that commit's tree, named for the sandbox,
/// whose graft names the fork's commit as carried.
#[test]
fn r3_fast_forward_when_the_base_is_current() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let tok = w.save(&Client::Fork(f), "a", 0);
        let fc = w.head(Tree::Fork(f));
        next(&mut w, Tree::Main);
        assert!(has(&w, Tree::Main, "a", &tok));
        match history {
            HistoryPolicy::FullLineage => {
                assert_eq!(w.main_head, fc);
                assert_eq!(w.fast_forwards, 1);
            }
            HistoryPolicy::Squashed => {
                assert_eq!(show(&w, &w.main_head, "%T"), show(&w, &fc, "%T"));
                assert_eq!(stored(&w, &w.main_head, "%P").split(' ').count(), 1);
                assert_eq!(
                    show(&w, &w.main_head, "%P").split(' ').nth(1),
                    Some(fc.as_str()),
                    "the graft names the carried commit"
                );
                assert_eq!(main_writers(&w)[0], "c1");
            }
        }
        drain(&mut w);
    }
}

/// Rule 3, a sandbox's landing into main: its concurrent same-line edit
/// wins in main ("If a sandbox pushes a conflicting change upstream, then
/// it should be applied"). The collision names main's lost line and the
/// line that overwrote it; main's losing version stays reachable. The
/// sync main queued into the fork when it wrote its line is processed
/// after the landing: it carries main's current version, so the fork's
/// edit, which already won in main, is never reverted in the fork.
#[test]
fn r3_landing_collision_the_fork_wins_in_the_parent() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let x = w.save(&Client::Fork(f), "a", 0);
        let fc = w.head(Tree::Fork(f));
        let m = w.save(&m1(), "a", 0);
        let mc = w.main_head.clone();
        next(&mut w, Tree::Main);
        assert!(has(&w, Tree::Main, "a", &x) && !has(&w, Tree::Main, "a", &m));
        let c = w.collisions.last().expect("a collision").clone();
        assert_eq!(
            (c.tree, c.direction, c.lost.clone(), c.won.clone()),
            (
                Tree::Main,
                Direction::Landing,
                vec![m.clone()],
                vec![x.clone()]
            )
        );
        assert_eq!(
            (c.winner.as_str(), c.loser.as_str()),
            (fc.as_str(), mc.as_str())
        );
        assert_eq!(git(w.repo(), &["rev-parse", &c.reference]), mc);
        assert_eq!(
            w.queue_len(Tree::Fork(f)),
            2,
            "{history:?}: two syncs queued"
        );
        next(&mut w, Tree::Fork(f)); // the sync queued when m was written
        assert!(has(&w, Tree::Fork(f), "a", &x), "{history:?}: stale sync");
        drain(&mut w);
        assert!(has(&w, Tree::Fork(f), "a", &x) && has(&w, Tree::Main, "a", &x));
    }
}

/// Rule 3, main's sync into a sandbox: main's concurrent same-line edit
/// wins in the sandbox ("main only wins when there is a collision from
/// upstream to a fork"). The sandbox's own edit is still queued for main:
/// it lands after, wins there, and comes back. The sandbox shows its
/// line, then main's, then its own again (coherence finding 4).
#[test]
fn r3_sync_collision_the_parent_wins_in_the_fork_until_the_fork_edit_lands() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let x = w.save(&Client::Fork(f), "a", 0);
        let m = w.save(&m1(), "a", 0);
        next(&mut w, Tree::Fork(f));
        assert!(has(&w, Tree::Fork(f), "a", &m) && !has(&w, Tree::Fork(f), "a", &x));
        let c = w.collisions.last().expect("a collision").clone();
        assert_eq!(
            (c.tree, c.direction, c.lost, c.won),
            (
                Tree::Fork(f),
                Direction::Sync,
                vec![x.clone()],
                vec![m.clone()]
            )
        );
        let head = w.head(Tree::Fork(f));
        assert_eq!(
            show(&w, &head, "%(trailers:key=Agentfs-Cause,valueonly)"),
            "Sync"
        );
        next(&mut w, Tree::Main);
        assert!(
            has(&w, Tree::Main, "a", &x),
            "{history:?}: the landing wins"
        );
        assert!(
            w.collisions
                .iter()
                .any(|c| c.tree == Tree::Main && c.lost == [m.clone()])
        );
        drain(&mut w);
        assert!(has(&w, Tree::Fork(f), "a", &x), "{history:?}");
    }
}

/// Coherence finding 4: the intent "no flip-flop" read strictly. The same
/// calls as above: the sandbox should never show main's line over its own
/// edit that is on its way to win in main. Rule 3 as stated requires it
/// to, for as long as the edit is in flight.
#[test]
#[ignore = "coherence finding 4: parent-wins-in-the-fork is undone when the fork's edit lands, so the fork flips"]
fn finding_4_a_fork_never_flips_back_to_its_own_edit() {
    let mut w = world(HistoryPolicy::FullLineage, CollisionScope::Line);
    let f = w.create_fork(Tree::Main);
    let x = w.save(&Client::Fork(f), "a", 0);
    w.save(&m1(), "a", 0);
    next(&mut w, Tree::Fork(f));
    assert!(
        has(&w, Tree::Fork(f), "a", &x),
        "the fork flipped to main's line"
    );
}

/// Rule 3 with crossing merges: the sandbox takes main's line while main
/// takes the sandbox's. The sandbox then writes y over main's line. The
/// next sync carries main's current version (the sandbox's older x), but
/// y's writer saw x, so y stays; y then lands and wins in main without a
/// collision.
#[test]
fn r3_crossing_merges_keep_a_newer_edit() {
    let (w, y) = crossing(MergeRule::PerLine);
    assert!(has(&w, Tree::Main, "a", &y));
}

/// Rule 3, "a sync carries the parent's current version": a line the
/// sandbox already received from its parent, and then lost to a
/// concurrent line landing from a sandbox inside it, is not delivered
/// again by the next sync. Main's p syncs into F; G's concurrent g lands
/// in F and wins there; main then changes another file. The sync of that
/// change does not bring p back over g: F holds the commit that set p in
/// main. g then lands in main and wins there.
#[test]
fn r3_a_sync_never_brings_back_a_line_the_fork_already_took() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let p = w.save(&m1(), "a", 0);
        let g = w.create_fork(Tree::Fork(f));
        let gl = w.save(&Client::Fork(g), "a", 0);
        next(&mut w, Tree::Fork(f)); // main's p into F
        assert!(has(&w, Tree::Fork(f), "a", &p));
        next(&mut w, Tree::Fork(f)); // G's g lands in F and wins
        assert!(has(&w, Tree::Fork(f), "a", &gl));
        let q = w.save(&m1(), "b", 0);
        while w.queue(Tree::Fork(f)).first() == Some(&Entry::Sync) {
            next(&mut w, Tree::Fork(f));
        }
        assert!(has(&w, Tree::Fork(f), "b", &q));
        assert!(has(&w, Tree::Fork(f), "a", &gl), "{history:?}: p came back");
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &gl));
    }
}

/// Coherence finding 1: the same calls with git's own three-way merge.
/// The crossing merges give two merge bases; git merges them leaving
/// conflict markers in the base, so y "collides" with x, which y's writer
/// saw, and the incoming x reverts it: a stale revert.
#[test]
#[ignore = "coherence finding 1: git's own merge base of crossing syncs and landings lets a stale line revert a newer one"]
fn finding_1_git_merge_base_on_crossing_merges() {
    crossing(MergeRule::GitMergeBase);
}

fn crossing(merge: MergeRule) -> (World, String) {
    let mut w = World::new(Policies {
        merge,
        ..Policies::new(HistoryPolicy::FullLineage, CollisionScope::Line)
    });
    let f = w.create_fork(Tree::Main);
    let x = w.save(&Client::Fork(f), "a", 0);
    let m = w.save(&m1(), "a", 0);
    next(&mut w, Tree::Fork(f)); // sync: main's m wins in F
    next(&mut w, Tree::Main); // landing: F's x wins in main; crossed
    assert!(has(&w, Tree::Fork(f), "a", &m) && has(&w, Tree::Main, "a", &x));
    let y = w.save(&Client::Fork(f), "a", 0); // written over m
    next(&mut w, Tree::Fork(f)); // main's current version: x
    assert!(
        has(&w, Tree::Fork(f), "a", &y),
        "{merge:?}: the newer line stays"
    );
    drain(&mut w);
    (w, y)
}

/// Rule 3's design choice, per line. One sandbox commit (a transaction's)
/// changes lines 0 and 2 of a; main changed line 0 and, cleanly, line 1.
/// The sandbox's lines win; main's clean line 1 stays.
#[test]
fn r3_collision_scope_per_line_keeps_clean_lines() {
    for history in BOTH {
        let (w, [x, y, m, k]) = clean_lines(history, CollisionScope::Line);
        assert!(has(&w, Tree::Main, "a", &x) && has(&w, Tree::Main, "a", &y));
        assert!(has(&w, Tree::Main, "a", &k), "{history:?}: the clean line");
        let lost: Vec<String> = w.collisions.iter().flat_map(|c| c.lost.clone()).collect();
        assert!(lost.contains(&m) && !lost.contains(&k));
    }
}

/// Coherence finding 3: the same calls per file. The sandbox's whole file
/// wins, so main's clean line 1, written after the sandbox's base, goes
/// back to the sandbox's older (unwritten) line: a stale revert, and a
/// "collision" between lines that never collided.
#[test]
#[ignore = "coherence finding 3: per file, the incoming file reverts the tree's newer clean lines"]
fn finding_3_per_file_keeps_clean_lines() {
    for history in BOTH {
        clean_lines(history, CollisionScope::File);
    }
}

fn clean_lines(history: HistoryPolicy, scope: CollisionScope) -> (World, [String; 4]) {
    let mut w = world(history, scope);
    let f = w.create_fork(Tree::Main);
    w.begin(f).unwrap();
    let x = w.save(&Client::Fork(f), "a", 0);
    let y = w.save(&Client::Fork(f), "a", 2);
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
    let m = w.save(&m1(), "a", 0);
    let k = w.save(&m1(), "a", 1);
    drain(&mut w);
    (w, [x, y, m, k])
}

/// Rule 3's design choice, per line, in the sync direction: two sandbox
/// commits (line 0, then line 2); main changes line 0 and, cleanly, line 1.
/// Main's sync collides on line 0 only: the sandbox keeps its line 2 and
/// takes main's line 1 in that same sync, and everything converges.
#[test]
fn r3_collision_scope_per_line_sync_keeps_the_clean_line() {
    let mut w = world(HistoryPolicy::FullLineage, CollisionScope::Line);
    let f = w.create_fork(Tree::Main);
    let x = w.save(&Client::Fork(f), "a", 0);
    let y = w.save(&Client::Fork(f), "a", 2);
    w.save(&m1(), "a", 0);
    let k = w.save(&m1(), "a", 1);
    next(&mut w, Tree::Fork(f));
    assert!(has(&w, Tree::Fork(f), "a", &y));
    assert!(has(&w, Tree::Fork(f), "a", &k), "main's clean line");
    drain(&mut w);
    assert!(has(&w, Tree::Main, "a", &x) && has(&w, Tree::Main, "a", &y));
    assert!(has(&w, Tree::Main, "a", &k));
}

/// Rule 3, per line, on neighbouring lines (one git hunk): the sandbox's
/// line 1 and main's line 0 both stay, in both trees.
#[test]
fn r3_neighbouring_lines_merge_per_line() {
    let (w, [k, m]) = neighbours(MergeRule::PerLine);
    for t in [Tree::Main, Tree::Fork(1)] {
        assert!(has(&w, t, "a", &k) && has(&w, t, "a", &m), "{t:?}");
    }
}

/// Coherence finding 6: the same calls with git's own merge taking the
/// incoming side of a conflicting hunk (`-X theirs`). Neighbouring changes
/// form one git hunk, so main's sync takes main's whole hunk and reverts
/// the sandbox's newer line 1 to main's older (unwritten) one. "Per hunk"
/// in git's sense is finding 3 in small; the model's rule is per line.
#[test]
#[ignore = "coherence finding 6: git's hunk-level -X theirs on neighbouring lines reverts the tree's newer line"]
fn finding_6_git_hunks_on_neighbouring_lines() {
    neighbours(MergeRule::GitMergeBase);
}

fn neighbours(merge: MergeRule) -> (World, [String; 2]) {
    let mut w = World::new(Policies {
        merge,
        layout: Layout::Adjacent,
        ..Policies::default()
    });
    let f = w.create_fork(Tree::Main);
    let k = w.save(&Client::Fork(f), "a", 1);
    let m = w.save(&m1(), "a", 0);
    next(&mut w, Tree::Fork(f)); // main's sync: m beside k
    assert!(has(&w, Tree::Fork(f), "a", &k) && has(&w, Tree::Fork(f), "a", &m));
    drain(&mut w);
    (w, [k, m])
}

/// Rule 3, a loss decided upstream: sibling G holds main's m (synced in,
/// beside G's own work) when F's concurrent x lands in main and wins. The
/// next sync brings x into G as a plain update: the collision is main's,
/// recorded once there, not again in every sibling holding m.
#[test]
fn r3_a_loss_decided_upstream_is_recorded_once() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let (f, g) = (w.create_fork(Tree::Main), w.create_fork(Tree::Main));
        w.save(&Client::Fork(g), "b", 0);
        let x = w.save(&Client::Fork(f), "a", 0);
        let m = w.save(&m1(), "a", 0);
        while w.queue(Tree::Fork(g)).first() == Some(&Entry::Sync) {
            next(&mut w, Tree::Fork(g));
        }
        assert!(has(&w, Tree::Fork(g), "a", &m));
        next(&mut w, Tree::Main); // G's b lands
        next(&mut w, Tree::Main); // F's x lands, wins over m
        drain(&mut w);
        assert!(has(&w, Tree::Fork(g), "a", &x), "{history:?}");
        let records: Vec<Tree> = w
            .collisions
            .iter()
            .filter(|c| c.lost.contains(&m))
            .map(|c| c.tree)
            .collect();
        assert_eq!(records, [Tree::Main], "{history:?}");
    }
}

/// Rule 7, `Squashed`: the fork's x lands as main's own single-parent
/// commit; the fork writes y over x; main's version, x, syncs back. The
/// graft beside main's history names the fork commit that carried x, so
/// git sees y's writer saw x, and y stays. (Without the graft, x reverted
/// y in the fork: coherence finding 2, resolved.)
#[test]
fn r7_squashed_sync_never_undoes_the_forks_newer_line() {
    let mut w = world(HistoryPolicy::Squashed, CollisionScope::Line);
    let f = w.create_fork(Tree::Main);
    w.save(&Client::Fork(f), "a", 0);
    next(&mut w, Tree::Main); // the fork's x lands
    let y = w.save(&Client::Fork(f), "a", 0); // y over x
    next(&mut w, Tree::Fork(f)); // main's version, x, syncs back
    assert!(has(&w, Tree::Fork(f), "a", &y));
    drain(&mut w);
    assert!(has(&w, Tree::Main, "a", &y));
    assert!(w.collisions.is_empty());
}

// ── rule 4: nothing waits; nested forks ─────────────────────────────────

/// Rule 4: a commit that collides does not hold back the next one; every
/// step takes its queue's head; the queues drain.
#[test]
fn r4_nothing_waits_behind_a_collision() {
    let mut w = world(HistoryPolicy::FullLineage, CollisionScope::Line);
    let f = w.create_fork(Tree::Main);
    w.save(&Client::Fork(f), "a", 0);
    let later = w.save(&Client::Fork(f), "b", 0);
    w.save(&m1(), "a", 0);
    assert_eq!(w.queue_len(Tree::Main), 2);
    next(&mut w, Tree::Main);
    assert_eq!(w.collisions.len(), 1);
    assert_eq!(w.queue_len(Tree::Main), 1);
    next(&mut w, Tree::Main);
    assert!(has(&w, Tree::Main, "b", &later));
    drain(&mut w);
}

/// Rule 4, nested forks: G over F over main, the same rules with F as
/// G's parent. G's landing wins in F over F's own line; F's landing
/// carries G's line into main, where it wins over main's concurrent line.
/// A clean change of G's reaches main named for G. Everything converges.
#[test]
fn r4_nested_forks_follow_the_same_rules() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let g = w.create_fork(Tree::Fork(f));
        let (cf, cg) = (Client::Fork(f), Client::Fork(g));
        let ga = w.save(&cg, "a", 0);
        let fa = w.save(&cf, "a", 0);
        next(&mut w, Tree::Fork(f)); // G's a lands in F: G wins.
        assert!(has(&w, Tree::Fork(f), "a", &ga) && !has(&w, Tree::Fork(f), "a", &fa));
        assert!(w.collisions.iter().any(|c| c.tree == Tree::Fork(f)
            && c.direction == Direction::Landing
            && c.lost == [fa.clone()]));
        let gb = w.save(&cg, "b", 0);
        let mb = w.save(&m1(), "b", 0);
        let gc = w.save(&cg, "c", 0);
        drain(&mut w);
        assert!(has(&w, Tree::Main, "b", &gb) && !has(&w, Tree::Main, "b", &mb));
        assert!(has(&w, Tree::Main, "c", &gc) && has(&w, Tree::Main, "a", &ga));
        assert!(
            w.collisions
                .iter()
                .any(|c| c.tree == Tree::Main && c.lost == [mb.clone()])
        );
        let c_commit = git(
            w.repo(),
            &[
                "log",
                "--format=%H",
                "-S",
                &gc,
                "--first-parent",
                &w.main_head,
            ],
        );
        let c_commit = c_commit.lines().last().expect("a main commit adds G's c");
        assert_eq!(w.writer_of_commit(c_commit), "c2", "{history:?}");
        w.end_client(g);
        w.end_client(f);
        drain(&mut w);
        assert_eq!(w.forks[&f].life, Life::Dropped);
    }
}

// ── rule 5: transactions ────────────────────────────────────────────────

/// Rule 5, a clean commit: the program reads its own staged edits on a
/// frozen snapshot while the fork keeps receiving syncs; the commit lands
/// in main as ONE commit named for the client.
#[test]
fn r5_transaction_commits_clean() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let c = Client::Fork(f);
        w.begin(f).unwrap();
        assert_eq!(w.begin(f), Err(ModelError::InTransaction));
        let sa = w.save(&c, "a", 0);
        let sb = w.save(&c, "b", 0);
        let m = w.save(&m1(), "c", 0);
        drain(&mut w);
        assert!(w.client_reads(&c, "a").contains(&sa));
        assert!(
            !has(&w, Tree::Fork(f), "a", &sa),
            "the fork's tree lacks it"
        );
        assert!(has(&w, Tree::Fork(f), "c", &m), "the fork got the sync");
        assert!(
            !w.client_reads(&c, "c").contains(&m),
            "the snapshot is frozen"
        );
        let before = main_writers(&w).len();
        assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
        assert!(w.client_reads(&c, "c").contains(&m));
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &sa) && has(&w, Tree::Main, "b", &sb));
        assert_eq!(
            main_writers(&w).len(),
            before + 1,
            "{history:?}: one commit"
        );
        assert_eq!(main_writers(&w)[0], "c1");
    }
}

/// Rule 5, a commit meeting a concurrent line: main's m on a line the
/// transaction changed has already synced into the sandbox. The commit is
/// the incoming change into the sandbox, so it is never refused and its
/// line wins there; the collision is recorded, naming m. Then it lands in
/// main whole and wins there too.
#[test]
fn r5_transaction_commit_wins_over_a_concurrent_line() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let c = Client::Fork(f);
        w.begin(f).unwrap();
        let s = w.save(&c, "a", 0);
        let s2 = w.save(&c, "b", 0);
        let m = w.save(&m1(), "a", 0);
        drain(&mut w);
        assert!(has(&w, Tree::Fork(f), "a", &m));
        assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
        ok(&mut w);
        assert!(w.client_reads(&c, "a").contains(&s) && w.client_reads(&c, "b").contains(&s2));
        let col = w.collisions.last().expect("a collision").clone();
        assert_eq!(
            (col.tree, col.direction, col.lost, col.won),
            (
                Tree::Fork(f),
                Direction::Transaction,
                vec![m.clone()],
                vec![s.clone()]
            )
        );
        let refs = git(
            w.repo(),
            &["for-each-ref", "--format=%(refname)", "refs/heads/txn"],
        );
        assert!(refs.is_empty());
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &s) && has(&w, Tree::Main, "b", &s2));
    }
}

/// Rule 5, no race: the same calls as above and as
/// [`r5_a_committed_transaction_then_follows_rule_3`], where main's sync
/// arrives after the commit, end with the same trees. Whether the sync
/// comes before or after the commit decides nothing.
#[test]
fn r5_sync_before_or_after_the_commit_ends_the_same() {
    for history in BOTH {
        let run = |sync_first: bool| {
            let mut w = world(history, CollisionScope::Line);
            let f = w.create_fork(Tree::Main);
            w.begin(f).unwrap();
            w.save(&Client::Fork(f), "a", 0);
            w.save(&Client::Fork(f), "b", 0);
            w.save(&m1(), "a", 0);
            if sync_first {
                drain(&mut w);
            }
            assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
            drain(&mut w);
            ["a", "b"].map(|p| w.read(Tree::Main, p))
        };
        assert_eq!(run(true), run(false), "{history:?}");
    }
}

/// Rule 5, rollback and client death: the branch is dropped, nothing of it
/// shows anywhere; the fork's own commits from before still land.
#[test]
fn r5_transaction_rollback_and_client_death_leave_nothing() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let c = Client::Fork(f);
        let kept = w.save(&c, "c", 0);
        w.begin(f).unwrap();
        let gone = w.save(&c, "a", 0);
        w.rollback(f).unwrap();
        ok(&mut w);
        assert!(!w.client_reads(&c, "a").contains(&gone));
        w.begin(f).unwrap();
        let died = w.save(&c, "b", 0);
        w.end_client(f);
        ok(&mut w);
        drain(&mut w);
        assert!(w.discarded.contains(&gone) && w.discarded.contains(&died));
        assert!(has(&w, Tree::Main, "c", &kept));
        assert_eq!(w.forks[&f].life, Life::Dropped);
    }
}

/// Rule 5 then rule 3: a commit is checked against the fork's head at that
/// moment only. A sync of main's concurrent line, queued before the commit
/// and processed after, wins its line in the fork (rule 3), so the fork
/// briefly shows part of the committed transaction; the transaction then
/// lands in main whole, wins there, and comes back (coherence finding 4).
#[test]
fn r5_a_committed_transaction_then_follows_rule_3() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        w.begin(f).unwrap();
        let sa = w.save(&Client::Fork(f), "a", 0);
        let sb = w.save(&Client::Fork(f), "b", 0);
        let m = w.save(&m1(), "a", 0); // queued into F, not yet synced
        assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
        next(&mut w, Tree::Fork(f));
        assert!(has(&w, Tree::Fork(f), "a", &m) && has(&w, Tree::Fork(f), "b", &sb));
        drain(&mut w);
        for t in [Tree::Main, Tree::Fork(f)] {
            assert!(
                has(&w, t, "a", &sa) && has(&w, t, "b", &sb),
                "{history:?} {t:?}"
            );
        }
    }
}

// ── rule 6: attribution ─────────────────────────────────────────────────

/// Rule 6: a sync is a merge commit labelled `Sync` with the parent's
/// commit as second parent; it originates nothing (the invariant checks
/// every commit: one writer each; a non-merge commit carries only its
/// writer's edits; a merge adds only what a non-first parent holds).
#[test]
fn r6_a_sync_is_a_merge_with_the_parent_second() {
    let mut w = world(HistoryPolicy::Squashed, CollisionScope::Line);
    let f = w.create_fork(Tree::Main);
    w.save(&Client::Fork(f), "a", 0);
    w.save(&m1(), "b", 0);
    let main = w.main_head.clone();
    next(&mut w, Tree::Fork(f));
    let head = w.head(Tree::Fork(f));
    let parents = show(&w, &head, "%P");
    assert_eq!(parents.split(' ').nth(1), Some(main.as_str()));
    assert_eq!(
        show(&w, &head, "%(trailers:key=Agentfs-Cause,valueonly)"),
        "Sync"
    );
    drain(&mut w);
}

// ── rule 7: the history setting ─────────────────────────────────────────

/// Rule 7: `FullLineage` main's history reaches the fork's commits;
/// `Squashed` holds only main's own, and its grafts keep the fork's commits
/// in lineage. Either way a collision's losing version stays reachable
/// under its ref after the fork is dropped.
#[test]
fn r7_history_setting_and_losers_stay_reachable() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        w.save(&Client::Fork(f), "a", 0);
        let fc = w.head(Tree::Fork(f));
        let m = w.save(&m1(), "a", 0);
        w.end_client(f);
        drain(&mut w);
        assert_eq!(w.forks[&f].life, Life::Dropped);
        assert_eq!(
            w.in_history(&fc, &w.main_head),
            history == HistoryPolicy::FullLineage
        );
        assert!(w.is_ancestor(&fc, &w.main_head), "{history:?}: lineage");
        let c = w
            .collisions
            .iter()
            .find(|c| c.lost == [m.clone()])
            .expect("m lost");
        let at = git(w.repo(), &["rev-parse", &c.reference]);
        assert!(w.file(&at, "a").contains(&m), "{history:?}");
    }
}

// ── convergence ─────────────────────────────────────────────────────────

/// When writes stop and the queues drain, every fork equals main
/// (`check_converged`), here with siblings, a nested fork and collisions
/// in both directions.
#[test]
fn convergence_when_writes_stop() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let g = w.create_fork(Tree::Main);
        let h = w.create_fork(Tree::Fork(f));
        for (i, c) in [Client::Fork(f), Client::Fork(g), Client::Fork(h), m1()]
            .iter()
            .enumerate()
        {
            w.save(c, "a", 0);
            w.save(c, "b", i % 2);
        }
        next(&mut w, Tree::Fork(g));
        next(&mut w, Tree::Main);
        drain(&mut w);
        assert!(!w.collisions.is_empty());
    }
}
