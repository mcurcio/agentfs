//! One or more scenarios per rule of MODEL.md, named `r<N>_...` after the
//! rule they show. Every scenario checks every invariant after each step.
//! A test marked `#[ignore = "coherence finding N: ..."]` asserts what the
//! rules intend but do not deliver; run it with `--ignored`.

use agentfs_model::check::{check, check_converged, step_checked};
use agentfs_model::content::{BLANK, Content, Layout, Line, State};
use agentfs_model::git::{git, with_reversed_dates};
use agentfs_model::world::{
    Client, CollisionScope, Direction, HistoryPolicy, Life, MergeRule, ModelError, Overwritten,
    Policies, Step, Tree, TxnOutcome, World, Write,
};
use std::collections::BTreeSet;

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

/// One step ([`step_checked`]), then every invariant.
fn take(w: &mut World, s: &Step) {
    if let Err(e) = step_checked(w, s) {
        panic!("{e}\nlog:\n  {}", w.log.join("\n  "));
    }
    ok(w);
}

/// One queued change taken whole (its take begins and ends: rule 9),
/// checking after each step.
fn next(w: &mut World, t: Tree) {
    take(w, &Step::Next(t));
    assert!(w.waits().get(&t).is_none(), "{t:?}'s queue waits");
    take(w, &Step::Finish(t));
}

/// Every step until none is left, checking after each; then convergence.
fn drain(w: &mut World) {
    ok(w);
    while let Some(s) = w.pending().into_iter().next() {
        w.note_step(&s);
        take(w, &s);
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
/// whatever order the scheduler visits the forks. (Under `Squashed` a
/// fork's waiting saves land together: `r7_squashed_saves_land_as_one_changeset`.)
#[test]
fn r2_one_queue_fixes_the_landing_order() {
    let mut w = world(HistoryPolicy::FullLineage, CollisionScope::Line);
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

// ── rule 3: the parent's queue decides; the parent's version wins ─────

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

/// Rule 3, a sandbox's landing into main, the one place their collision is
/// decided: main's concurrent same-line line wins ("i think the updated
/// main should win in a conflict"). The collision names the sandbox's lost
/// line and main's line that won; the sandbox's losing commit stays
/// reachable under the collision's ref. The sync main queued into the fork
/// when it wrote its line is processed after the landing: it carries
/// main's decided version, so the fork takes m.
#[test]
fn r3_landing_collision_the_parent_wins() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let x = w.save(&Client::Fork(f), "a", 0);
        let fc = w.head(Tree::Fork(f));
        let m = w.save(&m1(), "a", 0);
        let mc = w.main_head.clone();
        next(&mut w, Tree::Main);
        assert!(has(&w, Tree::Main, "a", &m) && !has(&w, Tree::Main, "a", &x));
        let c = w.collisions.last().expect("a collision").clone();
        assert_eq!(
            (c.tree, c.direction, c.lost.clone(), c.won.clone()),
            (
                Tree::Main,
                Direction::Landing,
                vec![x.clone()],
                vec![m.clone()]
            )
        );
        assert_eq!(
            (c.winner.as_str(), c.loser.as_str()),
            (mc.as_str(), fc.as_str())
        );
        assert_eq!(git(w.repo(), &["rev-parse", &c.reference]), fc);
        assert!(w.file(&c.reference, "a").contains(&x), "the loser is kept");
        assert_eq!(
            w.queue_len(Tree::Fork(f)),
            2,
            "{history:?}: two syncs queued"
        );
        next(&mut w, Tree::Fork(f)); // the sync queued when m was written
        assert!(has(&w, Tree::Fork(f), "a", &m), "{history:?}: decided");
        drain(&mut w);
        assert!(has(&w, Tree::Fork(f), "a", &m) && has(&w, Tree::Main, "a", &m));
        assert_eq!(w.collisions.len(), 1, "{history:?}: recorded once");
    }
}

/// Rule 3, no flip-flop (coherence finding 4, resolved): main's sync
/// reaches the sandbox before the sandbox's own concurrent edit lands. The
/// sync decides nothing, so the sandbox keeps its x; main's queue then
/// decides, main's m wins, and the sandbox takes m. Its line changes once,
/// x then m, never x, m, x (the `no_flicker` invariant checks this in every
/// run).
#[test]
fn r3_a_fork_never_flips_back_to_its_own_edit() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let x = w.save(&Client::Fork(f), "a", 0);
        let m = w.save(&m1(), "a", 0);
        next(&mut w, Tree::Fork(f)); // main's sync, before x lands
        assert!(
            has(&w, Tree::Fork(f), "a", &x),
            "{history:?}: the fork decided"
        );
        assert!(w.collisions.is_empty(), "a sync records nothing");
        let head = w.head(Tree::Fork(f));
        assert_eq!(
            show(&w, &head, "%(trailers:key=Agentfs-Cause,valueonly)"),
            "Sync"
        );
        next(&mut w, Tree::Main); // main's queue decides
        assert!(has(&w, Tree::Main, "a", &m) && !has(&w, Tree::Main, "a", &x));
        drain(&mut w);
        assert!(has(&w, Tree::Fork(f), "a", &m), "{history:?}");
        let records: Vec<(Tree, Vec<String>)> = w
            .collisions
            .iter()
            .map(|c| (c.tree, c.lost.clone()))
            .collect();
        assert_eq!(records, [(Tree::Main, vec![x.clone()])], "{history:?}");
    }
}

/// Rule 3, an edit on a line the parent has not decided: the sandbox
/// writes y over its own x while main's concurrent m waits for x's
/// landing. y never saw m, so it collides with m too, and main's m wins
/// again. The sync between them showed the sandbox nothing of m: what a
/// sandbox wrote over is read per line, not from which commits its version
/// merged.
#[test]
fn r3_an_edit_over_an_undecided_line_still_collides() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let x = w.save(&Client::Fork(f), "a", 0);
        let m = w.save(&m1(), "a", 0);
        next(&mut w, Tree::Fork(f)); // a Sync merge that keeps x
        let y = w.save(&Client::Fork(f), "a", 0); // over x, not m
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &m), "{history:?}");
        // Under `Squashed` x and y land as one changeset, carrying y only.
        let lost: Vec<String> = w.collisions.iter().flat_map(|c| c.lost.clone()).collect();
        let expect = match history {
            HistoryPolicy::FullLineage => vec![x.clone(), y.clone()],
            HistoryPolicy::Squashed => vec![y.clone()],
        };
        assert_eq!(lost, expect, "{history:?}");
    }
}

/// Rule 3, an edit made on the parent's current version replaces it: the
/// sandbox's x loses to main's m and the sandbox takes m; it then writes
/// y over m, and y replaces m in main without a collision.
#[test]
fn r3_an_edit_over_the_decided_line_replaces_it() {
    let (w, y) = crossing(MergeRule::PerLine);
    assert!(has(&w, Tree::Main, "a", &y));
    assert_eq!(w.collisions.len(), 1);
}

