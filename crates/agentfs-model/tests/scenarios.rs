//! One or more scenarios per rule of MODEL.md, named `r<N>_...` after the
//! rule they show. Every scenario checks every invariant after each step.
//! A test marked `#[ignore = "coherence finding N: ..."]` asserts what the
//! rules intend but do not deliver; run it with `--ignored`.

use agentfs_model::check::{check, check_converged, step_checked};
use agentfs_model::content::Layout;
use agentfs_model::git::{git, with_reversed_dates};
use agentfs_model::world::{
    Client, CollisionScope, Direction, HistoryPolicy, Life, MergeRule, ModelError, Overwritten,
    Policies, Step, Tree, TxnOutcome, World,
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

/// One step ([`step_checked`]), then every invariant.
fn take(w: &mut World, s: &Step) {
    if let Err(e) = step_checked(w, s) {
        panic!("{e}\nlog:\n  {}", w.log.join("\n  "));
    }
    ok(w);
}

/// One queued step, then every invariant.
fn next(w: &mut World, t: Tree) {
    take(w, &Step::Next(t));
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
/// parent: never refused, the sandbox's m wins that line, the other staged
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
                    slot: 0,
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
        let landed = ["a", "b", "c"].map(|p| w.read(Tree::Main, p).slot(0) != "-");
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
