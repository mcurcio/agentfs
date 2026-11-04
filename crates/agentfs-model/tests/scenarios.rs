//! One or more scenarios per rule of MODEL.md, named `r<N>_...` after the
//! rule they show. Every scenario checks every invariant after each step.
//! A test marked `#[ignore = "coherence finding N: ..."]` asserts what the
//! rules intend but the variant does not deliver; run it with `--ignored`.
//!
//! Every file starts as six blank lines and a last line naming it. Two
//! edits merge cleanly when at least one unchanged line lies between them;
//! edits on neighbouring lines are one git hunk (MODEL.md, rule 3e).

use agentfs_model::check::{check, check_converged, step_checked};
use agentfs_model::content::File;
use agentfs_model::git::{git, with_reversed_dates};
use agentfs_model::world::{
    Client, CollisionScope, Direction, HistoryPolicy, Life, ModelError, Policies, Step, Tree,
    TxnOutcome, World, Write,
};
use std::collections::BTreeSet;

const BOTH: [HistoryPolicy; 2] = [HistoryPolicy::FullLineage, HistoryPolicy::Squashed];

fn world(history: HistoryPolicy) -> World {
    World::new(Policies::new(history, CollisionScope::Hunk))
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

/// One queued change taken whole (its take begins and ends: rule 9).
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

fn has(w: &World, t: Tree, p: &str, line: &str) -> bool {
    w.file(&w.head(t), p)
        .is_some_and(|f| f.iter().any(|l| l == line))
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

/// One write call by client `c` on `p`, closed at once.
fn call(w: &mut World, c: &Client, p: &str, call: Write) -> Option<String> {
    let h = w.open_write(c, p);
    let x = w.write(h, call);
    w.close(h);
    ok(w);
    x
}

/// The lines each hunk of the run's collisions lost and won, as written.
fn hunks(w: &World) -> Vec<(Tree, File, File)> {
    w.collisions
        .iter()
        .map(|c| (c.tree, c.theirs.clone(), c.ours.clone()))
        .collect()
}

// ── rule 1: like a Linux filesystem ─────────────────────────────────────

/// Rule 1: writes are never refused; each writer reads its own write at
/// once; forks sync both ways.
#[test]
fn r1_writes_never_wait_and_forks_sync_both_ways() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        let mine = w.save(&Client::Fork(f), "a", 0);
        let theirs = w.save(&m1(), "b", 0);
        let both = w.save(&m1(), "a", 2);
        ok(&mut w);
        assert!(has(&w, Tree::Fork(f), "a", &mine) && has(&w, Tree::Main, "b", &theirs));
        assert!(!has(&w, Tree::Main, "a", &mine), "landing is a queued step");
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &mine) && has(&w, Tree::Main, "a", &both));
        assert!(has(&w, Tree::Fork(f), "b", &theirs), "{history:?}");
        assert!(w.collisions.is_empty(), "one unchanged line apart: clean");
    }
}

// ── rule 2: one landing queue per tree; no clock ────────────────────────

/// Rule 2: main's queue fixes the order its children's commits land in.
#[test]
fn r2_one_queue_fixes_the_landing_order() {
    let mut w = world(HistoryPolicy::FullLineage);
    let (f, g) = (w.create_fork(Tree::Main), w.create_fork(Tree::Main));
    w.save(&Client::Fork(f), "a", 0);
    w.save(&Client::Fork(g), "b", 0);
    w.save(&Client::Fork(f), "c", 0);
    assert_eq!(w.queue(Tree::Main).len(), 3);
    for _ in 0..3 {
        next(&mut w, Tree::Main);
    }
    assert_eq!(main_writers(&w)[..3], ["c1", "c2", "c1"]);
    drain(&mut w);
}

/// Rule 2, "no clock is read anywhere": the same calls give the same
/// history with git's dates pinned; with every commit dated before its
/// parents they decide the same trees and collisions; and the model's
/// source names no clock.
#[test]
fn r2_no_clock_commit_dates_decide_nothing() {
    let run = || {
        let mut w = world(HistoryPolicy::FullLineage);
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
/// single-parent commit with that commit's tree, whose graft names it.
#[test]
fn r3_fast_forward_when_the_base_is_current() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        let tok = w.save(&Client::Fork(f), "a", 0);
        let fc = w.head(Tree::Fork(f));
        next(&mut w, Tree::Main);
        assert!(has(&w, Tree::Main, "a", &tok));
        match history {
            HistoryPolicy::FullLineage => {
                assert_eq!((w.main_head.as_str(), w.fast_forwards), (fc.as_str(), 1));
            }
            HistoryPolicy::Squashed => {
                assert_eq!(show(&w, &w.main_head, "%T"), show(&w, &fc, "%T"));
                assert_eq!(stored(&w, &w.main_head, "%P").split(' ').count(), 1);
                assert_eq!(
                    show(&w, &w.main_head, "%P").split(' ').nth(1),
                    Some(fc.as_str())
                );
            }
        }
        drain(&mut w);
    }
}