/// Rule 3c, a save after the sync (owner, 2026-11-03: "if the program
/// saves after the sync has landed, then arguably the sandbox was the last
/// writer and its change should take effect"). A save's first parent is
/// the tree's head when it is made, so whatever the tree held then counts
/// as seen. The sandbox has its own unlanded line b, so main's m arrives
/// as a sync commit (a `Sync` merge naming `agentfs`, main's commit
/// second); the sandbox's y on m's line is written over m and replaces it
/// in main by lineage, with no collision.
#[test]
fn r3_a_save_after_the_sync_wins_upstream_by_lineage() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        w.save(&Client::Fork(f), "b", 0);
        let m = w.save(&m1(), "a", 0);
        next(&mut w, Tree::Fork(f)); // the sync lands in the sandbox
        let synced = w.head(Tree::Fork(f));
        assert_eq!(
            show(&w, &synced, "%(trailers:key=Agentfs-Cause,valueonly)"),
            "Sync",
            "{history:?}: a sync commit, the sandbox has an unlanded line"
        );
        assert!(has(&w, Tree::Fork(f), "a", &m));
        let y = w.save(&Client::Fork(f), "a", 0);
        assert_eq!(
            show(&w, &w.head(Tree::Fork(f)), "%P").split(' ').next(),
            Some(synced.as_str())
        );
        drain(&mut w);
        assert!(
            has(&w, Tree::Main, "a", &y),
            "{history:?}: the sandbox's later save wins"
        );
        assert!(
            w.collisions.is_empty(),
            "{history:?}: written over m, no collision"
        );
    }
}

/// Rule 3b, a save before the sync arrives: main writes m first, but the
/// sandbox saves x on the same line while main's sync still waits in the
/// sandbox's queue. x's first parent is the head the sandbox held, without
/// m, so x and m are concurrent whatever order they were written in, and
/// main's m wins at main's queue. The sandbox shows x, then m, never x
/// again; Replication, a step of its own, then pushes x to the shared
/// remote, naming its writer.
#[test]
fn r3_a_save_before_the_sync_arrives_loses_to_the_parent() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let m = w.save(&m1(), "a", 0);
        assert_eq!(w.queue_len(Tree::Fork(f)), 1, "main's sync waits");
        let x = w.save(&Client::Fork(f), "a", 0);
        let xc = w.head(Tree::Fork(f));
        ok(&mut w);
        assert!(
            has(&w, Tree::Fork(f), "a", &x),
            "the sandbox reads its own write"
        );
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &m) && has(&w, Tree::Fork(f), "a", &m));
        let lost: Vec<(Tree, Vec<String>, Vec<String>)> = w
            .collisions
            .iter()
            .map(|c| (c.tree, c.lost.clone(), c.won.clone()))
            .collect();
        assert_eq!(
            lost,
            [(Tree::Main, vec![x.clone()], vec![m.clone()])],
            "{history:?}"
        );
        assert!(
            !w.pending().contains(&Step::Replicate),
            "{history:?}: drained, Replication included"
        );
        let pushed = git(w.remote(), &["rev-parse", &w.collisions[0].reference]);
        assert_eq!(pushed, xc, "{history:?}: the loser is on the shared remote");
    }
}

/// Rule 3, "a sync carries the parent's current version, never a stale
/// change": main writes m1 then m2 over it, queueing two syncs. The first
/// sync brings m2 directly; the sandbox never shows m1.
#[test]
fn r3_a_sync_carries_the_parents_current_version() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        w.save(&Client::Fork(f), "b", 0);
        let m1_ = w.save(&m1(), "a", 0);
        let m2 = w.save(&m1(), "a", 0);
        assert_eq!(w.queue_len(Tree::Fork(f)), 2);
        next(&mut w, Tree::Fork(f));
        assert!(has(&w, Tree::Fork(f), "a", &m2) && !has(&w, Tree::Fork(f), "a", &m1_));
        let head = w.head(Tree::Fork(f));
        next(&mut w, Tree::Fork(f));
        assert_eq!(w.head(Tree::Fork(f)), head, "{history:?}: nothing new");
        drain(&mut w);
    }
}

/// Coherence finding 1: the same calls as
/// [`r3_an_edit_over_the_decided_line_replaces_it`] with git's own
/// three-way merge for the sync, a conflicting hunk keeping the fork's
/// side. After x lands and loses, the sandbox and main have crossed (each
/// merged the other), so git finds two merge bases, merges them leaving
/// conflict markers in the base, and sees x against m as a conflict: the
/// sandbox keeps x, which main already decided against.
#[test]
#[ignore = "coherence finding 1: git's own merge base of crossing syncs and landings keeps a fork line the parent already decided against"]
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
    next(&mut w, Tree::Fork(f)); // sync: x is not decided, so F keeps it
    next(&mut w, Tree::Main); // landing: main's m wins; crossed
    assert!(has(&w, Tree::Fork(f), "a", &x) && has(&w, Tree::Main, "a", &m));
    next(&mut w, Tree::Fork(f)); // main's decided version
    assert!(
        has(&w, Tree::Fork(f), "a", &m),
        "{merge:?}: the fork takes main's decision"
    );
    let y = w.save(&Client::Fork(f), "a", 0); // written over m
    drain(&mut w);
    (w, y)
}

/// Rule 3's design choice, per line. One sandbox commit (a transaction's)
/// changes lines 0 and 2 of a; main changed line 0 and, cleanly, line 1.
/// Main's line 0 wins; the sandbox's line 2 applies; main's clean line 1
/// stays.
#[test]
fn r3_collision_scope_per_line_keeps_clean_lines() {
    for history in BOTH {
        let (w, [x, y, m, k]) = clean_lines(history, CollisionScope::Line);
        assert!(has(&w, Tree::Main, "a", &m) && has(&w, Tree::Main, "a", &y));
        assert!(has(&w, Tree::Main, "a", &k), "{history:?}: the clean line");
        let lost: Vec<String> = w.collisions.iter().flat_map(|c| c.lost.clone()).collect();
        assert_eq!(lost, [x], "{history:?}");
    }
}

/// Coherence finding 3: the same calls per file. Main's whole file wins,
/// so the sandbox's clean line 2 is dropped too; the sandbox then takes
/// main's file, and its newer line 2 goes back to main's older (unwritten)
/// one: a stale revert.
#[test]
#[ignore = "coherence finding 3: per file, the parent's winning file reverts the fork's newer clean lines"]
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
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::clean()));
    let m = w.save(&m1(), "a", 0);
    let k = w.save(&m1(), "a", 1);
    drain(&mut w);
    (w, [x, y, m, k])
}

/// Rule 3's design choice, per line, in the sync direction: two sandbox
/// commits (line 0, then line 2); main changes line 0 and, cleanly, line 1.
/// The sync takes main's line 1 and keeps the sandbox's lines 0 and 2,
/// which main's queue has not decided yet. Then main's line 0 wins, line 2
/// applies, and everything converges.
#[test]
fn r3_collision_scope_per_line_sync_keeps_the_clean_line() {
    let mut w = world(HistoryPolicy::FullLineage, CollisionScope::Line);
    let f = w.create_fork(Tree::Main);
    let x = w.save(&Client::Fork(f), "a", 0);
    let y = w.save(&Client::Fork(f), "a", 2);
    let m = w.save(&m1(), "a", 0);
    let k = w.save(&m1(), "a", 1);
    next(&mut w, Tree::Fork(f));
    assert!(has(&w, Tree::Fork(f), "a", &x) && has(&w, Tree::Fork(f), "a", &y));
    assert!(has(&w, Tree::Fork(f), "a", &k), "main's clean line");
    drain(&mut w);
    assert!(has(&w, Tree::Main, "a", &m) && has(&w, Tree::Main, "a", &y));
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

/// Coherence finding 6: the same calls with git's own merge for the sync,
/// a conflicting hunk keeping the sandbox's side (`-X ours`). Neighbouring
/// changes form one git hunk, so the sync keeps the sandbox's whole hunk
/// and withholds main's line 0, which main had decided. "Per hunk" in
/// git's sense is finding 3 in small; the model's rule is per line.
#[test]
#[ignore = "coherence finding 6: git's hunk on neighbouring lines withholds the parent's decided line"]
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

/// Rule 3, decided once, at the parent's queue: nested G's g lands in its
/// parent F, where F's line m (main's, synced in) wins. F's next landing
/// into main carries nothing of g, so main records nothing; G takes m from
/// F. Sibling H holds m throughout.
#[test]
fn r3_a_collision_is_recorded_once_at_the_parents_queue() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let (f, h) = (w.create_fork(Tree::Main), w.create_fork(Tree::Main));
        let g = w.create_fork(Tree::Fork(f));
        let gl = w.save(&Client::Fork(g), "a", 0);
        let m = w.save(&m1(), "a", 0);
        next(&mut w, Tree::Fork(f)); // G's g lands in F: F has nothing there
        assert!(has(&w, Tree::Fork(f), "a", &gl));
        drain(&mut w);
        for t in [Tree::Main, Tree::Fork(f), Tree::Fork(g), Tree::Fork(h)] {
            assert!(has(&w, t, "a", &m), "{history:?} {t:?}");
        }
        let records: Vec<Tree> = w
            .collisions
            .iter()
            .filter(|c| c.lost.contains(&gl))
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
/// G's parent. G's landing loses in F to F's own concurrent line; G's
/// later line lands in F, then loses in main to main's concurrent line;
/// a clean change of G's reaches main named for G. Everything converges.
#[test]
fn r4_nested_forks_follow_the_same_rules() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let g = w.create_fork(Tree::Fork(f));
        let (cf, cg) = (Client::Fork(f), Client::Fork(g));
        let ga = w.save(&cg, "a", 0);
        let fa = w.save(&cf, "a", 0);
        next(&mut w, Tree::Fork(f)); // G's a lands in F: F's line wins.
        assert!(has(&w, Tree::Fork(f), "a", &fa) && !has(&w, Tree::Fork(f), "a", &ga));
        assert!(w.collisions.iter().any(|c| c.tree == Tree::Fork(f)
            && c.direction == Direction::Landing
            && c.lost == [ga.clone()]));
        let gb = w.save(&cg, "b", 0);
        let mb = w.save(&m1(), "b", 0);
        let gc = w.save(&cg, "c", 0);
        drain(&mut w);
        assert!(has(&w, Tree::Main, "b", &mb) && !has(&w, Tree::Main, "b", &gb));
        assert!(has(&w, Tree::Main, "c", &gc) && has(&w, Tree::Main, "a", &fa));
        for t in [Tree::Fork(f), Tree::Fork(g)] {
            assert!(has(&w, t, "b", &mb) && has(&w, t, "a", &fa), "{t:?}");
        }
        assert!(
            w.collisions
                .iter()
                .any(|c| c.tree == Tree::Main && c.lost == [gb.clone()])
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
        assert_eq!(w.commit_txn(f), Ok(TxnOutcome::clean()));
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

/// Rule 5, a commit meeting a concurrent line (a design choice): main's m
/// on a line the transaction changed has already synced into the sandbox.
/// The commit lands in the sandbox as a child's change lands in its
/// parent: no collision refuses it, the sandbox's m wins that line, the other staged
/// line applies, and the answer names the overwritten line. The collision
/// is recorded in the sandbox, naming the staged line.
#[test]
fn r5_transaction_commit_reports_the_overwritten_line() {
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
        assert_eq!(
            w.commit_txn(f),
            Ok(TxnOutcome::Committed {
                overwritten: vec![Overwritten {
                    path: "a".into(),
                    line: "a0".into(),
                    staged: s.clone(),
                    kept: m.clone(),
                }]
            })
        );
        ok(&mut w);
        assert!(w.client_reads(&c, "a").contains(&m) && w.client_reads(&c, "b").contains(&s2));
        let col = w.collisions.last().expect("a collision").clone();
        assert_eq!(
            (col.tree, col.direction, col.lost, col.won),
            (
                Tree::Fork(f),
                Direction::Transaction,
                vec![s.clone()],
                vec![m.clone()]
            )
        );
        let refs = git(
            w.repo(),
            &["for-each-ref", "--format=%(refname)", "refs/heads/txn"],
        );
        assert!(refs.is_empty());
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &m) && has(&w, Tree::Main, "b", &s2));
    }
}

/// Rule 5, no race: the same calls as above and as
/// [`r5_a_committed_transaction_then_lands_like_any_change`], where main's
/// sync arrives after the commit, end with the same trees. Whether the
/// sync comes before or after the commit decides nothing: main's line
/// wins either way, at the sandbox's commit or at main's queue.
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
            assert!(matches!(w.commit_txn(f), Ok(TxnOutcome::Committed { .. })));
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

/// Rule 5 then rule 3: a committed transaction lands like any change. A
/// sync of main's concurrent line, queued before the commit and processed
/// after, decides nothing: the sandbox keeps the whole transaction. Main's
/// queue then decides: main's line wins line a, the transaction's line b
/// applies, and the sandbox takes main's line a, once.
#[test]
fn r5_a_committed_transaction_then_lands_like_any_change() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        w.begin(f).unwrap();
        let sa = w.save(&Client::Fork(f), "a", 0);
        let sb = w.save(&Client::Fork(f), "b", 0);
        let m = w.save(&m1(), "a", 0); // queued into F, not yet synced
        assert_eq!(w.commit_txn(f), Ok(TxnOutcome::clean()));
        next(&mut w, Tree::Fork(f));
        assert!(has(&w, Tree::Fork(f), "a", &sa) && has(&w, Tree::Fork(f), "b", &sb));
        drain(&mut w);
        for t in [Tree::Main, Tree::Fork(f)] {
            assert!(
                has(&w, t, "a", &m) && has(&w, t, "b", &sb),
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
/// in lineage. Either way the fork's losing version stays reachable under
/// the collision's ref after the fork is dropped.
#[test]
fn r7_history_setting_and_losers_stay_reachable() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let x = w.save(&Client::Fork(f), "a", 0);
        let fc = w.head(Tree::Fork(f));
        w.save(&m1(), "a", 0);
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
            .find(|c| c.lost == [x.clone()])
            .expect("x lost");
        let at = git(w.repo(), &["rev-parse", &c.reference]);
        assert!(w.file(&at, "a").contains(&x), "{history:?}");
    }
}