/// Rule 3, a sandbox's landing into main: main's concurrent line wins
/// ("i think the updated main should win in a conflict"). The collision
/// names the hunk, both sides; the losing commit stays reachable under the
/// collision's ref; the sandbox then takes main's line.
#[test]
fn r3_landing_collision_the_parent_wins() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        let x = w.save(&Client::Fork(f), "a", 0);
        let fc = w.head(Tree::Fork(f));
        let m = w.save(&m1(), "a", 0);
        let mc = w.main_head.clone();
        next(&mut w, Tree::Main);
        assert!(has(&w, Tree::Main, "a", &m) && !has(&w, Tree::Main, "a", &x));
        let c = w.collisions.last().expect("a collision").clone();
        assert_eq!((c.tree, c.direction), (Tree::Main, Direction::Landing));
        assert_eq!(
            (c.theirs.clone(), c.ours.clone()),
            (vec![x.clone()], vec![m.clone()])
        );
        assert_eq!(
            (c.winner.as_str(), c.loser.as_str()),
            (mc.as_str(), fc.as_str())
        );
        assert_eq!(git(w.repo(), &["rev-parse", &c.reference]), fc);
        drain(&mut w);
        assert!(has(&w, Tree::Fork(f), "a", &m) && has(&w, Tree::Main, "a", &m));
        assert_eq!(w.collisions.len(), 1, "{history:?}: recorded once");
    }
}

/// Rule 3, the variant's sync: main's sync reaches the sandbox before its
/// concurrent x lands. The sync rebases x's commit onto main's head; the
/// hunk conflicts, and the sandbox takes main's side at once (m), with no
/// collision recorded. Main's queue then decides, the same way, and records
/// it. The sandbox's line changes once, x then m.
#[test]
fn r3_a_sync_takes_the_parents_side_before_the_queue_records_it() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        let x = w.save(&Client::Fork(f), "a", 0);
        let m = w.save(&m1(), "a", 0);
        next(&mut w, Tree::Fork(f));
        assert!(has(&w, Tree::Fork(f), "a", &m) && !has(&w, Tree::Fork(f), "a", &x));
        assert!(w.collisions.is_empty(), "a sync records nothing");
        assert_eq!(w.head(Tree::Fork(f)), w.main_head, "x's copy became empty");
        assert_eq!((w.rebased, w.rebased_empty), (1, 1));
        next(&mut w, Tree::Main);
        drain(&mut w);
        assert_eq!(hunks(&w), [(Tree::Main, vec![x.clone()], vec![m.clone()])]);
        assert!(has(&w, Tree::Fork(f), "a", &m), "{history:?}");
    }
}

/// Rule 3c: a save after the sync brought main's line is written over it
/// and wins upstream: its own change (from its first parent) replaces m.
#[test]
fn r3_a_save_after_the_sync_wins_upstream_by_lineage() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        let m = w.save(&m1(), "a", 0);
        next(&mut w, Tree::Fork(f));
        let y = w.save(&Client::Fork(f), "a", 0);
        assert!(w.oracle.descends(&y, &m));
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &y) && !has(&w, Tree::Main, "a", &m));
        assert!(w.collisions.is_empty(), "{history:?}");
    }
}

/// Rule 3b: a save made before main's sync arrived is concurrent with it
/// and loses; the loser is pushed by Replication.
#[test]
fn r3_a_save_before_the_sync_arrives_loses_to_the_parent() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        let m = w.save(&m1(), "a", 0);
        let y = w.save(&Client::Fork(f), "a", 0);
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &m) && !has(&w, Tree::Main, "a", &y));
        assert_eq!(hunks(&w), [(Tree::Main, vec![y.clone()], vec![m.clone()])]);
        let r = &w.collisions[0].reference;
        let on_remote = git(w.remote(), &["rev-parse", r]);
        assert_eq!(on_remote, w.collisions[0].loser, "{history:?}");
    }
}

/// Rule 3, finding 8 under the variant. Before the sync: the sandbox
/// writes y over its own undecided x, both concurrent with main's m; both
/// lose (under `Squashed` too: a run lands commit by commit into one
/// result, each change from its own first parent). After the sync: the
/// sandbox shows m, so its next edit z is written over m and wins.
#[test]
fn r3_an_edit_over_an_undecided_line_still_collides() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        let x = w.save(&Client::Fork(f), "a", 0);
        let y = w.save(&Client::Fork(f), "a", 0);
        let m = w.save(&m1(), "a", 0);
        next(&mut w, Tree::Fork(f));
        assert!(has(&w, Tree::Fork(f), "a", &m));
        let z = w.save(&Client::Fork(f), "a", 0);
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &z), "{history:?}: z saw m");
        let lost: Vec<File> = hunks(&w).into_iter().map(|h| h.1).collect();
        assert_eq!(lost, [vec![x.clone()], vec![y.clone()]], "{history:?}");
    }
}