/// Rule 7, the squash: under `Squashed` a sandbox's plain saves waiting in
/// main's queue when it takes the first land as ONE changeset ("the
/// sandbox commits could be squashed to one changeset (if they are the
/// naive simple commits)"); a transaction's commit lands as its own unit
/// ("it might be best to leave the transaction commits as they are"), and
/// so does each later save. Every sandbox commit stays stored ("every
/// sandbox commit stays stored ... that makes sense"), in main's lineage
/// through the graft, not in main's pushed history. Under `FullLineage`
/// each commit lands on its own, in main's history.
#[test]
fn r7_squashed_saves_land_as_one_changeset() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let c = Client::Fork(f);
        let saves: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|p| {
                w.save(&c, p, 0);
                w.head(Tree::Fork(f))
            })
            .collect();
        w.begin(f).unwrap();
        w.save(&c, "d", 0);
        w.commit_txn(f).unwrap();
        w.save(&c, "a", 1);
        assert_eq!(w.queue_len(Tree::Main), 5);
        let before = main_writers(&w).len();
        next(&mut w, Tree::Main);
        let squashed = history == HistoryPolicy::Squashed;
        assert_eq!(w.queue_len(Tree::Main), if squashed { 2 } else { 4 });
        assert_eq!(main_writers(&w).len(), before + 1);
        let landed = ["a", "b", "c"].map(|p| w.read(Tree::Main, p).at(0) != format!("{p}0"));
        assert_eq!(landed, [true, squashed, squashed], "{history:?}");
        drain(&mut w);
        assert_eq!(
            main_writers(&w).len(),
            before + if squashed { 3 } else { 5 },
            "{history:?}"
        );
        for s in &saves {
            assert!(w.is_ancestor(s, &w.main_head), "{history:?}: lineage");
            assert_eq!(w.in_history(s, &w.main_head), !squashed, "{history:?}");
        }
        assert_eq!(w.squashed_runs, usize::from(squashed));
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

// ── rule 8: a save records the write calls ──────────────────────────────

/// The sandbox's program read `a` before main's m reached it by sync,
/// then opens `a`, makes one write call and closes. Returns the world, the
/// program's stale read, the synced m, and the sandbox.
fn read_then_sync(history: HistoryPolicy) -> (World, Content, String, u32) {
    let mut w = world(history, CollisionScope::Line);
    let f = w.create_fork(Tree::Main);
    let o = w.save(&m1(), "a", 0);
    drain(&mut w);
    let read = w.client_reads(&Client::Fork(f), "a");
    assert!(read.contains(&o));
    let m = w.save(&m1(), "a", 0); // over o
    next(&mut w, Tree::Fork(f));
    assert!(has(&w, Tree::Fork(f), "a", &m));
    (w, read, m, f)
}

/// Rule 8, an append after the sync: it adds a line at the file's end as
/// it is now, so the line the sync brought stays, in the sandbox and, once
/// landed, in main.
#[test]
fn r8_an_append_after_the_sync_keeps_the_synced_line() {
    for history in BOTH {
        let (mut w, _, m, f) = read_then_sync(history);
        let h = w.open_write(&Client::Fork(f), "a");
        let x = w.write(h, Write::Append).unwrap();
        w.close(h);
        ok(&mut w);
        assert_eq!(w.read(Tree::Fork(f), "a").last(), x, "after the end");
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &m) && has(&w, Tree::Main, "a", &x));
        assert!(w.collisions.is_empty(), "{history:?}");
    }
}

/// Rule 8, a write at an offset elsewhere: it replaces exactly that line;
/// the line the sync brought, which the program never read, stays.
#[test]
fn r8_an_offset_write_elsewhere_keeps_the_synced_line() {
    for history in BOTH {
        let (mut w, _, m, f) = read_then_sync(history);
        let h = w.open_write(&Client::Fork(f), "a");
        let y = w.write(h, Write::At(2)).unwrap();
        w.close(h);
        drain(&mut w);
        for t in [Tree::Main, Tree::Fork(f)] {
            let a = w.read(t, "a");
            assert_eq!((a.at(0), a.at(2)), (m.as_str(), y.as_str()), "{t:?}");
        }
        assert!(w.collisions.is_empty(), "{history:?}");
    }
}

/// Rule 8, a stale truncate and rewrite: the program writes back the whole
/// file it read before the sync. That really puts o's bytes back over m,
/// recorded as the program's write: a new line by the sandbox's writer
/// (`git blame` names it), written over m, so it replaces m in main too
/// with no collision. m stays in main's history.
#[test]
fn r8_a_stale_rewrite_overwrites_the_synced_line_and_history_keeps_it() {
    for history in BOTH {
        let (mut w, read, m, f) = read_then_sync(history);
        let m_commit = w.main_head.clone();
        let h = w.open_write(&Client::Fork(f), "a");
        assert_eq!(w.write(h, Write::Rewrite(read.clone())), None);
        w.close(h);
        let a = w.read(Tree::Fork(f), "a");
        let r = a.at(0).to_string();
        assert_eq!(
            a.get(0).and_then(|l| l.text()),
            Some(read.at(0)),
            "o's bytes, back"
        );
        assert!(r.starts_with("c1.") && w.lineage.descends(&r, &m));
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &r) && !has(&w, Tree::Main, "a", &m));
        assert!(w.collisions.is_empty(), "{history:?}: no collision");
        // The first line's record: git line `line_of(1)`, after the name.
        let n = Content::line_of(1, Layout::Separated);
        let blame = git(
            w.repo(),
            &[
                "blame",
                "--porcelain",
                &format!("-L{n},{n}"),
                &w.main_head,
                "--",
                "a",
            ],
        );
        let by = blame.split(' ').next().expect("a commit").to_string();
        assert_eq!(
            w.writer_of_commit(&by),
            "c1",
            "{history:?}: the program's write"
        );
        assert!(w.in_history(&m_commit, &w.main_head), "m stays in history");
        assert!(w.file(&m_commit, "a").contains(&m));
    }
}

/// Rule 8, `O_APPEND`: two of main's clients append to one file at once.
/// Each append goes after the file's end as it is at its close, so neither
/// writes over the other's line, whichever closes first; and a check
/// rejects an append that lands on a written slot (`one_writer`).
#[test]
fn r8_concurrent_appends_never_overwrite_each_other() {
    for history in BOTH {
        for m2_first in [true, false] {
            let mut w = world(history, CollisionScope::Line);
            let o = w.save(&m1(), "a", 0);
            let m2 = Client::Main("m2".into());
            let (a, b) = (w.open_write(&m1(), "a"), w.open_write(&m2, "a"));
            let xa = w.write(a, Write::Append).unwrap();
            let xb = w.write(b, Write::Append).unwrap();
            let (first, second) = if m2_first { (b, a) } else { (a, b) };
            w.close(first);
            ok(&mut w);
            w.close(second);
            ok(&mut w);
            let f = w.read(Tree::Main, "a");
            let (one, two) = if m2_first { (&xb, &xa) } else { (&xa, &xb) };
            assert_eq!(
                (f.at(0), f.at(6), f.at(7)),
                (o.as_str(), one.as_str(), two.as_str()),
                "{history:?}: in close order after the last line, none over another"
            );
            drain(&mut w);
        }
    }
}

// ── rule 9: a take locks the files it overwrites ────────────────────────

/// Rule 9: main's sync waits while the sandbox's program has `a` open for
/// writing: its queue head records the wait and its step is not offered.
/// The close (an event) wakes it, and it applies over the saved file. A
/// sync of a file nobody has open does not wait.
#[test]
fn r9_a_sync_waits_for_an_open_writer_then_applies() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let h = w.open_write(&Client::Fork(f), "a");
        let m = w.save(&m1(), "a", 0);
        take(&mut w, &Step::Next(Tree::Fork(f)));
        assert_eq!(w.waits()[&Tree::Fork(f)], BTreeSet::from(["a".to_string()]));
        assert!(!w.pending().contains(&Step::Next(Tree::Fork(f))));
        assert!(!has(&w, Tree::Fork(f), "a", &m), "{history:?}: not written");
        let x = w.write(h, Write::At(1)).unwrap();
        w.close(h);
        ok(&mut w);
        assert!(w.waits().is_empty(), "the close woke it");
        next(&mut w, Tree::Fork(f));
        let a = w.read(Tree::Fork(f), "a");
        assert_eq!((a.at(0), a.at(1)), (m.as_str(), x.as_str()));
        let h = w.open_write(&Client::Fork(f), "b");
        w.save(&m1(), "c", 0);
        next(&mut w, Tree::Fork(f)); // c only: does not wait for b
        w.close(h);
        drain(&mut w);
    }
}

/// Rule 9, the same rule for a landing into a tree (a choice): main's
/// queue waits while one of main's clients has the file open for writing.
#[test]
fn r9_a_landing_waits_for_the_parents_open_writer() {
    let mut w = world(HistoryPolicy::FullLineage, CollisionScope::Line);
    let f = w.create_fork(Tree::Main);
    let h = w.open_write(&m1(), "a");
    let x = w.save(&Client::Fork(f), "a", 2);
    take(&mut w, &Step::Next(Tree::Main));
    assert!(w.waits().contains_key(&Tree::Main) && !has(&w, Tree::Main, "a", &x));
    let y = w.write(h, Write::At(0)).unwrap();
    w.close(h);
    ok(&mut w);
    next(&mut w, Tree::Main);
    assert!(has(&w, Tree::Main, "a", &x) && has(&w, Tree::Main, "a", &y));
    drain(&mut w);
}

/// Rule 9: while a sync holds `a`, a new write-open of `a` waits; the
/// sync's release (an event) grants it, showing the synced file.
#[test]
fn r9_a_write_open_waits_while_a_sync_holds_the_file() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let m = w.save(&m1(), "a", 0);
        take(&mut w, &Step::Next(Tree::Fork(f)));
        assert!(w.holds()[&Tree::Fork(f)].files.contains("a"));
        let h = w.open_write(&Client::Fork(f), "a");
        ok(&mut w);
        assert!(w.handle(h).waiting, "{history:?}: the open waits");
        let other = w.open_write(&Client::Fork(f), "b");
        assert!(!w.handle(other).waiting, "an unheld file opens at once");
        take(&mut w, &Step::Finish(Tree::Fork(f)));
        assert!(!w.handle(h).waiting, "the release granted it");
        let x = w.write(h, Write::Append).unwrap();
        w.close(h);
        w.close(other);
        let a = w.read(Tree::Fork(f), "a");
        assert_eq!((a.at(0), a.last()), (m.as_str(), x.as_str()));
        drain(&mut w);
    }
}

/// Rule 9, the client's death (its end event) releases its locks: its
/// open file closes, saving its write calls, which wakes the waiting sync;
/// its waiting open is dropped.
#[test]
fn r9_client_death_releases_its_locks() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let (f, g) = (w.create_fork(Tree::Main), w.create_fork(Tree::Main));
        let h = w.open_write(&Client::Fork(f), "a");
        let x = w.write(h, Write::At(1)).unwrap();
        let m = w.save(&m1(), "a", 0);
        take(&mut w, &Step::Next(Tree::Fork(f)));
        assert!(w.waits().contains_key(&Tree::Fork(f)));
        let n = w.save(&m1(), "b", 0);
        take(&mut w, &Step::Next(Tree::Fork(g)));
        let waiting = w.open_write(&Client::Fork(g), "a");
        assert!(w.handle(waiting).waiting);
        w.end_client(f);
        w.end_client(g);
        ok(&mut w);
        assert!(
            w.waits().is_empty() && w.handles().next().is_none(),
            "{history:?}"
        );
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &x) && has(&w, Tree::Main, "a", &m));
        assert!(has(&w, Tree::Main, "b", &n));
    }
}

/// Rule 9, one of main's clients dies (its end event) holding `a` open
/// for writing while main's queue waits for it, and waiting to open `b`,
/// which a landing holds: its write calls are saved as the kernel's close
/// on exit would, which wakes the queue; its waiting open is dropped; and
/// no lock of it outlives it (`locks`).
#[test]
fn r9_a_main_clients_death_releases_its_locks() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let x = w.save(&Client::Fork(f), "b", 0);
        take(&mut w, &Step::Next(Tree::Main));
        assert!(w.holds()[&Tree::Main].files.contains("b"));
        let waiting = w.open_write(&m1(), "b");
        assert!(w.handle(waiting).waiting);
        take(&mut w, &Step::Finish(Tree::Main));
        w.close(waiting);
        let h = w.open_write(&m1(), "a");
        let y = w.write(h, Write::At(0)).unwrap();
        let z = w.save(&Client::Fork(f), "a", 2);
        take(&mut w, &Step::Next(Tree::Main));
        assert!(w.waits().contains_key(&Tree::Main), "{history:?}");
        let m2 = Client::Main("m2".into());
        let other = w.open_write(&m2, "d");
        w.end_main_client("m1");
        ok(&mut w);
        assert!(
            w.waits().is_empty(),
            "{history:?}: the death woke main's queue"
        );
        assert!(has(&w, Tree::Main, "a", &y), "its write calls were saved");
        assert!(!w.handle(other).waiting, "another client's open stays");
        next(&mut w, Tree::Main);
        assert!(has(&w, Tree::Main, "a", &z));
        w.close(other);
        drain(&mut w);
        assert!(has(&w, Tree::Main, "b", &x));
        assert!(w.handles().next().is_none());
    }
}