/// Rule 3d: a sync carries the parent's current version, read when its
/// take begins.
#[test]
fn r3_a_sync_carries_the_parents_current_version() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        w.save(&m1(), "a", 0);
        let m2 = w.save(&m1(), "a", 0);
        next(&mut w, Tree::Fork(f));
        assert!(has(&w, Tree::Fork(f), "a", &m2));
        drain(&mut w);
    }
}

/// Coherence finding 1, re-measured: crossing syncs and landings. The
/// sandbox never merges main (it rebases), and every merge runs from an
/// explicit base, so git never finds two merge bases; the sandbox's line
/// main decided against never comes back, and its later edit over main's
/// line wins.
#[test]
fn finding_1_crossing_syncs_and_landings_no_longer_keep_a_decided_line() {
    let mut w = world(HistoryPolicy::FullLineage);
    let f = w.create_fork(Tree::Main);
    let x = w.save(&Client::Fork(f), "a", 0);
    let m = w.save(&m1(), "a", 0);
    next(&mut w, Tree::Fork(f));
    next(&mut w, Tree::Main);
    next(&mut w, Tree::Fork(f));
    assert!(has(&w, Tree::Fork(f), "a", &m) && !has(&w, Tree::Fork(f), "a", &x));
    let y = w.save(&Client::Fork(f), "a", 0);
    drain(&mut w);
    assert!(has(&w, Tree::Main, "a", &y) && !has(&w, Tree::Main, "a", &x));
    let merges = git(w.repo(), &["rev-list", "--merges", "--all"]);
    for c in merges.lines() {
        assert_ne!(
            show(&w, c, "%(trailers:key=Agentfs-Cause,valueonly)"),
            "Sync"
        );
    }
}

/// Rule 3e under the variant (finding 6, now the accepted cost): edits on
/// neighbouring lines are one git hunk. The sandbox edits line 0 and main
/// line 1: the hunk conflicts, main's side wins it whole, and the
/// sandbox's line 0 is lost with a collision naming it. One unchanged line
/// apart (lines 0 and 2), both apply.
#[test]
fn r3_neighbouring_lines_are_one_hunk() {
    for history in BOTH {
        for (theirs, ours, clean) in [(0, 1, false), (0, 2, true)] {
            let mut w = world(history);
            let f = w.create_fork(Tree::Main);
            let x = w.save(&Client::Fork(f), "a", theirs);
            let m = w.save(&m1(), "a", ours);
            drain(&mut w);
            assert!(has(&w, Tree::Main, "a", &m));
            assert_eq!(
                has(&w, Tree::Main, "a", &x),
                clean,
                "{history:?} {theirs},{ours}"
            );
            assert_eq!(w.collisions.is_empty(), clean);
        }
    }
}

/// A transaction's commit changes lines 0 and 4 of `a`; main changed line
/// 0 and, cleanly, line 2. Per hunk: main's line 0 wins, line 4 applies,
/// main's line 2 stays.
#[test]
fn r3_collision_scope_per_hunk_keeps_clean_lines() {
    for history in BOTH {
        let (w, [x, y, m, k]) = clean_lines(history, CollisionScope::Hunk);
        assert!(has(&w, Tree::Main, "a", &m) && has(&w, Tree::Main, "a", &y));
        assert!(has(&w, Tree::Main, "a", &k), "{history:?}: the clean line");
        let lost: Vec<File> = hunks(&w).into_iter().map(|h| h.1).collect();
        assert_eq!(lost, [vec![x]], "{history:?}");
    }
}

/// Coherence finding 3, re-measured: per file, main's whole file wins, so
/// the sandbox's clean line 4 is dropped too.
#[test]
#[ignore = "coherence finding 3: per file, the parent's winning file reverts the fork's newer clean lines"]
fn finding_3_per_file_keeps_clean_lines() {
    for history in BOTH {
        let (w, [_, y, _, _]) = clean_lines(history, CollisionScope::File);
        assert!(
            has(&w, Tree::Main, "a", &y),
            "{history:?}: the fork's clean line {y} is lost with the file"
        );
    }
}

/// Main gives every line of `p` its own bytes, so git's diff aligns them
/// by content, not among identical blank lines (see the costs in MODEL.md).
fn distinct(w: &mut World, p: &str) {
    for i in 0..6 {
        w.save(&m1(), p, i);
    }
    drain(w);
}

fn clean_lines(history: HistoryPolicy, scope: CollisionScope) -> (World, [String; 4]) {
    let mut w = World::new(Policies::new(history, scope));
    distinct(&mut w, "a");
    let f = w.create_fork(Tree::Main);
    w.begin(f).unwrap();
    let x = w.save(&Client::Fork(f), "a", 0);
    let y = w.save(&Client::Fork(f), "a", 4);
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::clean()));
    let m = w.save(&m1(), "a", 0);
    let k = w.save(&m1(), "a", 2);
    drain(&mut w);
    (w, [x, y, m, k])
}