/// Rule 5 with open files (a choice): a file the program opened before
/// begin keeps writing to the sandbox, so a long-lived open file blocks no
/// transaction. The commit is refused ([`ModelError::OpenFiles`]) only
/// while the program itself holds open a file of the transaction, or a
/// sandbox file the commit would write; once it closes them, it commits.
#[test]
fn r5_open_files_and_transactions() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let c = Client::Fork(f);
        let log = w.open_write(&c, "d");
        let edit = w.open_write(&c, "a");
        w.begin(f).unwrap();
        let s = w.save(&c, "a", 1);
        let staged = w.open_write(&c, "b");
        assert_eq!(w.commit_txn(f), Err(ModelError::OpenFiles), "{history:?}");
        w.write(staged, Write::At(0)).unwrap();
        w.close(staged);
        assert_eq!(w.commit_txn(f), Err(ModelError::OpenFiles), "a is open");
        let l = w.write(log, Write::Append).unwrap();
        w.close(log);
        assert!(
            has(&w, Tree::Fork(f), "d", &l),
            "the log saves to the sandbox"
        );
        assert!(!l.contains('~'), "not a staged line");
        w.close(edit);
        assert_eq!(w.commit_txn(f), Ok(TxnOutcome::clean()));
        assert!(has(&w, Tree::Fork(f), "a", &s));
        drain(&mut w);
    }
}

// ── rule 10: line identity ──────────────────────────────────────────────

/// One open, one write call, one close: the version the call made, if any.
fn call(w: &mut World, c: &Client, p: &str, call: Write) -> Option<String> {
    let h = w.open_write(c, p);
    let made = w.write(h, call);
    w.close(h);
    made
}

/// The live lines' version ids of `p` in `t`.
fn ids(w: &World, t: Tree, p: &str) -> Vec<String> {
    w.read(t, p).live().map(|l| l.id.clone()).collect()
}

/// A content holding these bytes, for a whole-file rewrite (only its live
/// lines' bytes are written).
fn texts(lines: &[&str]) -> Content {
    let mut c = Content::root("a");
    c.lines = lines
        .iter()
        .enumerate()
        .map(|(i, t)| Line {
            id: format!("t{i}"),
            line: format!("t{i}"),
            over: None,
            state: State::Live(t.to_string()),
        })
        .collect();
    c
}

fn ends_converged_without_collisions(w: &mut World) {
    drain(w);
    assert!(w.collisions.is_empty(), "{:?}", w.collisions);
}

/// Rule 10: an insert shifts the lines after it without changing their
/// identity. The sandbox inserts a line at the top while main writes line
/// 2; then main inserts at the top while the sandbox writes line 2. Each
/// edit lands on the line its writer meant, now one further down, with no
/// collision.
#[test]
fn r10_an_insert_shifts_lines_without_changing_their_identity() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let x = call(&mut w, &Client::Fork(f), "a", Write::Insert(0)).unwrap();
        let m = w.save(&m1(), "a", 2);
        ends_converged_without_collisions(&mut w);
        let a = w.read(Tree::Main, "a");
        assert_eq!((a.at(0), a.at(3)), (x.as_str(), m.as_str()), "{history:?}");
        let n = call(&mut w, &m1(), "b", Write::Insert(0)).unwrap();
        let y = w.save(&Client::Fork(f), "b", 2);
        ends_converged_without_collisions(&mut w);
        let b = w.read(Tree::Main, "b");
        assert_eq!((b.at(0), b.at(3)), (n.as_str(), y.as_str()), "{history:?}");
        assert_eq!(ids(&w, Tree::Fork(f), "b"), ids(&w, Tree::Main, "b"));
    }
}

/// Rule 10, two inserts at one place: both stay, the parent's first (a
/// design choice), with no collision, whichever reaches the sandbox first.
#[test]
fn r10_concurrent_inserts_at_one_place_keep_both_the_parents_first() {
    for history in BOTH {
        for sync_first in [false, true] {
            let mut w = world(history, CollisionScope::Line);
            let f = w.create_fork(Tree::Main);
            let x = call(&mut w, &Client::Fork(f), "a", Write::Insert(1)).unwrap();
            let m = call(&mut w, &m1(), "a", Write::Insert(1)).unwrap();
            if sync_first {
                next(&mut w, Tree::Fork(f));
                assert_eq!(ids(&w, Tree::Fork(f), "a")[..4], ["a0", &m, &x, "a1"]);
            }
            ends_converged_without_collisions(&mut w);
            let a = ids(&w, Tree::Main, "a");
            assert_eq!(a[..4], ["a0", &m, &x, "a1"], "{history:?} {sync_first}");
        }
    }
}

/// Rule 10, a delete against a concurrent edit of the same line: a delete
/// is a version of the line, so the per-line rule decides. The lines hold
/// distinct bytes (among identical ones the diff picks which one went:
/// `r10_repeated_identical_lines_align_by_the_diff`). The sandbox deletes
/// line 1 while main writes it: main's line stays. Main deletes line 1
/// while the sandbox writes it: the line stays deleted. Both delete it:
/// they agree, no collision.
#[test]
fn r10_a_delete_against_a_concurrent_edit_the_parent_wins() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let t = distinct(&mut w, "a");
        call(&mut w, &Client::Fork(f), "a", Write::Delete(1));
        let m = w.save(&m1(), "a", 1);
        drain(&mut w);
        assert_eq!(
            ids(&w, Tree::Main, "a")[..3],
            [t[0].as_str(), &m, &t[2]],
            "{history:?}"
        );
        let c = w.collisions.last().expect("a collision").clone();
        assert_eq!((c.lines, c.won), (vec!["a1".to_string()], vec![m.clone()]));
        let t = distinct(&mut w, "b");
        call(&mut w, &m1(), "b", Write::Delete(1));
        let x = w.save(&Client::Fork(f), "b", 1);
        drain(&mut w);
        assert_eq!(
            ids(&w, Tree::Main, "b")[..2],
            [t[0].as_str(), &t[2]],
            "{history:?}"
        );
        assert_eq!(w.collisions.last().map(|c| c.lost.clone()), Some(vec![x]));
        let t = distinct(&mut w, "c");
        let before = w.collisions.len();
        call(&mut w, &Client::Fork(f), "c", Write::Delete(1));
        call(&mut w, &m1(), "c", Write::Delete(1));
        drain(&mut w);
        assert_eq!(w.collisions.len(), before, "{history:?}: two deletes agree");
        assert_eq!(ids(&w, Tree::Main, "c")[..2], [t[0].as_str(), &t[2]]);
    }
}

/// Lines 0 to 3 of `p` given distinct bytes by main, then every tree
/// synced: their bytes, in order.
fn distinct(w: &mut World, p: &str) -> Vec<String> {
    let h = w.open_write(&m1(), p);
    let made: Vec<String> = (0..4).filter_map(|i| w.write(h, Write::At(i))).collect();
    w.close(h);
    drain(w);
    made
}

/// Rule 10, a move: the save sees only bytes, so its one alignment reads a
/// moved line as a delete plus a new line holding the same bytes. With no
/// concurrent edit of that line it lands as the program wrote it. Against
/// main's concurrent edit of the moved line, the delete loses to main's
/// edit and the new line applies: main's edit shows at the old place and
/// the old bytes at the new one, and only the delete is recorded as a
/// collision (a cost: git-model.md §16). Main's move against the
/// sandbox's edit of that line: main's delete wins, the sandbox's edit
/// loses.
#[test]
fn r10_a_move_is_a_delete_plus_an_insert() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let t = distinct(&mut w, "a");
        let mv = Write::Move { from: 0, to: 3 };
        call(&mut w, &Client::Fork(f), "a", mv.clone());
        let m = w.save(&m1(), "a", 5);
        ends_converged_without_collisions(&mut w);
        let a = w.read(Tree::Main, "a");
        assert_eq!(
            a.texts(),
            [&t[1], &t[2], &t[3], &t[0], BLANK, &m],
            "{history:?}"
        );
        assert!(!a.contains(&t[0]), "{history:?}: the moved line is new");
        let t = distinct(&mut w, "b");
        call(&mut w, &Client::Fork(f), "b", mv.clone());
        let m = w.save(&m1(), "b", 0);
        drain(&mut w);
        assert_eq!(
            w.read(Tree::Main, "b").texts()[..5],
            [m.as_str(), &t[1], &t[2], &t[3], &t[0]],
            "{history:?}: main's edit and the old bytes both show"
        );
        let c = w.collisions.last().expect("a collision").clone();
        assert_eq!((c.lines, c.won), (vec!["b0".to_string()], vec![m]));
        let t = distinct(&mut w, "c");
        let before = w.collisions.len();
        call(&mut w, &m1(), "c", mv);
        let x = w.save(&Client::Fork(f), "c", 0);
        drain(&mut w);
        assert_eq!(
            w.read(Tree::Main, "c").texts()[..4],
            [t[1].as_str(), &t[2], &t[3], &t[0]],
            "{history:?}"
        );
        let lost: Vec<String> = w.collisions[before..]
            .iter()
            .flat_map(|c| c.lost.clone())
            .collect();
        assert_eq!(lost, [x]);
    }
}

/// Rule 10, repeated lines. Six blank lines, and the sandbox inserts a
/// blank line at the top while main writes line 1. In bytes the new line
/// could be any of seven blank lines; git's diff takes the last (it trims
/// the common prefix first), so main's edit stays on line 1, not line 2,
/// as the sandbox's program would read it (a cost: git-model.md §16). No
/// line is lost or doubled, and no collision. Then braces: main writes
/// `{ x } { y }`; the sandbox's program rewrites the whole file with a
/// third block between, while main edits `y`. The diff anchors on the
/// lines that occur once, so the rewrite's new braces are new lines and
/// main's edit stays on `y`'s line, no collision.
#[test]
fn r10_repeated_identical_lines_align_by_the_diff() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let h = w.open_write(&Client::Fork(f), "a");
        w.write_text(h, Write::Insert(0), BLANK);
        w.close(h);
        let m = w.save(&m1(), "a", 1);
        ends_converged_without_collisions(&mut w);
        let a = w.read(Tree::Main, "a");
        assert_eq!(
            a.texts(),
            [BLANK, &m, BLANK, BLANK, BLANK, BLANK, BLANK],
            "{history:?}"
        );
        let h = w.open_write(&m1(), "b");
        for (i, t) in ["{", "x", "}", "{", "y", "}"].iter().enumerate() {
            w.write_text(h, Write::At(i), t);
        }
        w.close(h);
        drain(&mut w);
        let block = ["{", "x", "}", "{", "z", "}", "{", "y", "}"];
        let h = w.open_write(&Client::Fork(f), "b");
        w.write(h, Write::Rewrite(texts(&block)));
        let y = w.save(&m1(), "b", 4);
        w.close(h);
        ends_converged_without_collisions(&mut w);
        let b = w.read(Tree::Main, "b").texts();
        assert_eq!(
            b,
            ["{", "x", "}", "{", "z", "}", "{", &y, "}"],
            "{history:?}"
        );
    }
}

// ── rule 11: deletes and renames ────────────────────────────────────────

/// Rule 11, an unlink against a concurrent edit, in each direction. The
/// sandbox unlinks `a` while main writes a line of it: the unlink never
/// saw that line, so it loses whole and `a` stays, with main's line. Main
/// unlinks `b` while the sandbox writes a line of it: main's unlink wins,
/// the sandbox's line loses. Both are recorded collisions.
#[test]
fn r11_an_unlink_against_a_concurrent_edit_the_parent_wins() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        assert_eq!(w.unlink(&Client::Fork(f), "a"), None);
        assert_eq!(w.path_of(Tree::Fork(f), "a"), None, "unlinked at once");
        let m = w.save(&m1(), "a", 0);
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &m) && has(&w, Tree::Fork(f), "a", &m));
        let c = w.collisions.last().expect("a collision").clone();
        assert!(
            c.by_file && c.lines == ["a"] && c.won == [m.clone()],
            "{c:?}"
        );
        w.unlink(&m1(), "b");
        let x = w.save(&Client::Fork(f), "b", 0);
        drain(&mut w);
        for t in [Tree::Main, Tree::Fork(f)] {
            assert_eq!(w.path_of(t, "b"), None, "{history:?} {t:?}");
        }
        let c = w.collisions.last().expect("a collision").clone();
        assert!(c.by_file && c.lost == [x.clone()], "{c:?}");
    }
}

/// Rule 11, an unlink after the sync: the sandbox saw main's line, so its
/// unlink wins upstream by lineage, with no collision.
#[test]
fn r11_an_unlink_after_the_sync_wins() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        w.save(&m1(), "a", 0);
        next(&mut w, Tree::Fork(f));
        w.unlink(&Client::Fork(f), "a");
        ends_converged_without_collisions(&mut w);
        assert_eq!(w.path_of(Tree::Main, "a"), None, "{history:?}");
    }
}

/// Rule 11, a rename and a concurrent edit both apply: a file keeps its
/// identity through a rename, so the edit follows it to its new path, in
/// each direction.
#[test]
fn r11_a_rename_and_a_concurrent_edit_both_apply() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        w.rename(&Client::Fork(f), "a", "e");
        let m = w.save(&m1(), "a", 0);
        ends_converged_without_collisions(&mut w);
        assert!(has(&w, Tree::Main, "e", &m), "{history:?}");
        w.rename(&m1(), "b", "g");
        let x = w.save(&Client::Fork(f), "b", 0);
        ends_converged_without_collisions(&mut w);
        assert!(has(&w, Tree::Main, "g", &x) && has(&w, Tree::Fork(f), "g", &x));
    }
}