// ── rule 4: nothing waits; nested forks ─────────────────────────────────

/// Rule 4: a collision holds nothing: the next commit lands.
#[test]
fn r4_nothing_waits_behind_a_collision() {
    let mut w = world(HistoryPolicy::FullLineage);
    let f = w.create_fork(Tree::Main);
    w.save(&Client::Fork(f), "a", 0);
    let later = w.save(&Client::Fork(f), "b", 0);
    w.save(&m1(), "a", 0);
    next(&mut w, Tree::Main);
    assert_eq!((w.collisions.len(), w.queue_len(Tree::Main)), (1, 1));
    next(&mut w, Tree::Main);
    assert!(has(&w, Tree::Main, "b", &later));
    drain(&mut w);
}

/// Rule 4, nested forks: G over F over main, F the parent of G. A landing
/// into a fork is always a merge (a fork's branch is rebased by its own
/// syncs, so a fast-forwarded child commit would be rebased away from it);
/// its rebased copy keeps G's commit as second parent.
#[test]
fn r4_nested_forks_follow_the_same_rules() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        let g = w.create_fork(Tree::Fork(f));
        let (cf, cg) = (Client::Fork(f), Client::Fork(g));
        let ga = w.save(&cg, "a", 0);
        let fa = w.save(&cf, "a", 0);
        next(&mut w, Tree::Fork(f)); // G's a lands in F: F's line wins.
        assert!(has(&w, Tree::Fork(f), "a", &fa) && !has(&w, Tree::Fork(f), "a", &ga));
        assert!(w.collisions.iter().any(|c| c.tree == Tree::Fork(f)));
        let gb = w.save(&cg, "b", 0);
        let mb = w.save(&m1(), "b", 0);
        let gc = w.save(&cg, "c", 0);
        drain(&mut w);
        assert!(has(&w, Tree::Main, "b", &mb) && !has(&w, Tree::Main, "b", &gb));
        assert!(has(&w, Tree::Main, "c", &gc) && has(&w, Tree::Main, "a", &fa));
        for t in [Tree::Fork(f), Tree::Fork(g)] {
            assert!(has(&w, t, "b", &mb) && has(&w, t, "a", &fa), "{t:?}");
        }
        w.end_client(g);
        w.end_client(f);
        drain(&mut w);
        assert_eq!(w.forks[&f].life, Life::Dropped);
    }
}

// ── rule 5: transactions ────────────────────────────────────────────────

/// Rule 5, a clean commit: the program reads its staged edits on a frozen
/// snapshot while the fork keeps syncing; the commit lands named for it.
#[test]
fn r5_transaction_commits_clean() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        let c = Client::Fork(f);
        w.begin(f).unwrap();
        assert_eq!(w.begin(f), Err(ModelError::InTransaction));
        let sa = w.save(&c, "a", 0);
        let sb = w.save(&c, "b", 0);
        let m = w.save(&m1(), "c", 0);
        drain(&mut w);
        assert!(w.client_reads(&c, "a").unwrap().contains(&sa));
        assert!(
            !has(&w, Tree::Fork(f), "a", &sa),
            "the fork's tree lacks it"
        );
        assert!(has(&w, Tree::Fork(f), "c", &m), "the fork got the sync");
        assert_eq!(w.commit_txn(f), Ok(TxnOutcome::clean()));
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &sa) && has(&w, Tree::Main, "b", &sb));
        assert_eq!(w.commit_txn(f), Err(ModelError::NoTransaction));
    }
}

/// Rule 5: a sync brings main's concurrent line into the fork after begin;
/// the commit's hunk loses to the fork's (its parent's) line, and the
/// answer names that hunk.
#[test]
fn r5_transaction_commit_reports_the_overwritten_hunk() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        w.begin(f).unwrap();
        let s = w.save(&Client::Fork(f), "a", 0);
        let m = w.save(&m1(), "a", 0);
        drain(&mut w);
        let out = w.commit_txn(f).unwrap();
        let TxnOutcome::Committed { overwritten } = out else {
            panic!("committed")
        };
        assert_eq!(
            overwritten
                .iter()
                .map(|o| (o.staged.clone(), o.kept.clone()))
                .collect::<Vec<_>>(),
            [(vec![s.clone()], vec![m.clone()])]
        );
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &m) && !has(&w, Tree::Main, "a", &s));
    }
}

/// Rule 5: rollback and the client's death leave nothing in any tree or
/// reachable history.
#[test]
fn r5_transaction_rollback_and_client_death_leave_nothing() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        w.begin(f).unwrap();
        let s = w.save(&Client::Fork(f), "a", 0);
        w.rollback(f).unwrap();
        ok(&mut w);
        assert!(!has(&w, Tree::Fork(f), "a", &s));
        w.begin(f).unwrap();
        w.save(&Client::Fork(f), "b", 0);
        w.end_client(f);
        drain(&mut w);
        assert_eq!(w.discarded.len(), 2, "{history:?}");
    }
}