/// Rule 11, two concurrent renames of one file: the parent's name wins.
/// Two files renamed to one path: the parent's file keeps it, the
/// sandbox's keeps its old name.
#[test]
fn r11_concurrent_renames_the_parent_wins() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        w.rename(&Client::Fork(f), "a", "e");
        w.rename(&m1(), "a", "g");
        drain(&mut w);
        for t in [Tree::Main, Tree::Fork(f)] {
            assert_eq!(w.path_of(t, "a").as_deref(), Some("g"), "{history:?} {t:?}");
        }
        w.rename(&Client::Fork(f), "b", "h");
        w.rename(&m1(), "c", "h");
        // Main's queue decides before the sandbox syncs: a sync first would
        // show the sandbox two files named h (not modelled: MODEL.md).
        next(&mut w, Tree::Main);
        drain(&mut w);
        for t in [Tree::Main, Tree::Fork(f)] {
            let names = (w.path_of(t, "b"), w.path_of(t, "c"));
            assert_eq!(names, (Some("b".into()), Some("h".into())), "{t:?}");
        }
        let c = w.collisions.last().expect("a clash").clone();
        assert!(c.by_file && c.lines == ["b"], "{c:?}");
    }
}

/// Rule 11, a rename-over (`rename(2)` onto an existing file, the atomic
/// save idiom) is one call: it applies or loses whole. The sandbox renames
/// `a` over `b` while main writes a line of `b`: the unlink of `b` never
/// saw it, so the whole rename-over loses: `a` keeps its name, `b` keeps
/// main's line. Without the concurrent edit it applies. Main's rename-over
/// takes a sandbox edit of the renamed file with it, and the replaced
/// file's edit loses.
#[test]
fn r11_a_rename_over_applies_or_loses_whole() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        w.rename(&Client::Fork(f), "a", "b");
        let m = w.save(&m1(), "b", 0);
        drain(&mut w);
        for t in [Tree::Main, Tree::Fork(f)] {
            assert_eq!(w.path_of(t, "a").as_deref(), Some("a"), "{history:?} {t:?}");
            assert!(has(&w, t, "b", &m));
        }
        let lost: BTreeSet<&str> = w.collisions.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(lost, BTreeSet::from(["a", "b"]), "both halves lose");
        let before = w.collisions.len();
        w.rename(&Client::Fork(f), "c", "d");
        drain(&mut w);
        assert_eq!(w.collisions.len(), before, "{history:?}: it applies");
        assert_eq!(w.path_of(Tree::Main, "c").as_deref(), Some("d"));
        assert_eq!(w.path_of(Tree::Main, "d"), None);
        w.rename(&m1(), "a", "b");
        let kept = w.save(&Client::Fork(f), "a", 1);
        let gone = w.save(&Client::Fork(f), "b", 1);
        drain(&mut w);
        assert!(has(&w, Tree::Main, "b", &kept), "{history:?}");
        assert_eq!(w.path_of(Tree::Main, "a").as_deref(), Some("b"));
        assert_eq!(w.path_of(Tree::Main, "b"), None);
        let lost: Vec<String> = w.collisions[before..]
            .iter()
            .flat_map(|c| c.lost.clone())
            .collect();
        assert_eq!(lost, [gone]);
    }
}

/// Rule 11, a rename-over's other half: the sandbox renames `a` over `b`
/// while main renames `a` to `g`. The sandbox's rename loses to main's
/// name, so its unlink of `b` loses with it (one call): `b` keeps its path
/// and its lines, and `a` is at `g`, in both trees.
#[test]
fn r11_a_rename_over_whose_rename_loses_keeps_the_replaced_file() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let x = w.save(&m1(), "b", 0);
        drain(&mut w);
        w.rename(&Client::Fork(f), "a", "b");
        w.rename(&m1(), "a", "g");
        drain(&mut w);
        for t in [Tree::Main, Tree::Fork(f)] {
            assert_eq!(w.path_of(t, "a").as_deref(), Some("g"), "{history:?} {t:?}");
            assert_eq!(w.path_of(t, "b").as_deref(), Some("b"), "{history:?} {t:?}");
            assert!(has(&w, t, "b", &x), "{history:?} {t:?}");
        }
        let lost: BTreeSet<&str> = w.collisions.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(lost, BTreeSet::from(["a", "b"]), "both halves lose");
    }
}

/// Rule 11 with rule 9: an unlink or a rename of a file a take holds waits
/// like a write-open, and the take's release runs it; a rename onto a held
/// file waits too. A take that brings an unlink waits for the sandbox's
/// open writer of that file.
#[test]
fn r11_an_unlink_or_rename_of_a_held_file_waits() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let f = w.create_fork(Tree::Main);
        let c = Client::Fork(f);
        w.save(&m1(), "a", 0);
        take(&mut w, &Step::Next(Tree::Fork(f)));
        let h = w.unlink(&c, "a").expect("waits");
        assert!(w.handle(h).waiting && w.path_of(Tree::Fork(f), "a").is_some());
        take(&mut w, &Step::Finish(Tree::Fork(f)));
        assert_eq!(w.path_of(Tree::Fork(f), "a"), None, "{history:?}: it ran");
        w.save(&m1(), "c", 0);
        take(&mut w, &Step::Next(Tree::Fork(f)));
        assert!(w.rename(&c, "d", "c").is_some(), "onto a held file");
        take(&mut w, &Step::Finish(Tree::Fork(f)));
        assert_eq!(w.path_of(Tree::Fork(f), "d").as_deref(), Some("c"));
        drain(&mut w);
        w.unlink(&m1(), "b");
        let open = w.open_write(&c, "b");
        take(&mut w, &Step::Next(Tree::Fork(f)));
        assert!(w.waits().contains_key(&Tree::Fork(f)), "{history:?}");
        let x = w.write(open, Write::At(0)).unwrap();
        w.close(open);
        drain(&mut w);
        assert_eq!(w.path_of(Tree::Fork(f), "b"), None);
        assert!(w.collisions.iter().any(|c| c.lost.contains(&x)));
    }
}

/// Rule 11 in one tree, as on Linux: an open file follows its file through
/// a rename, and a file unlinked while open saves nothing at close.
#[test]
fn r11_an_open_file_follows_a_rename_and_an_unlinked_one_saves_nothing() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Line);
        let m2 = Client::Main("m2".into());
        let h = w.open_write(&m1(), "a");
        let x = w.write(h, Write::At(0)).unwrap();
        w.rename(&m2, "a", "e");
        w.close(h);
        assert!(has(&w, Tree::Main, "e", &x), "{history:?}");
        let h = w.open_write(&m1(), "b");
        let y = w.write(h, Write::At(0)).unwrap();
        w.unlink(&m2, "b");
        let head = w.main_head.clone();
        w.close(h);
        assert_eq!(w.main_head, head, "nothing saved");
        assert!(!w.file(&head, "b").holds(&y));
        drain(&mut w);
    }
}