/// Rule 5: a program holding its own file open cannot commit.
#[test]
fn r5_open_files_and_transactions() {
    let mut w = world(HistoryPolicy::FullLineage);
    let f = w.create_fork(Tree::Main);
    w.begin(f).unwrap();
    let h = w.open_write(&Client::Fork(f), "a");
    w.write(h, Write::At(0));
    assert_eq!(w.commit_txn(f), Err(ModelError::OpenFiles));
    w.close(h);
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::clean()));
    drain(&mut w);
}

// ── rule 6: attribution ─────────────────────────────────────────────────

/// Rule 3, nested: a sync rebases only the fork's own commits since its
/// last sync (`git rebase --onto`), never its parent's. f2 synced onto f1's
/// append x; f1 deletes x and rebases both commits onto main; f2's next
/// sync leaves x out, as f1 does.
#[test]
fn r3_a_nested_sync_never_replays_its_parents_rebased_commits() {
    for history in BOTH {
        let mut w = world(history);
        let f1 = w.create_fork(Tree::Main);
        let f2 = w.create_fork(Tree::Fork(f1));
        let x = call(&mut w, &Client::Fork(f1), "a", Write::Append).unwrap();
        next(&mut w, Tree::Fork(f2));
        assert!(has(&w, Tree::Fork(f2), "a", &x));
        let n = w.read(Tree::Fork(f1), "a").len();
        call(&mut w, &Client::Fork(f1), "a", Write::Delete(n - 1));
        w.save(&m1(), "c", 0);
        next(&mut w, Tree::Fork(f1));
        assert!(!has(&w, Tree::Fork(f1), "a", &x));
        while w.queue_len(Tree::Fork(f2)) > 0 {
            next(&mut w, Tree::Fork(f2));
        }
        assert!(
            !has(&w, Tree::Fork(f2), "a", &x),
            "{history:?}: f2 shows x, which f1 deleted"
        );
        drain(&mut w);
    }
}

/// Rule 6 under the variant: a sync makes no merge. A sandbox commit main
/// has not taken is replayed onto main's head as a copy that keeps its
/// writer and cause and names its original (`Agentfs-Rebased`); the
/// pre-rebase head stays reachable under `refs/rebased/<n>`.
#[test]
fn r6_a_sync_rebases_and_the_copy_names_its_original() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        w.save(&Client::Fork(f), "a", 0);
        let orig = w.head(Tree::Fork(f));
        w.save(&m1(), "b", 0);
        next(&mut w, Tree::Fork(f));
        let copy = w.head(Tree::Fork(f));
        assert_ne!(copy, orig);
        assert_eq!(show(&w, &copy, "%P"), w.main_head, "on main's head");
        assert_eq!(w.writer_of_commit(&copy), "c1");
        assert_eq!(
            show(&w, &copy, "%(trailers:key=Agentfs-Rebased,valueonly)"),
            orig
        );
        assert_eq!(git(w.repo(), &["rev-parse", "refs/rebased/1"]), orig);
        drain(&mut w);
        let merges = git(w.repo(), &["rev-list", "--merges", &w.head(Tree::Fork(f))]);
        for c in merges.lines() {
            assert_ne!(
                show(&w, c, "%(trailers:key=Agentfs-Writer,valueonly)"),
                "agentfs"
            );
        }
    }
}

// ── rule 7: history settings ────────────────────────────────────────────

/// Rule 7, `Squashed`: a sandbox's waiting saves land as one changeset.
#[test]
fn r7_squashed_saves_land_as_one_changeset() {
    let mut w = world(HistoryPolicy::Squashed);
    let f = w.create_fork(Tree::Main);
    let a = w.save(&Client::Fork(f), "a", 0);
    let b = w.save(&Client::Fork(f), "b", 0);
    assert_eq!(w.queue_len(Tree::Main), 2);
    assert_eq!(w.step(&Step::Next(Tree::Main)), 2);
    take(&mut w, &Step::Finish(Tree::Main));
    assert_eq!(w.squashed_runs, 1);
    assert!(has(&w, Tree::Main, "a", &a) && has(&w, Tree::Main, "b", &b));
    drain(&mut w);
}

/// Rule 7 and the losers: under both settings every losing commit stays
/// reachable and is pushed; main's stored history holds only main's own
/// commits under `Squashed`.
#[test]
fn r7_history_setting_and_losers_stay_reachable() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        w.save(&Client::Fork(f), "a", 0);
        w.save(&m1(), "a", 0);
        drain(&mut w);
        let loser = &w.collisions[0].loser;
        assert_eq!(
            w.in_history(loser, &w.main_head),
            history == HistoryPolicy::FullLineage
        );
        assert!(w.is_ancestor(loser, &w.main_head), "lineage keeps it");
    }
}

/// Convergence: once writes stop and the queues drain, every fork equals
/// main.
#[test]
fn convergence_when_writes_stop() {
    for history in BOTH {
        let mut w = world(history);
        let (f, g) = (w.create_fork(Tree::Main), w.create_fork(Tree::Main));
        w.save(&Client::Fork(f), "a", 0);
        w.save(&Client::Fork(g), "a", 0);
        w.save(&m1(), "a", 3);
        w.save(&Client::Fork(g), "b", 1);
        drain(&mut w);
    }
}

// ── rule 8: a save records the write calls ──────────────────────────────

/// The sandbox's program read `a` before main's m reached it by sync.
fn read_then_sync(history: HistoryPolicy) -> (World, File, String, u32) {
    let mut w = world(history);
    let f = w.create_fork(Tree::Main);
    let o = w.save(&m1(), "a", 0);
    drain(&mut w);
    let read = w.client_reads(&Client::Fork(f), "a").unwrap();
    assert!(read.contains(&o));
    let m = w.save(&m1(), "a", 0); // over o
    next(&mut w, Tree::Fork(f));
    assert!(has(&w, Tree::Fork(f), "a", &m));
    (w, read, m, f)
}

/// Rule 8, an append after the sync keeps the synced line.
#[test]
fn r8_an_append_after_the_sync_keeps_the_synced_line() {
    for history in BOTH {
        let (mut w, _, m, f) = read_then_sync(history);
        let x = call(&mut w, &Client::Fork(f), "a", Write::Append).unwrap();
        assert_eq!(w.read(Tree::Fork(f), "a").last(), Some(&x));
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &m) && has(&w, Tree::Main, "a", &x));
        assert!(w.collisions.is_empty(), "{history:?}");
    }
}

/// Rule 8, a write at an offset elsewhere keeps the synced line.
#[test]
fn r8_an_offset_write_elsewhere_keeps_the_synced_line() {
    for history in BOTH {
        let (mut w, _, m, f) = read_then_sync(history);
        let y = call(&mut w, &Client::Fork(f), "a", Write::At(2)).unwrap();
        drain(&mut w);
        for t in [Tree::Main, Tree::Fork(f)] {
            let a = w.read(t, "a");
            assert_eq!((&a[0], &a[2]), (&m, &y), "{t:?}");
        }
        assert!(w.collisions.is_empty(), "{history:?}");
    }
}

/// Rule 8, a stale truncate and rewrite: the program writes back the file
/// it read before the sync, o's bytes over m. In its own tree that is its
/// write. Upstream, git reads the change from the save's first parent
/// (which held m): o over m, a change main's head (still m) agrees with as
/// the base, so it applies with no collision, credited to the program.
#[test]
fn r8_a_stale_rewrite_overwrites_the_synced_line_and_history_keeps_it() {
    for history in BOTH {
        let (mut w, read, m, f) = read_then_sync(history);
        let m_commit = w.main_head.clone();
        assert_eq!(
            call(&mut w, &Client::Fork(f), "a", Write::Rewrite(read.clone())),
            None
        );
        assert_eq!(w.read(Tree::Fork(f), "a"), read, "o's bytes, back");
        drain(&mut w);
        assert_eq!(w.read(Tree::Main, "a"), read, "{history:?}");
        assert!(w.collisions.is_empty(), "{history:?}: no collision");
        assert!(w.in_history(&m_commit, &w.main_head), "m stays in history");
        assert!(w.file(&m_commit, "a").unwrap().contains(&m));
    }
}

/// Rule 8, `O_APPEND`: two of main's clients append to one file at once;
/// each lands after the end as it is at its close.
#[test]
fn r8_concurrent_appends_never_overwrite_each_other() {
    for history in BOTH {
        let mut w = world(history);
        let m2 = Client::Main("m2".into());
        let (a, b) = (w.open_write(&m1(), "a"), w.open_write(&m2, "a"));
        let xa = w.write(a, Write::Append).unwrap();
        let xb = w.write(b, Write::Append).unwrap();
        w.close(b);
        ok(&mut w);
        w.close(a);
        ok(&mut w);
        let f = w.read(Tree::Main, "a");
        assert_eq!(&f[f.len() - 2..], [xb, xa], "{history:?}: in close order");
        drain(&mut w);
    }
}

// ── rule 9: a take locks the files it overwrites ────────────────────────

/// Rule 9: main's sync waits while the sandbox's program has `a` open for
/// writing; the close wakes it.
#[test]
fn r9_a_sync_waits_for_an_open_writer_then_applies() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        let h = w.open_write(&Client::Fork(f), "a");
        let m = w.save(&m1(), "a", 0);
        take(&mut w, &Step::Next(Tree::Fork(f)));
        assert_eq!(w.waits()[&Tree::Fork(f)], BTreeSet::from(["a".to_string()]));
        assert!(!has(&w, Tree::Fork(f), "a", &m), "{history:?}: not written");
        let x = w.write(h, Write::At(2)).unwrap();
        w.close(h);
        ok(&mut w);
        assert!(w.waits().is_empty(), "the close woke it");
        next(&mut w, Tree::Fork(f));
        let a = w.read(Tree::Fork(f), "a");
        assert_eq!((&a[0], &a[2]), (&m, &x));
        drain(&mut w);
    }
}

/// Rule 9: while a sync holds `a`, a new write-open of `a` waits; the
/// release grants it.
#[test]
fn r9_a_write_open_waits_while_a_sync_holds_the_file() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        let m = w.save(&m1(), "a", 0);
        take(&mut w, &Step::Next(Tree::Fork(f)));
        let h = w.open_write(&Client::Fork(f), "a");
        ok(&mut w);
        assert!(w.handle(h).waiting, "{history:?}: the open waits");
        take(&mut w, &Step::Finish(Tree::Fork(f)));
        assert!(!w.handle(h).waiting, "the release granted it");
        let x = w.write(h, Write::Append).unwrap();
        w.close(h);
        let a = w.read(Tree::Fork(f), "a");
        assert_eq!((&a[0], a.last()), (&m, Some(&x)));
        drain(&mut w);
    }
}

/// Rule 9: a client's death closes its files (saving) and wakes the take.
#[test]
fn r9_client_death_releases_its_locks() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        let h = w.open_write(&Client::Fork(f), "a");
        let x = w.write(h, Write::At(2)).unwrap();
        w.save(&m1(), "a", 0);
        take(&mut w, &Step::Next(Tree::Fork(f)));
        assert!(w.waits().contains_key(&Tree::Fork(f)));
        w.end_client(f);
        ok(&mut w);
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &x), "{history:?}: saved at death");
    }
}

// ── rule 11: deletes and renames, by git's rename detection ─────────────

/// Rule 11: the sandbox unlinks `a` while main edits it: a modify/delete
/// path conflict; main's side (its edit) wins, the file stays.
#[test]
fn r11_an_unlink_against_a_concurrent_edit_the_parent_wins() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        w.unlink(&Client::Fork(f), "a");
        let m = w.save(&m1(), "a", 0);
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &m) && has(&w, Tree::Fork(f), "a", &m));
        assert!(w.collisions.iter().all(|c| c.by_file), "{history:?}");
        assert_eq!(w.collisions.len(), 1);
    }
}

/// Rule 11: main unlinks `a` while the sandbox edits it: main's unlink
/// wins, the sandbox's edit is lost and named.
#[test]
fn r11_an_edit_of_a_file_the_parent_unlinked_loses() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        let x = w.save(&Client::Fork(f), "a", 0);
        w.unlink(&m1(), "a");
        drain(&mut w);
        assert!(w.file(&w.main_head, "a").is_none());
        assert!(
            w.collisions.iter().any(|c| c.theirs.contains(&x)),
            "{history:?}"
        );
    }
}

/// Rule 11: an unlink after the sync saw main's line and wins.
#[test]
fn r11_an_unlink_after_the_sync_wins() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        w.save(&m1(), "a", 0);
        next(&mut w, Tree::Fork(f));
        w.unlink(&Client::Fork(f), "a");
        drain(&mut w);
        assert!(w.file(&w.main_head, "a").is_none() && w.collisions.is_empty());
    }
}

/// Rule 11: a rename and a concurrent edit both apply: git's rename
/// detection carries the edit to the new path.
#[test]
fn r11_a_rename_and_a_concurrent_edit_both_apply() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        w.rename(&Client::Fork(f), "a", "e1");
        let m = w.save(&m1(), "a", 0);
        drain(&mut w);
        assert!(has(&w, Tree::Main, "e1", &m) && w.file(&w.main_head, "a").is_none());
        assert!(w.collisions.is_empty(), "{history:?}");
    }
}

/// Rule 11: two renames of one file: a rename/rename path conflict; main's
/// name wins, the sandbox's goes.
#[test]
fn r11_concurrent_renames_the_parent_wins() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        w.rename(&Client::Fork(f), "a", "e1");
        w.rename(&m1(), "a", "e2");
        drain(&mut w);
        let paths = w.paths(&w.main_head);
        assert!(
            paths.contains(&"e2".into()) && !paths.contains(&"e1".into()),
            "{history:?}"
        );
    }
}

/// Rule 11 under git: a rename over `b` against main's concurrent edit of
/// `b`: `b` is a modified path on both sides, one hunk spanning the file;
/// main's `b` wins, and `a`, which the rename removed cleanly, is gone:
/// its content survives only in the losing commit, which the collision
/// names. (Before the variant, the rename-over lost whole and `a` stayed.)
#[test]
fn r11_a_rename_over_against_an_edit_loses_the_renamed_file() {
    for history in BOTH {
        let mut w = world(history);
        let f = w.create_fork(Tree::Main);
        let x = w.save(&Client::Fork(f), "a", 0);
        next(&mut w, Tree::Main);
        w.rename(&Client::Fork(f), "a", "b");
        let m = w.save(&m1(), "b", 0);
        drain(&mut w);
        assert!(has(&w, Tree::Main, "b", &m), "{history:?}");
        assert!(w.file(&w.main_head, "a").is_none() && !has(&w, Tree::Main, "b", &x));
        assert!(w.collisions.iter().any(|c| c.theirs.contains(&x)));
    }
}

/// Rule 11: an open file follows a rename in its tree; one unlinked while
/// open saves nothing.
#[test]
fn r11_an_open_file_follows_a_rename_and_an_unlinked_one_saves_nothing() {
    let mut w = world(HistoryPolicy::FullLineage);
    let h = w.open_write(&m1(), "a");
    let x = w.write(h, Write::At(0)).unwrap();
    w.rename(&m1(), "a", "e1");
    w.close(h);
    ok(&mut w);
    assert!(has(&w, Tree::Main, "e1", &x));
    let h = w.open_write(&m1(), "b");
    w.write(h, Write::At(0));
    w.unlink(&m1(), "b");
    let before = w.main_head.clone();
    w.close(h);
    assert_eq!(w.main_head, before);
    drain(&mut w);
}

/// Coherence finding 13: git pairs a rename by content. Two files with the
/// same content (here every root file, without its naming line) are told
/// apart by nothing: main renames `b` and `d`; the sandbox edits `d`; the
/// landing carries the sandbox's edit into whichever renamed file git
/// paired with `d`. The test asserts the edit follows `d`.
#[test]
#[ignore = "coherence finding 13: git's rename detection pairs identical files arbitrarily, so an edit can follow the wrong file"]
fn finding_13_rename_detection_pairs_identical_files_by_content() {
    let mut w = world(HistoryPolicy::FullLineage);
    for p in ["b", "d"] {
        let content: File = vec!["-".into(); 6];
        let h = w.open_write(&m1(), p);
        w.write(h, Write::Rewrite(content));
        w.close(h);
    }
    let f = w.create_fork(Tree::Main);
    let x = call(&mut w, &Client::Fork(f), "d", Write::At(0)).unwrap();
    w.save(&m1(), "b", 3);
    w.rename(&m1(), "b", "e1");
    w.rename(&m1(), "d", "e2");
    w.settle();
    assert!(
        has(&w, Tree::Main, "e2", &x),
        "the edit of d follows d to e2"
    );
}

/// Coherence finding 14: the sync decided before the parent's queue. The
/// sandbox writes x on line 1; main writes m on line 1; the sandbox syncs
/// and takes m (its x leaves); main puts line 1 back to its old bytes; main's
/// queue lands x's commit, which now conflicts with nothing, so x applies;
/// the sandbox syncs and shows x again: x, m, x.
#[test]
#[ignore = "coherence finding 14: a sync's rebase takes the parent's side before the parent's queue decides, which can decide otherwise"]
fn finding_14_a_sync_decides_before_the_parents_queue() {
    let mut w = world(HistoryPolicy::FullLineage);
    let f = w.create_fork(Tree::Main);
    let x = w.save(&Client::Fork(f), "a", 1);
    let m = w.save(&m1(), "a", 1);
    next(&mut w, Tree::Fork(f));
    assert!(has(&w, Tree::Fork(f), "a", &m) && !has(&w, Tree::Fork(f), "a", &x));
    let h = w.open_write(&m1(), "a");
    w.write(h, Write::Delete(1));
    w.write_text(h, Write::Insert(1), "-");
    w.close(h);
    drain(&mut w);
}

/// Coherence finding 14, its other face: the parent's own write is lost
/// with no collision. Main has o; the sandbox writes x over o; main writes
/// m over o, then a program of main's rewrites the file from a read taken
/// before m (o's bytes, back: its new write). The landing's three-way merge
/// sees main's line equal to the base (o), so the sandbox's x applies over
/// main's newer write: the parent lost, and nothing names it.
#[test]
#[ignore = "coherence finding 14: a parent's write back to the base's bytes is invisible to a three-way merge"]
fn finding_14_the_parents_write_back_to_the_base_is_lost_silently() {
    let mut w = world(HistoryPolicy::FullLineage);
    w.save(&m1(), "a", 0);
    drain(&mut w);
    let read = w.read(Tree::Main, "a");
    let f = w.create_fork(Tree::Main);
    w.save(&Client::Fork(f), "a", 0);
    w.save(&m1(), "a", 0);
    call(&mut w, &m1(), "a", Write::Rewrite(read));
    drain(&mut w);
}
