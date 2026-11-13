//! One or more scenarios per rule of MODEL.md, named `r<N>_...` after the
//! rule they show. Every scenario checks every invariant after each step.
//! A test marked `#[ignore = "coherence finding N: ..."]` asserts what the
//! rules intend but do not deliver; run it with `--ignored`.

use agentfs_model::calls::{Client, TreeId};
use agentfs_model::check::Checked;
use agentfs_model::content::{BLANK, Content, Layout, Line, State};
use agentfs_model::git::{git, with_reversed_dates};
use agentfs_model::recording::{Target, Write};
use agentfs_model::service::Step;
use agentfs_model::world::{
    By, Direction, Life, LineEdit, MODEL_ROOT, MergeRule, ModelError, Overwritten, Policies,
    TreeNames, TxnEnd, TxnOutcome, World, decisions_in,
};
use std::collections::BTreeSet;

fn world() -> Checked {
    Checked::new(Policies::default(), TreeNames::new(MODEL_ROOT))
}

/// Every invariant, now.
fn ok(w: &mut Checked) {
    if let Err(e) = w.check() {
        panic!("{e}\nlog:\n  {}", w.log.join("\n  "));
    }
}

/// One step ([`Checked::step_checked`]), then every invariant.
fn take(w: &mut Checked, s: &Step) {
    if let Err(e) = w.step_checked(s) {
        panic!("{e}\nlog:\n  {}", w.log.join("\n  "));
    }
    ok(w);
}

/// One queued change taken whole (rule 9: one step), then every invariant.
fn next(w: &mut Checked, t: TreeId) {
    take(w, &Step::Next(t));
    assert!(w.waits().get(&t).is_none(), "{t:?}'s queue waits");
}

/// Every step until none is left, checking after each; then convergence.
fn drain(w: &mut Checked) {
    ok(w);
    while let Some(s) = w.pending().into_iter().next() {
        w.note_step(&s);
        take(w, &s);
    }
    if let Err(e) = w.converged() {
        panic!("{e}\nlog:\n  {}", w.log.join("\n  "));
    }
}

fn m1() -> Client {
    Client::Root("m1".into())
}

fn has(w: &World, t: TreeId, p: &str, token: &str) -> bool {
    w.read(t, p).contains(token)
}

fn show(w: &World, rev: &str, format: &str) -> String {
    git(w.repo(), &["log", "-1", &format!("--format={format}"), rev])
}

/// The writers of the root's first-parent commits, newest first.
fn root_writers(w: &World) -> Vec<String> {
    git(
        w.repo(),
        &[
            "log",
            "--first-parent",
            "--format=%(trailers:key=Agentfs-Writer,valueonly)",
            &w.head(TreeId::ROOT),
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
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let mine = w.save(&Client::Fork(f), "a", 0);
    let theirs = w.save(&m1(), "b", 0);
    let both = w.save(&m1(), "a", 1);
    ok(&mut w);
    assert!(has(&w, f, "a", &mine) && has(&w, TreeId::ROOT, "b", &theirs));
    assert!(
        !has(&w, TreeId::ROOT, "a", &mine),
        "landing is a queued step"
    );
    drain(&mut w);
    assert!(has(&w, TreeId::ROOT, "a", &mine) && has(&w, TreeId::ROOT, "a", &both));
    assert!(has(&w, f, "b", &theirs));
}

// ── rule 2: one landing queue per tree; no clock ────────────────────────

/// Rule 2: the root's queue fixes the order its children's commits land in,
/// whatever order the scheduler visits the forks.
#[test]
fn r2_one_queue_fixes_the_landing_order() {
    let mut w = world();
    let (f, g) = (w.create_fork(TreeId::ROOT), w.create_fork(TreeId::ROOT));
    w.save(&Client::Fork(f), "a", 0);
    w.save(&Client::Fork(g), "b", 0);
    w.save(&Client::Fork(f), "c", 0);
    let order: Vec<String> = w
        .queue(TreeId::ROOT)
        .iter()
        .map(|e| format!("{e:?}"))
        .collect();
    assert_eq!(order.len(), 3);
    for _ in 0..3 {
        next(&mut w, TreeId::ROOT);
    }
    assert_eq!(root_writers(&w)[..3], ["c1", "c2", "c1"]);
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
        let mut w = world();
        let f = w.create_fork(TreeId::ROOT);
        let g = w.create_fork(f);
        w.save(&Client::Fork(f), "a", 0);
        w.save(&Client::Fork(g), "c", 0);
        w.save(&m1(), "a", 0);
        next(&mut w, f);
        next(&mut w, TreeId::ROOT);
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
            agentfs_model::digest::canonical_text(&w),
        )
    };
    let pinned = run();
    assert_eq!(pinned, run());
    assert_eq!(pinned.1, with_reversed_dates(run).1);
    // Every source file of the model, read at test time by walking `src`, so a new module
    // or unit is scanned too.
    let mut units = Vec::new();
    let mut dirs = vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(&d).expect("a source directory") {
            let p = e.expect("an entry of src").path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                units.push(p);
            }
        }
    }
    units.sort();
    for unit in [
        "world.rs",
        "recording.rs",
        "queue.rs",
        "referee/mod.rs",
        "check/lineage.rs",
    ] {
        assert!(
            units.iter().any(|u| u.ends_with(unit)),
            "src/{unit} is scanned: {units:?}"
        );
    }
    let mut src = String::new();
    for u in units {
        src.push_str(&std::fs::read_to_string(&u).expect("a source file"));
    }
    for clock in ["SystemTime", "Instant", "chrono", "UNIX_EPOCH"] {
        assert!(!src.contains(clock), "the model reads a clock: {clock}");
    }
}

// ── rule 3: the upstream's queue decides; the upstream's version wins ─────

/// Rule 3: an edit whose base is the root's current version replaces it: the root's
/// head becomes the fork's commit.
#[test]
fn r3_fast_forward_when_the_base_is_current() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let tok = w.save(&Client::Fork(f), "a", 0);
    let fc = w.head(f);
    next(&mut w, TreeId::ROOT);
    assert!(has(&w, TreeId::ROOT, "a", &tok));
    assert_eq!(w.head(TreeId::ROOT), fc);
    assert_eq!(w.fast_forwards, 1);
    drain(&mut w);
}

/// Rule 3, a fork's landing into the root, the one place their collision is
/// decided: the root's concurrent same-line line wins ("i think the updated
/// main should win in a conflict"). The collision names the fork's lost
/// line and the root's line that won; the fork's losing commit stays
/// reachable under the collision's ref. The sync the root queued into the fork
/// when it wrote its line is processed after the landing: it carries
/// the root's decided version, so the fork takes m.
#[test]
fn r3_landing_collision_the_upstream_wins() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let x = w.save(&Client::Fork(f), "a", 0);
    let fc = w.head(f);
    let m = w.save(&m1(), "a", 0);
    let mc = w.head(TreeId::ROOT);
    next(&mut w, TreeId::ROOT);
    assert!(has(&w, TreeId::ROOT, "a", &m) && !has(&w, TreeId::ROOT, "a", &x));
    let c = w.collisions.last().expect("a collision").clone();
    assert_eq!(
        (c.tree, c.direction, c.lost.clone(), c.won.clone()),
        (
            TreeId::ROOT,
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
    assert_eq!(w.queue_len(f), 2, "two syncs queued");
    next(&mut w, f); // the sync queued when m was written
    assert!(has(&w, f, "a", &m), "decided");
    drain(&mut w);
    assert!(has(&w, f, "a", &m) && has(&w, TreeId::ROOT, "a", &m));
    assert_eq!(w.collisions.len(), 1, "recorded once");
}

/// Rule 3, no flip-flop (coherence finding 4, resolved): the root's sync
/// reaches the fork before the fork's own concurrent edit lands. The
/// sync decides nothing, so the fork keeps its x; the root's queue then
/// decides, the root's m wins, and the fork takes m. Its line changes once,
/// x then m, never x, m, x (the `no_flicker` invariant checks this in every
/// run).
#[test]
fn r3_a_fork_never_flips_back_to_its_own_edit() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let x = w.save(&Client::Fork(f), "a", 0);
    let m = w.save(&m1(), "a", 0);
    next(&mut w, f); // the root's sync, before x lands
    assert!(has(&w, f, "a", &x), "the fork decided");
    assert!(w.collisions.is_empty(), "a sync records nothing");
    let head = w.head(f);
    assert_eq!(
        show(&w, &head, "%(trailers:key=Agentfs-Cause,valueonly)"),
        "Sync"
    );
    next(&mut w, TreeId::ROOT); // the root's queue decides
    assert!(has(&w, TreeId::ROOT, "a", &m) && !has(&w, TreeId::ROOT, "a", &x));
    drain(&mut w);
    assert!(has(&w, f, "a", &m));
    let records: Vec<(TreeId, Vec<String>)> = w
        .collisions
        .iter()
        .map(|c| (c.tree, c.lost.clone()))
        .collect();
    assert_eq!(records, [(TreeId::ROOT, vec![x.clone()])]);
}

/// Rule 3, an edit on a line the upstream has not decided: the fork
/// writes y over its own x while the root's concurrent m waits for x's
/// landing. y never saw m, so it collides with m too, and the root's m wins
/// again. The sync between them showed the fork nothing of m: what a
/// fork wrote over is read per line, not from which commits its version
/// merged.
#[test]
fn r3_an_edit_over_an_undecided_line_still_collides() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let x = w.save(&Client::Fork(f), "a", 0);
    let m = w.save(&m1(), "a", 0);
    next(&mut w, f); // a Sync merge that keeps x
    let y = w.save(&Client::Fork(f), "a", 0); // over x, not m
    drain(&mut w);
    assert!(has(&w, TreeId::ROOT, "a", &m));
    let lost: Vec<String> = w.collisions.iter().flat_map(|c| c.lost.clone()).collect();
    assert_eq!(lost, [x.clone(), y.clone()]);
}

/// Rule 3, an edit made on the upstream's current version replaces it: the
/// fork's x loses to the root's m and the fork takes m; it then writes
/// y over m, and y replaces m in the root without a collision.
#[test]
fn r3_an_edit_over_the_decided_line_replaces_it() {
    let (w, y) = crossing(MergeRule::PerLine);
    assert!(has(&w, TreeId::ROOT, "a", &y));
    assert_eq!(w.collisions.len(), 1);
}

/// Rule 3c, a save after the sync (owner, 2026-11-03: "if the program
/// saves after the sync has landed, then arguably the sandbox was the last
/// writer and its change should take effect"). A save's first parent is
/// the tree's head when it is made, so whatever the tree held then counts
/// as seen. The fork has its own unlanded line b, so the root's m arrives
/// as a sync commit (a `Sync` merge naming `agentfs`, the root's commit
/// second); the fork's y on m's line is written over m and replaces it
/// in the root by lineage, with no collision.
#[test]
fn r3_a_save_after_the_sync_wins_upstream_by_lineage() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    w.save(&Client::Fork(f), "b", 0);
    let m = w.save(&m1(), "a", 0);
    next(&mut w, f); // the sync lands in the fork
    let synced = w.head(f);
    assert_eq!(
        show(&w, &synced, "%(trailers:key=Agentfs-Cause,valueonly)"),
        "Sync",
        "a sync commit, the fork has an unlanded line"
    );
    assert!(has(&w, f, "a", &m));
    let y = w.save(&Client::Fork(f), "a", 0);
    assert_eq!(
        show(&w, &w.head(f), "%P").split(' ').next(),
        Some(synced.as_str())
    );
    drain(&mut w);
    assert!(has(&w, TreeId::ROOT, "a", &y), "the fork's later save wins");
    assert!(w.collisions.is_empty(), "written over m, no collision");
}

/// Rule 3b, a save before the sync arrives: the root writes m first, but the
/// fork saves x on the same line while the root's sync still waits in the
/// fork's queue. x's first parent is the head the fork held, without
/// m, so x and m are concurrent whatever order they were written in, and
/// the root's m wins at the root's queue. The fork shows x, then m, never x
/// again; the mirror's push, a step of its own, then carries x to the
/// mirror, naming its writer.
#[test]
fn r3_a_save_before_the_sync_arrives_loses_to_the_upstream() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let m = w.save(&m1(), "a", 0);
    assert_eq!(w.queue_len(f), 1, "the root's sync waits");
    let x = w.save(&Client::Fork(f), "a", 0);
    let xc = w.head(f);
    ok(&mut w);
    assert!(has(&w, f, "a", &x), "the fork reads its own write");
    drain(&mut w);
    assert!(has(&w, TreeId::ROOT, "a", &m) && has(&w, f, "a", &m));
    let lost: Vec<(TreeId, Vec<String>, Vec<String>)> = w
        .collisions
        .iter()
        .map(|c| (c.tree, c.lost.clone(), c.won.clone()))
        .collect();
    assert_eq!(lost, [(TreeId::ROOT, vec![x.clone()], vec![m.clone()])],);
    assert!(
        !w.pending().contains(&Step::Push),
        "drained, the mirror's push included"
    );
    let pushed = git(
        w.mirror().repo(),
        &["rev-parse", &w.collisions[0].reference],
    );
    assert_eq!(pushed, xc, "the loser is on the mirror");
}

/// Rule 3, "a sync carries the parent's current version, never a stale
/// change": the root writes m1 then m2 over it, queueing two syncs. The first
/// sync brings m2 directly; the fork never shows m1.
#[test]
fn r3_a_sync_carries_the_upstreams_current_version() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    w.save(&Client::Fork(f), "b", 0);
    let m1_ = w.save(&m1(), "a", 0);
    let m2 = w.save(&m1(), "a", 0);
    assert_eq!(w.queue_len(f), 2);
    next(&mut w, f);
    assert!(has(&w, f, "a", &m2) && !has(&w, f, "a", &m1_));
    let head = w.head(f);
    next(&mut w, f);
    assert_eq!(w.head(f), head, "nothing new");
    drain(&mut w);
}

/// Coherence finding 1: the same calls as
/// [`r3_an_edit_over_the_decided_line_replaces_it`] with git's own
/// three-way merge for the sync, a conflicting hunk keeping the fork's
/// side. After x lands and loses, the fork and the root have crossed (each
/// merged the other), so git finds two merge bases, merges them leaving
/// conflict markers in the base, and sees x against m as a conflict: the
/// fork keeps x, which the root already decided against.
#[test]
#[ignore = "coherence finding 1: git's own merge base of crossing syncs and landings keeps a fork line the upstream already decided against"]
fn finding_1_git_merge_base_on_crossing_merges() {
    crossing(MergeRule::GitMergeBase);
}

fn crossing(merge: MergeRule) -> (Checked, String) {
    let mut w = Checked::new(
        Policies {
            merge,
            ..Policies::default()
        },
        TreeNames::new(MODEL_ROOT),
    );
    let f = w.create_fork(TreeId::ROOT);
    let x = w.save(&Client::Fork(f), "a", 0);
    let m = w.save(&m1(), "a", 0);
    next(&mut w, f); // sync: x is not decided, so F keeps it
    next(&mut w, TreeId::ROOT); // landing: the root's m wins; crossed
    assert!(has(&w, f, "a", &x) && has(&w, TreeId::ROOT, "a", &m));
    next(&mut w, f); // the root's decided version
    assert!(
        has(&w, f, "a", &m),
        "{merge:?}: the fork takes the root's decision"
    );
    let y = w.save(&Client::Fork(f), "a", 0); // written over m
    drain(&mut w);
    (w, y)
}

/// One open of `p` by `c`, a write at each of `slots`, one close: one save
/// of the whole file (rule 3e's unit). The versions the calls made.
fn save_lines(w: &mut World, c: &Client, p: &str, slots: &[usize]) -> Vec<String> {
    let h = w.open_write(c, p);
    let made = slots
        .iter()
        .map(|s| w.edit(h, LineEdit::At(*s)).expect("a new version"))
        .collect();
    w.close(h);
    made
}

/// A collision's (tree, cause, lost, won, loser).
type Decision = (TreeId, By, Vec<String>, Vec<String>, String);

/// The collisions, in order.
fn decided(w: &World) -> Vec<Decision> {
    w.collisions
        .iter()
        .map(|c| (c.tree, c.by, c.lost.clone(), c.won.clone(), c.loser.clone()))
        .collect()
}

/// Rule 3e (owner, 2026-11-03: "The whole file, right? That was the
/// “traditional Linux fs” model we had discussed"): one save writes lines 0
/// and 2 of a; the root concurrently writes line 0 and, cleanly, line 1. At
/// the root's queue the save's line 0 collides and the root wins, so the whole
/// save loses: its clean line 2 does not land either. Both are named, the
/// clean one as lost with its change (`By::Whole`), for the root's winning
/// line; the save is kept under its collision ref and pushed. The fork
/// then takes the root's file, line 2 included, once.
#[test]
fn r3_a_save_loses_whole_with_its_clean_lines() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let t = distinct(&mut w, "a");
    let v = save_lines(&mut w, &Client::Fork(f), "a", &[0, 2]);
    let (x, y) = (v[0].clone(), v[1].clone());
    let save = w.head(f);
    let m = w.save(&m1(), "a", 0);
    let k = w.save(&m1(), "a", 1);
    drain(&mut w);
    for tr in [TreeId::ROOT, f] {
        assert_eq!(
            ids(&w, tr, "a")[..3],
            [m.clone(), k.clone(), t[2].clone()],
            "{tr:?}"
        );
    }
    assert_eq!(
        decided(&w),
        [
            (
                TreeId::ROOT,
                By::Line,
                vec![x.clone()],
                vec![m.clone()],
                save.clone()
            ),
            (
                TreeId::ROOT,
                By::Whole,
                vec![y.clone()],
                vec![m.clone()],
                save.clone()
            ),
        ],
    );
    for c in &w.collisions {
        assert_eq!(
            git(w.mirror().repo(), &["rev-parse", &c.reference]),
            save,
            "pushed"
        );
    }
}

/// Rule 3e, "later saves are judged whole on their own": after the save
/// that lost (lines 0 and 2), the fork saves line 2 again, over its own
/// lost line (which was written over the root's), and then b. Neither later
/// save collides with anything of the root's, so both land whole, the first
/// replacing the root's line 2 by lineage.
#[test]
fn r3_a_later_save_after_a_losing_one_is_judged_on_its_own() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let c = Client::Fork(f);
    let t = distinct(&mut w, "a");
    let lost = save_lines(&mut w, &c, "a", &[0, 2]);
    let z = w.save(&c, "a", 2);
    let b = w.save(&c, "b", 0);
    let m = w.save(&m1(), "a", 0);
    drain(&mut w);
    assert_eq!(
        ids(&w, TreeId::ROOT, "a")[..3],
        [m.clone(), t[1].clone(), z.clone()],
    );
    assert!(has(&w, TreeId::ROOT, "b", &b));
    let names: Vec<String> = w.collisions.iter().flat_map(|c| c.lost.clone()).collect();
    assert_eq!(names, lost, "only the first save lost");
}

/// Rule 3e for a transaction (owner, 2026-11-03: "some commits represent a
/// transaction"): it stages lines of a and b; the root's line on a's staged
/// line syncs into the fork first. At its commit the fork's line wins
/// a, so the whole transaction loses: b's staged line does not apply
/// either, the answer names both (b's for the line that decided), the
/// collision ref keeps the transaction's branch, and nothing of it reaches
/// the fork or the root.
#[test]
fn r3_a_transaction_loses_whole_and_is_told() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let c = Client::Fork(f);
    w.begin(f).unwrap();
    let s = w.save(&c, "a", 0);
    let s2 = w.save(&c, "b", 0);
    let m = w.save(&m1(), "a", 0);
    drain(&mut w);
    assert_eq!(
        w.commit_txn(f),
        Ok(TxnOutcome::Lost {
            lines: vec![
                Overwritten {
                    path: "a".into(),
                    line: "a0".into(),
                    staged: s.clone(),
                    kept: m.clone(),
                },
                Overwritten {
                    path: "b".into(),
                    line: "b0".into(),
                    staged: s2.clone(),
                    kept: m.clone(),
                }
            ]
        })
    );
    ok(&mut w);
    // The transaction's one commit, the merge's second parent, lost.
    let staged = git(w.repo(), &["rev-parse", &format!("{}^2", w.head(f))]);
    assert_eq!(
        decided(&w),
        [
            (
                f,
                By::Line,
                vec![s.clone()],
                vec![m.clone()],
                staged.clone()
            ),
            (
                f,
                By::Whole,
                vec![s2.clone()],
                vec![m.clone()],
                staged.clone()
            ),
        ]
    );
    drain(&mut w);
    for t in [TreeId::ROOT, f] {
        assert!(has(&w, t, "a", &m) && !has(&w, t, "b", &s2), "{t:?}");
    }
    assert_eq!(w.collisions.len(), 2, "decided once, in the fork");
}

/// Rule 3e, a committed transaction at the root's queue (the case of coherence
/// finding 3, now the owner's rule): it committed cleanly in the fork,
/// with lines 0 and 2 of a; the root concurrently changed line 0 and, cleanly,
/// line 1. The root wins line 0, so the transaction's commit loses whole at
/// the root: line 2 stays the root's, and the fork takes the root's file, its line 2
/// going back to the root's, decided by that collision.
#[test]
fn r3_a_committed_transaction_loses_whole_at_the_root() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    w.begin(f).unwrap();
    let x = w.save(&Client::Fork(f), "a", 0);
    let y = w.save(&Client::Fork(f), "a", 2);
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
    let commit = w.head(f);
    let m = w.save(&m1(), "a", 0);
    let k = w.save(&m1(), "a", 1);
    drain(&mut w);
    for t in [TreeId::ROOT, f] {
        assert_eq!(
            ids(&w, t, "a")[..3],
            [m.clone(), k.clone(), "a2".into()],
            "{t:?}"
        );
    }
    assert_eq!(
        decided(&w),
        [
            (
                TreeId::ROOT,
                By::Line,
                vec![x],
                vec![m.clone()],
                commit.clone()
            ),
            (
                TreeId::ROOT,
                By::Whole,
                vec![y],
                vec![m.clone()],
                commit.clone()
            ),
        ],
    );
}

/// Rule 3e for a loss by a file rule (rule 11) inside a change of several
/// files: the transaction edits a and b, and the root unlinks a; whether the
/// sync brings the unlink into the fork before the commit (the commit
/// loses there, and is told) or the root's queue meets it after (the committed
/// transaction loses there), b's clean line never lands. The same with a
/// rename-over in the transaction (c over a, while the root edits a): its
/// unlink of a loses, so the whole transaction does, b included.
#[test]
fn r3_a_file_rule_loss_costs_the_whole_change() {
    for synced_first in [true, false] {
        let mut w = world();
        let f = w.create_fork(TreeId::ROOT);
        let c = Client::Fork(f);
        w.begin(f).unwrap();
        w.save(&c, "a", 0);
        let sb = w.save(&c, "b", 0);
        w.unlink(&m1(), "a");
        if synced_first {
            drain(&mut w);
        }
        let out = w.commit_txn(f).unwrap();
        ok(&mut w);
        assert_eq!(
            matches!(out, TxnOutcome::Lost { .. }),
            synced_first,
            "{out:?}"
        );
        drain(&mut w);
        for t in [TreeId::ROOT, f] {
            assert!(!has(&w, t, "b", &sb), "{t:?}: a partial change");
            assert_eq!(w.path_of(t, "a"), None, "{t:?}: the root's unlink");
        }
        let lost: BTreeSet<&str> = w.collisions.iter().map(|c| c.path.as_str()).collect();
        assert!(lost.contains("b"), "b's clean line is named lost");
        let tree = if synced_first { f } else { TreeId::ROOT };
        assert!(w.collisions.iter().all(|c| c.tree == tree));
    }
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let c = Client::Fork(f);
    w.begin(f).unwrap();
    w.rename(&c, "c", "a");
    let sb = w.save(&c, "b", 0);
    let m = w.save(&m1(), "a", 0);
    drain(&mut w);
    let out = w.commit_txn(f).unwrap();
    ok(&mut w);
    assert!(matches!(out, TxnOutcome::Lost { .. }), "{out:?}");
    drain(&mut w);
    for t in [TreeId::ROOT, f] {
        assert!(!has(&w, t, "b", &sb), "{t:?}: a partial change");
        assert!(has(&w, t, "a", &m), "{t:?}: the root's edit keeps a");
        assert_eq!(w.path_of(t, "c").as_deref(), Some("c"), "{t:?}");
    }
}

/// Rule 3e: only a lost line costs a change. One save deletes line 1 of a
/// and writes line 3; the root deletes line 1 too. The two deletes agree (no
/// collision), so nothing of the save lost and it lands whole: its line 3
/// applies.
#[test]
fn r3_a_change_whose_delete_agrees_lands_whole() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let t = distinct(&mut w, "a");
    let h = w.open_write(&Client::Fork(f), "a");
    w.edit(h, LineEdit::Delete(1));
    let y = w.edit(h, LineEdit::At(2)).expect("a new version");
    w.close(h);
    call(&mut w, &m1(), "a", LineEdit::Delete(1));
    drain(&mut w);
    assert!(w.collisions.is_empty(), "{:?}", w.collisions);
    assert_eq!(
        ids(&w, TreeId::ROOT, "a")[..3],
        [t[0].clone(), t[2].clone(), y],
    );
}

/// Rule 3d with whole saves, in the sync direction: two fork saves (line
/// 0, then line 2); the root changes line 0 and, cleanly, line 1. The sync
/// takes the root's line 1 and keeps the fork's lines 0 and 2, which the root's
/// queue has not decided yet. Then the root's line 0 wins against the first
/// save, and the second, its own change, lands.
#[test]
fn r3_a_sync_keeps_the_undecided_saves() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let x = w.save(&Client::Fork(f), "a", 0);
    let y = w.save(&Client::Fork(f), "a", 2);
    let m = w.save(&m1(), "a", 0);
    let k = w.save(&m1(), "a", 1);
    next(&mut w, f);
    assert!(has(&w, f, "a", &x) && has(&w, f, "a", &y));
    assert!(has(&w, f, "a", &k), "the root's clean line");
    drain(&mut w);
    assert_eq!(ids(&w, TreeId::ROOT, "a")[..3], [m, k, y]);
}

/// Rule 3, per line, on neighbouring lines (one git hunk): the fork's
/// line 1 and the root's line 0 both stay, in both trees.
#[test]
fn r3_neighbouring_lines_merge_per_line() {
    let (w, [k, m]) = neighbours(MergeRule::PerLine);
    for t in [TreeId::ROOT, TreeId::new(1)] {
        assert!(has(&w, t, "a", &k) && has(&w, t, "a", &m), "{t:?}");
    }
}

/// Coherence finding 6: the same calls with git's own merge for the sync,
/// a conflicting hunk keeping the fork's side (`-X ours`). Neighbouring
/// changes form one git hunk, so the sync keeps the fork's whole hunk
/// and withholds the root's line 0, which the root had decided. "Per hunk" in
/// git's sense withholds a decided line; the model's rule is per line.
#[test]
#[ignore = "coherence finding 6: git's hunk on neighbouring lines withholds the upstream's decided line"]
fn finding_6_git_hunks_on_neighbouring_lines() {
    neighbours(MergeRule::GitMergeBase);
}

fn neighbours(merge: MergeRule) -> (Checked, [String; 2]) {
    let mut w = Checked::new(
        Policies {
            merge,
            layout: Layout::Adjacent,
        },
        TreeNames::new(MODEL_ROOT),
    );
    let f = w.create_fork(TreeId::ROOT);
    let k = w.save(&Client::Fork(f), "a", 1);
    let m = w.save(&m1(), "a", 0);
    next(&mut w, f); // the root's sync: m beside k
    assert!(has(&w, f, "a", &k) && has(&w, f, "a", &m));
    drain(&mut w);
    (w, [k, m])
}

/// Rule 3, decided once, at the upstream's queue: nested G's g lands in its
/// upstream F, where F's line m (the root's, synced in) wins. F's next landing
/// into the root carries nothing of g, so the root records nothing; G takes m from
/// F. Sibling H holds m throughout.
#[test]
fn r3_a_collision_is_recorded_once_at_the_upstreams_queue() {
    let mut w = world();
    let (f, h) = (w.create_fork(TreeId::ROOT), w.create_fork(TreeId::ROOT));
    let g = w.create_fork(f);
    let gl = w.save(&Client::Fork(g), "a", 0);
    let m = w.save(&m1(), "a", 0);
    next(&mut w, f); // G's g lands in F: F has nothing there
    assert!(has(&w, f, "a", &gl));
    drain(&mut w);
    for t in [TreeId::ROOT, f, g, h] {
        assert!(has(&w, t, "a", &m), "{t:?}");
    }
    let records: Vec<TreeId> = w
        .collisions
        .iter()
        .filter(|c| c.lost.contains(&gl))
        .map(|c| c.tree)
        .collect();
    assert_eq!(records, [TreeId::ROOT]);
}

// ── rule 4: nothing waits; nested forks ─────────────────────────────────

/// Rule 4: a commit that collides does not hold back the next one; every
/// step takes its queue's head; the queues drain.
#[test]
fn r4_nothing_waits_behind_a_collision() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    w.save(&Client::Fork(f), "a", 0);
    let later = w.save(&Client::Fork(f), "b", 0);
    w.save(&m1(), "a", 0);
    assert_eq!(w.queue_len(TreeId::ROOT), 2);
    next(&mut w, TreeId::ROOT);
    assert_eq!(w.collisions.len(), 1);
    assert_eq!(w.queue_len(TreeId::ROOT), 1);
    next(&mut w, TreeId::ROOT);
    assert!(has(&w, TreeId::ROOT, "b", &later));
    drain(&mut w);
}

/// Rule 4, nested forks: G over F over the root, the same rules with F as
/// G's upstream. G's landing loses in F to F's own concurrent line; G's
/// later line lands in F, then loses in the root to the root's concurrent line;
/// a clean change of G's reaches the root named for G. Everything converges.
#[test]
fn r4_nested_forks_follow_the_same_rules() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let g = w.create_fork(f);
    let (cf, cg) = (Client::Fork(f), Client::Fork(g));
    let ga = w.save(&cg, "a", 0);
    let fa = w.save(&cf, "a", 0);
    next(&mut w, f); // G's a lands in F: F's line wins.
    assert!(has(&w, f, "a", &fa) && !has(&w, f, "a", &ga));
    assert!(
        w.collisions
            .iter()
            .any(|c| c.tree == f && c.direction == Direction::Landing && c.lost == [ga.clone()])
    );
    let gb = w.save(&cg, "b", 0);
    let mb = w.save(&m1(), "b", 0);
    let gc = w.save(&cg, "c", 0);
    drain(&mut w);
    assert!(has(&w, TreeId::ROOT, "b", &mb) && !has(&w, TreeId::ROOT, "b", &gb));
    assert!(has(&w, TreeId::ROOT, "c", &gc) && has(&w, TreeId::ROOT, "a", &fa));
    for t in [f, g] {
        assert!(has(&w, t, "b", &mb) && has(&w, t, "a", &fa), "{t:?}");
    }
    assert!(
        w.collisions
            .iter()
            .any(|c| c.tree == TreeId::ROOT && c.lost == [gb.clone()])
    );
    let c_commit = git(
        w.repo(),
        &[
            "log",
            "--format=%H",
            "-S",
            &gc,
            "--first-parent",
            &w.head(TreeId::ROOT),
        ],
    );
    let c_commit = c_commit.lines().last().expect("a root commit adds G's c");
    assert_eq!(w.writer_of_commit(c_commit), "c2");
    w.end_client(g);
    w.end_client(f);
    drain(&mut w);
    assert_eq!(w.forks[&f].life, Life::Dropped);
}

// ── rule 5: transactions ────────────────────────────────────────────────

/// Rule 5, a clean commit: the program reads its own staged edits on a
/// frozen snapshot while the fork keeps receiving syncs; the commit lands
/// in the root as ONE commit named for the client.
#[test]
fn r5_transaction_commits_clean() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let c = Client::Fork(f);
    w.begin(f).unwrap();
    assert_eq!(w.begin(f), Err(ModelError::InTransaction));
    let sa = w.save(&c, "a", 0);
    let sb = w.save(&c, "b", 0);
    let m = w.save(&m1(), "c", 0);
    drain(&mut w);
    assert!(w.client_reads(&c, "a").contains(&sa));
    assert!(!has(&w, f, "a", &sa), "the fork's tree lacks it");
    assert!(has(&w, f, "c", &m), "the fork got the sync");
    assert!(
        !w.client_reads(&c, "c").contains(&m),
        "the snapshot is frozen"
    );
    let before = root_writers(&w).len();
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
    assert!(w.client_reads(&c, "c").contains(&m));
    drain(&mut w);
    assert!(has(&w, TreeId::ROOT, "a", &sa) && has(&w, TreeId::ROOT, "b", &sb));
    assert_eq!(root_writers(&w).len(), before + 1, "one commit");
    assert_eq!(root_writers(&w)[0], "c1");
}

/// R5.5 (the owner, 2026-11-13: "a transaction should be recorded as one
/// commit"): saves in a transaction make no commit; `txn/<n>` stays at begin.
/// The commit is one commit T on begin, naming each open it staged a save
/// of; with the fork still at begin it is a fast-forward (R6.2), so the
/// fork's history gains exactly T.
#[test]
fn r5_a_transaction_is_recorded_as_one_commit() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let c = Client::Fork(f);
    let begin = w.head(f);
    w.begin(f).unwrap();
    let branch = TreeNames::txn_branch(1);
    let all = w.all_commits().len();
    let (ha, hb) = (w.open_write(&c, "a"), w.open_write(&c, "b"));
    let sa = w.edit(ha, LineEdit::At(0)).unwrap();
    let sb = w.edit(hb, LineEdit::At(0)).unwrap();
    w.close(ha);
    w.close(hb);
    ok(&mut w);
    assert_eq!(w.all_commits().len(), all, "a staged save makes no commit");
    assert_eq!(git(w.repo(), &["rev-parse", &branch]), begin);
    assert!(w.client_reads(&c, "a").contains(&sa));
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
    ok(&mut w);
    let t = w.head(f);
    assert_eq!(
        git(w.repo(), &["rev-list", &format!("{begin}..{t}")]),
        t,
        "one commit"
    );
    assert_eq!(show(&w, &t, "%P"), begin);
    assert_eq!(
        show(&w, &t, "%(trailers:key=Agentfs-Cause,valueonly)"),
        "Transaction t1"
    );
    assert_eq!(
        show(&w, &t, "%(trailers:key=Agentfs-Tree,valueonly)"),
        "fork/1"
    );
    assert_eq!(
        show(
            &w,
            &t,
            "%(trailers:key=Agentfs-Open,valueonly,separator=%x2c)"
        ),
        format!("{ha},{hb}")
    );
    assert!(has(&w, f, "a", &sa) && has(&w, f, "b", &sb));
    assert!(
        git(w.repo(), &["for-each-ref", &branch]).is_empty(),
        "its branch is gone"
    );
    drain(&mut w);
    assert!(has(&w, TreeId::ROOT, "a", &sa) && has(&w, TreeId::ROOT, "b", &sb));
}

/// R5.5: a line staged twice is one version in T, written over begin's: the
/// version staged between is in no commit. A line staged and written back
/// keeps begin's version.
#[test]
fn r5_a_line_staged_twice_is_one_version_over_begin() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let c = Client::Fork(f);
    w.begin(f).unwrap();
    w.save(&c, "a", 0);
    let last = w.save(&c, "a", 0);
    w.save(&c, "a", 1);
    let h = w.open_write(&c, "a");
    w.edit_text(h, LineEdit::At(1), BLANK);
    w.close(h);
    let staged = w.save(&c, "a", 2);
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
    ok(&mut w);
    let a = w.file(&w.head(f), "a");
    let root = Content::root("a");
    assert_eq!(a.lines[0].id, last);
    assert_eq!(
        a.lines[0].over.as_deref(),
        Some("a0"),
        "written over begin's"
    );
    assert_eq!(a.lines[1], root.lines[1], "written back: begin's version");
    assert_eq!(a.lines[2].id, staged);
    assert_eq!(a.lines[2].over.as_deref(), Some("a2"));
    drain(&mut w);
}

/// R5.5, net nothing: a transaction whose staged saves are all written back
/// changes nothing over begin, so it makes no commit and is told `Empty`.
#[test]
fn r5_a_transaction_that_changes_nothing_makes_no_commit() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let c = Client::Fork(f);
    let begin = w.head(f);
    w.begin(f).unwrap();
    w.save(&c, "a", 0);
    let h = w.open_write(&c, "a");
    w.edit_text(h, LineEdit::At(0), BLANK);
    w.close(h);
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Empty));
    ok(&mut w);
    assert_eq!(w.head(f), begin);
    drain(&mut w);
}

/// Rule 5, no race: the same calls as
/// [`r3_a_transaction_loses_whole_and_is_told`] and as
/// [`r5_a_committed_transaction_then_lands_like_any_change`], where the root's
/// sync arrives after the commit, end with the same trees. Whether the
/// sync comes before or after the commit decides nothing: the root's line
/// wins either way, and the transaction loses whole, at the fork's
/// commit or at the root's queue.
#[test]
fn r5_sync_before_or_after_the_commit_ends_the_same() {
    let run = |sync_first: bool| {
        let mut w = world();
        let f = w.create_fork(TreeId::ROOT);
        w.begin(f).unwrap();
        w.save(&Client::Fork(f), "a", 0);
        w.save(&Client::Fork(f), "b", 0);
        w.save(&m1(), "a", 0);
        if sync_first {
            drain(&mut w);
        }
        let outcome = w.commit_txn(f).expect("committed");
        assert_eq!(matches!(outcome, TxnOutcome::Lost { .. }), sync_first);
        drain(&mut w);
        ["a", "b"].map(|p| w.read(TreeId::ROOT, p))
    };
    assert_eq!(run(true), run(false));
}

/// Rule 5, rollback and client death: the branch is dropped, nothing of it
/// shows anywhere; the fork's own commits from before still land.
#[test]
fn r5_transaction_rollback_and_client_death_leave_nothing() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
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
    assert!(has(&w, TreeId::ROOT, "c", &kept));
    assert_eq!(w.forks[&f].life, Life::Dropped);
}

/// Rule 5 then rule 3: a committed transaction lands like any change. A
/// sync of the root's concurrent line, queued before the commit and processed
/// after, decides nothing: the fork keeps the whole transaction. The root's
/// queue then decides: the root's line wins line a, so the transaction loses
/// whole (rule 3e), its line b too, and the fork takes the root's lines,
/// once.
#[test]
fn r5_a_committed_transaction_then_lands_like_any_change() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    w.begin(f).unwrap();
    let sa = w.save(&Client::Fork(f), "a", 0);
    let sb = w.save(&Client::Fork(f), "b", 0);
    let m = w.save(&m1(), "a", 0); // queued into F, not yet synced
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
    next(&mut w, f);
    assert!(has(&w, f, "a", &sa) && has(&w, f, "b", &sb));
    drain(&mut w);
    for t in [TreeId::ROOT, f] {
        assert!(has(&w, t, "a", &m) && !has(&w, t, "b", &sb), "{t:?}");
    }
}

// ── rule 6: attribution ─────────────────────────────────────────────────

/// Rule 6: a sync is a merge commit labelled `Sync` with the upstream's
/// commit as second parent; it originates nothing (the invariant checks
/// every commit: one writer each; a non-merge commit carries only its
/// writer's edits; a merge adds only what a non-first parent holds).
#[test]
fn r6_a_sync_is_a_merge_with_the_upstream_second() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    w.save(&Client::Fork(f), "a", 0);
    w.save(&m1(), "b", 0);
    let root = w.head(TreeId::ROOT);
    next(&mut w, f);
    let head = w.head(f);
    let parents = show(&w, &head, "%P");
    assert_eq!(parents.split(' ').nth(1), Some(root.as_str()));
    assert_eq!(
        show(&w, &head, "%(trailers:key=Agentfs-Cause,valueonly)"),
        "Sync"
    );
    drain(&mut w);
}

// ── rule 7: full lineage ────────────────────────────────────────────────

/// Rule 7: the root's history reaches the fork's commits, the losing one
/// included, and the fork's losing version stays reachable under the
/// collision's ref after the fork is dropped.
#[test]
fn r7_the_root_reaches_a_losing_commit_and_its_ref_keeps_it() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let x = w.save(&Client::Fork(f), "a", 0);
    let fc = w.head(f);
    w.save(&m1(), "a", 0);
    w.end_client(f);
    drain(&mut w);
    assert_eq!(w.forks[&f].life, Life::Dropped);
    assert!(w.is_ancestor(&fc, &w.head(TreeId::ROOT)));
    let c = w
        .collisions
        .iter()
        .find(|c| c.lost == [x.clone()])
        .expect("x lost");
    let at = git(w.repo(), &["rev-parse", &c.reference]);
    assert!(w.file(&at, "a").contains(&x));
}

/// Rule 7 (the owner, 2026-11-05: "drop squashing"): each commit of a
/// fork, a transaction's included, lands on its own and is in the root's
/// history; every fork commit stays stored.
#[test]
fn r7_every_fork_commit_lands_on_its_own_in_the_roots_history() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let c = Client::Fork(f);
    let mut commits: Vec<String> = ["a", "b", "c"]
        .iter()
        .map(|p| {
            w.save(&c, p, 0);
            w.head(f)
        })
        .collect();
    w.begin(f).unwrap();
    w.save(&c, "d", 0);
    w.commit_txn(f).unwrap();
    commits.push(w.head(f));
    w.save(&c, "a", 1);
    commits.push(w.head(f));
    assert_eq!(w.queue_len(TreeId::ROOT), 5);
    let before = root_writers(&w).len();
    next(&mut w, TreeId::ROOT);
    assert_eq!(w.queue_len(TreeId::ROOT), 4, "one commit, one take");
    assert_eq!(root_writers(&w).len(), before + 1);
    let landed = ["a", "b", "c"].map(|p| w.read(TreeId::ROOT, p).at(0) != format!("{p}0"));
    assert_eq!(landed, [true, false, false]);
    drain(&mut w);
    assert_eq!(root_writers(&w).len(), before + 5);
    for s in &commits {
        assert!(w.is_ancestor(s, &w.head(TreeId::ROOT)));
    }
}

/// Rule 3e at every level: G over F over the root. G's save of lines 0 and 2
/// of a loses whole in F, to F's own concurrent line 0, its line 2 too.
/// G's next save of lines 0 and 2 of b wins in F and travels up in F's
/// landing commit, which loses whole at the root to the root's concurrent line 0
/// of b: b's line 2 reaches the root from neither.
#[test]
fn r4_nested_changes_land_whole_or_lose_whole_at_every_level() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let g = w.create_fork(f);
    let (cf, cg) = (Client::Fork(f), Client::Fork(g));
    let (ta, tb) = (distinct(&mut w, "a"), distinct(&mut w, "b"));
    let ga = save_lines(&mut w, &cg, "a", &[0, 2]);
    let fa = w.save(&cf, "a", 0);
    next(&mut w, f); // G's save loses whole in F
    assert_eq!(
        ids(&w, f, "a")[..3],
        [fa.clone(), ta[1].clone(), ta[2].clone()]
    );
    let gb = save_lines(&mut w, &cg, "b", &[0, 2]);
    let mb = w.save(&m1(), "b", 0);
    drain(&mut w);
    for t in [TreeId::ROOT, f, g] {
        assert_eq!(
            ids(&w, t, "a")[..3],
            [fa.clone(), ta[1].clone(), ta[2].clone()],
            "{t:?}"
        );
        assert_eq!(
            ids(&w, t, "b")[..3],
            [mb.clone(), tb[1].clone(), tb[2].clone()],
            "{t:?}"
        );
    }
    let by_tree: Vec<(TreeId, Vec<String>)> = w
        .collisions
        .iter()
        .map(|c| (c.tree, c.lost.clone()))
        .collect();
    assert_eq!(
        by_tree,
        [
            (f, vec![ga[0].clone()]),
            (f, vec![ga[1].clone()]),
            (TreeId::ROOT, vec![gb[0].clone()]),
            (TreeId::ROOT, vec![gb[1].clone()]),
        ],
    );
}

// ── convergence ─────────────────────────────────────────────────────────

/// When writes stop and the queues drain, every fork equals the root
/// (`check_converged`), here with siblings, a nested fork and collisions
/// in both directions.
#[test]
fn convergence_when_writes_stop() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let g = w.create_fork(TreeId::ROOT);
    let h = w.create_fork(f);
    for (i, c) in [Client::Fork(f), Client::Fork(g), Client::Fork(h), m1()]
        .iter()
        .enumerate()
    {
        w.save(c, "a", 0);
        w.save(c, "b", i % 2);
    }
    next(&mut w, g);
    next(&mut w, TreeId::ROOT);
    drain(&mut w);
    assert!(!w.collisions.is_empty());
}

// ── rule 8: a save records the write calls ──────────────────────────────

/// The fork's program read `a` before the root's m reached it by sync,
/// then opens `a`, makes one write call and closes. Returns the world, the
/// program's stale read, the synced m, and the fork.
fn read_then_sync() -> (Checked, Content, String, TreeId) {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let o = w.save(&m1(), "a", 0);
    drain(&mut w);
    let read = w.client_reads(&Client::Fork(f), "a");
    assert!(read.contains(&o));
    let m = w.save(&m1(), "a", 0); // over o
    next(&mut w, f);
    assert!(has(&w, f, "a", &m));
    (w, read, m, f)
}

/// Rule 8, an append after the sync: it adds a line at the file's end as
/// it is now, so the line the sync brought stays, in the fork and, once
/// landed, in the root.
#[test]
fn r8_an_append_after_the_sync_keeps_the_synced_line() {
    let (mut w, _, m, f) = read_then_sync();
    let h = w.open_write(&Client::Fork(f), "a");
    let x = w.edit(h, LineEdit::Append).unwrap();
    w.close(h);
    ok(&mut w);
    assert_eq!(w.read(f, "a").last(), x, "after the end");
    drain(&mut w);
    assert!(has(&w, TreeId::ROOT, "a", &m) && has(&w, TreeId::ROOT, "a", &x));
    assert!(w.collisions.is_empty());
}

/// Rule 8, a write at an offset elsewhere: it replaces exactly that line;
/// the line the sync brought, which the program never read, stays.
#[test]
fn r8_an_offset_write_elsewhere_keeps_the_synced_line() {
    let (mut w, _, m, f) = read_then_sync();
    let h = w.open_write(&Client::Fork(f), "a");
    let y = w.edit(h, LineEdit::At(2)).unwrap();
    w.close(h);
    drain(&mut w);
    for t in [TreeId::ROOT, f] {
        let a = w.read(t, "a");
        assert_eq!((a.at(0), a.at(2)), (m.as_str(), y.as_str()), "{t:?}");
    }
    assert!(w.collisions.is_empty());
}

/// Rule 8, a stale truncate and rewrite: the program writes back the whole
/// file it read before the sync. That really puts o's bytes back over m,
/// recorded as the program's write: a new line by the fork's writer
/// (`git blame` names it), written over m, so it replaces m in the root too
/// with no collision. m stays in the root's history.
#[test]
fn r8_a_stale_rewrite_overwrites_the_synced_line_and_history_keeps_it() {
    let (mut w, read, m, f) = read_then_sync();
    let m_commit = w.head(TreeId::ROOT);
    let h = w.open_write(&Client::Fork(f), "a");
    assert_eq!(w.edit(h, LineEdit::Rewrite(read.clone())), None);
    w.close(h);
    let a = w.read(f, "a");
    let r = a.at(0).to_string();
    assert_eq!(
        a.get(0).and_then(|l| l.text()),
        read.get(0).and_then(|l| l.text()),
        "o's bytes, back"
    );
    assert!(r.starts_with("c1.") && w.descends(&r, &m));
    drain(&mut w);
    assert!(has(&w, TreeId::ROOT, "a", &r) && !has(&w, TreeId::ROOT, "a", &m));
    assert!(w.collisions.is_empty(), "no collision");
    // The first line's record: git line `line_of(1)`, after the name.
    let n = Content::line_of(1, Layout::Separated);
    let blame = git(
        w.repo(),
        &[
            "blame",
            "--porcelain",
            &format!("-L{n},{n}"),
            &w.head(TreeId::ROOT),
            "--",
            "a",
        ],
    );
    let by = blame.split(' ').next().expect("a commit").to_string();
    assert_eq!(w.writer_of_commit(&by), "c1", "the program's write");
    assert!(
        w.is_ancestor(&m_commit, &w.head(TreeId::ROOT)),
        "m stays in history"
    );
    assert!(w.file(&m_commit, "a").contains(&m));
}

/// Rule 8, `O_APPEND`: two of the root's clients append to one file at once.
/// Each append goes after the file's end as it is at its close, so neither
/// writes over the other's line, whichever closes first; and a check
/// rejects an append that lands on a written slot (`one_writer`).
#[test]
fn r8_concurrent_appends_never_overwrite_each_other() {
    for m2_first in [true, false] {
        let mut w = world();
        let o = w.save(&m1(), "a", 0);
        let m2 = Client::Root("m2".into());
        let (a, b) = (w.open_write(&m1(), "a"), w.open_write(&m2, "a"));
        let xa = w.edit(a, LineEdit::Append).unwrap();
        let xb = w.edit(b, LineEdit::Append).unwrap();
        let (first, second) = if m2_first { (b, a) } else { (a, b) };
        w.close(first);
        ok(&mut w);
        w.close(second);
        ok(&mut w);
        let f = w.read(TreeId::ROOT, "a");
        let (one, two) = if m2_first { (&xb, &xa) } else { (&xa, &xb) };
        assert_eq!(
            (f.at(0), f.at(6), f.at(7)),
            (o.as_str(), one.as_str(), two.as_str()),
            "in close order after the last line, none over another"
        );
        drain(&mut w);
    }
}

/// The bytes of `p` in `t`.
fn bytes(w: &World, t: TreeId, p: &str) -> String {
    w.read(t, p).bytes()
}

/// Opens `p` as `c`, makes these byte calls, closes.
fn calls(w: &mut World, c: &Client, p: &str, calls: &[Write]) {
    let h = w.open_write(c, p);
    for x in calls {
        w.write(h, x.clone());
    }
    w.close(h);
}

/// `a` holding `foo` with no final newline, saved by the root: one line, the
/// file's last six lines deleted. Returns that line's identity.
fn foo_without_newline(w: &mut Checked) -> String {
    calls(
        w,
        &m1(),
        "a",
        &[Write::Truncate(0), Write::Append("foo".into())],
    );
    drain(w);
    assert_eq!(bytes(w, TreeId::ROOT, "a"), "foo");
    w.read(TreeId::ROOT, "a")
        .get(0)
        .expect("a line")
        .line
        .clone()
}

/// Rule 8, an `O_APPEND` write onto a partial last line: "foo" with no
/// final newline and an append of "bar" is one line, "foobar", a new
/// version of foo's line written over it, not a new line. Bytes after the
/// first newline the append writes are new lines.
#[test]
fn r8_an_append_onto_a_partial_last_line_finishes_it() {
    let mut w = world();
    let foo = foo_without_newline(&mut w);
    let before = w.read(TreeId::ROOT, "a").get(0).expect("foo").id.clone();
    calls(&mut w, &m1(), "a", &[Write::Append("bar".into())]);
    let a = w.read(TreeId::ROOT, "a");
    assert_eq!(a.bytes(), "foobar");
    let l = a.get(0).expect("a line");
    assert_eq!(
        (&l.line, &l.over),
        (&foo, &Some(before)),
        "the same line, one version on"
    );
    calls(&mut w, &m1(), "a", &[Write::Append("\nbaz\nq".into())]);
    let a = w.read(TreeId::ROOT, "a");
    assert_eq!(a.texts(), ["foobar", "baz", "q"]);
    assert_eq!(a.get(0).expect("a line").line, foo);
    assert!(
        a.get(1).is_some_and(|l| l.line == l.id),
        "baz is a new line"
    );
    drain(&mut w);
}

/// Rule 8 with rule 3: two trees append to a file whose last line has no
/// newline. Each append finishes that line, so each is a version of it:
/// they collide, the upstream's wins and the fork's save loses whole
/// (a cost: appends to a file without a final newline are not concurrent-
/// safe, as they are on a terminated file, `r8_concurrent_appends_*`).
#[test]
fn r8_concurrent_appends_onto_a_partial_last_line_collide() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let foo = foo_without_newline(&mut w);
    calls(
        &mut w,
        &Client::Fork(f),
        "a",
        &[Write::Append("x\n".into())],
    );
    calls(&mut w, &m1(), "a", &[Write::Append("m\n".into())]);
    drain(&mut w);
    assert_eq!(bytes(&w, TreeId::ROOT, "a"), "foom\n");
    assert_eq!(bytes(&w, f, "a"), "foom\n");
    let c = w.collisions.last().expect("a collision").clone();
    assert_eq!((c.by, c.lines), (By::Line, vec![foo]));
}

/// R10.4 (the owner, 2026-11-05: "collide"): the root appends "}" with no final
/// newline while the fork appends a line. The fork's line, which would
/// come after the root's unfinished last line, collides with it and the root wins, at
/// every level of nesting. agentfs adds no newline: the file ends in "}", and
/// the fork's save stays in history, kept by its collision ref.
#[test]
fn r10_a_line_after_the_upstreams_unfinished_line_collides() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let g = w.create_fork(f);
    calls(
        &mut w,
        &Client::Fork(f),
        "b",
        &[Write::Append("n9\n".into())],
    );
    let n9 = w.read(f, "b").last().to_string();
    calls(&mut w, &m1(), "b", &[Write::Append("}".into())]);
    let brace = w.read(TreeId::ROOT, "b").last().to_string();
    next(&mut w, TreeId::ROOT);
    assert_eq!(&bytes(&w, TreeId::ROOT, "b")[12..], "}");
    let c = w.collisions.last().expect("a collision").clone();
    assert_eq!(
        (c.tree, c.by, c.lost, c.won),
        (TreeId::ROOT, By::Line, vec![n9.clone()], vec![brace])
    );
    assert!(w.file(&c.loser, "b").contains(&n9), "kept in history");
    drain(&mut w);
    // The same with a fork as the upstream: its own unfinished last line wins.
    calls(&mut w, &Client::Fork(f), "c", &[Write::Append("{".into())]);
    calls(
        &mut w,
        &Client::Fork(g),
        "c",
        &[Write::Append("n8\n".into())],
    );
    next(&mut w, f);
    assert_eq!(&bytes(&w, f, "c")[12..], "{");
    let c = w.collisions.last().expect("a collision").clone();
    assert_eq!(c.tree, f);
    drain(&mut w);
    for t in [TreeId::ROOT, f, g] {
        assert_eq!(&bytes(&w, t, "b")[12..], "}", "{t:?}");
        assert_eq!(&bytes(&w, t, "c")[12..], "{", "{t:?}");
    }
}

/// R8.1, not R10.4: an append made after the sync brought the root's unfinished
/// last line finishes that line, written over it, and lands with no collision.
#[test]
fn r10_an_append_over_the_synced_unfinished_line_finishes_it() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    calls(&mut w, &m1(), "b", &[Write::Append("}".into())]);
    next(&mut w, f);
    calls(
        &mut w,
        &Client::Fork(f),
        "b",
        &[Write::Append(";\n".into())],
    );
    ends_converged_without_collisions(&mut w);
    assert_eq!(&bytes(&w, TreeId::ROOT, "b")[12..], "};\n");
}

/// R10.6 (finding 29): a sync hides the upstream's unfinished last line in front
/// of the fork's undecided line, shows the upstream's version once decided, and
/// no tree shows bytes nobody wrote.
#[test]
fn r10_a_sync_hides_the_upstreams_unfinished_line_before_an_undecided_one() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    calls(
        &mut w,
        &Client::Fork(f),
        "b",
        &[Write::Append("n9\n".into())],
    );
    calls(&mut w, &m1(), "b", &[Write::Append("}".into())]);
    next(&mut w, f);
    let b = w.read(f, "b");
    let lines = b.live().count();
    assert_eq!(
        b.bytes().lines().count(),
        lines,
        "{:?}: only a file's last line can lack a newline",
        b.bytes()
    );
    assert!(b.bytes().ends_with("n9\n"), "{:?}", b.bytes());
    assert!(!b.bytes().contains('}'), "{:?}", b.bytes());
    drain(&mut w);
    assert!(bytes(&w, TreeId::ROOT, "b").ends_with('}'));
    assert_eq!(bytes(&w, f, "b"), bytes(&w, TreeId::ROOT, "b"));
}

/// R10.9 (finding 33): a sync hides the upstream's later lines behind the
/// fork's undecided unfinished last line, so `y` then `m7\n` never reads
/// `ym7\n`. Nothing is lost: the save loses (R10.8), it stays in history, and
/// the fork then shows the upstream's version.
#[test]
fn r10_a_sync_hides_the_upstreams_lines_after_an_undecided_unfinished_line() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let c = Client::Fork(f);
    let cut = w.open_trunc(&c, "c");
    w.write(
        cut,
        Write::Pwrite {
            offset: 0,
            bytes: "y".into(),
        },
    );
    w.close(cut);
    let loser = w.head(f);
    calls(&mut w, &m1(), "c", &[Write::Append("m7\n".into())]);
    let root = bytes(&w, TreeId::ROOT, "c");
    next(&mut w, f);
    assert_eq!(bytes(&w, f, "c"), "y");
    drain(&mut w);
    assert_eq!(bytes(&w, TreeId::ROOT, "c"), root);
    assert_eq!(bytes(&w, f, "c"), root);
    let lost = w
        .collisions
        .iter()
        .find(|x| x.tree == TreeId::ROOT && x.loser == loser)
        .expect("the save lost, with its record");
    assert!(w.is_ancestor(&lost.loser, &w.head(TreeId::ROOT)));
}

/// Finding 35, answered by the owner, 2026-11-06, "A is fine": the root cuts a line the
/// fork already holds to an unfinished last line, in front of the fork's
/// undecided line. R10.6 covers it: the line leaves the fork's file until the
/// upstream decides, then the upstream's version arrives.
#[test]
fn finding_35_a_sync_hides_a_line_the_fork_already_held() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    calls(
        &mut w,
        &m1(),
        "c",
        &[Write::Pwrite {
            offset: 2,
            bytes: "n3\n".into(),
        }],
    );
    next(&mut w, f);
    assert!(bytes(&w, f, "c").contains("n3\n"));
    calls(
        &mut w,
        &Client::Fork(f),
        "c",
        &[Write::Append("n5\n".into())],
    );
    calls(&mut w, &m1(), "c", &[Write::Truncate(3)]);
    assert_eq!(bytes(&w, TreeId::ROOT, "c"), "-\nn");
    next(&mut w, f);
    // Undecided: the fork's line alone, neither version of the held line.
    assert_eq!(bytes(&w, f, "c"), "-\nn5\n");
    drain(&mut w);
    // Decided: the fork's line lost to the unfinished line (R10.4), and the
    // upstream's version arrived.
    assert_eq!(bytes(&w, TreeId::ROOT, "c"), "-\nn");
    assert_eq!(bytes(&w, f, "c"), "-\nn");
    assert!(
        w.collisions
            .iter()
            .any(|x| x.tree == TreeId::ROOT && x.path == "c")
    );
}

/// Finding 39: a fork's atomic save that loses leaves two files, each with its own
/// bytes, and a program reads them. Recorded so a real mount replays it too.
#[test]
fn r11_a_losing_atomic_save_keeps_two_files_apart() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let s = Client::Fork(f);
    let h = World::create(&mut w, &s, "t").unwrap();
    w.write(h, Write::Append("n\n-\n-\n-\n-\n-\n".into()));
    w.close(h);
    w.rename(&s, "t", "c");
    calls(
        &mut w,
        &m1(),
        "c",
        &[Write::Pwrite {
            offset: 0,
            bytes: "m\n".into(),
        }],
    );
    drain(&mut w);
    assert_eq!(bytes(&w, f, "c"), "m\n-\n-\n-\n-\n-\n");
    assert_eq!(bytes(&w, f, "t"), "n\n-\n-\n-\n-\n-\n");
    assert_eq!(bytes(&w, TreeId::ROOT, "t"), "n\n-\n-\n-\n-\n-\n");
}

/// R11.3 and the same mechanism: an unlink, or a rename over the file, that loses brings
/// the file back; a writer that held the old one still reaches no file.
#[test]
fn r11_an_old_writer_stays_orphaned_when_the_change_that_ended_its_file_loses() {
    for rename in [false, true] {
        let mut w = world();
        let f = w.create_fork(TreeId::ROOT);
        let s = Client::Fork(f);
        let old = w.open_write(&s, "c");
        w.write(
            old,
            Write::Pwrite {
                offset: 0,
                bytes: "x\n".into(),
            },
        );
        if rename {
            w.rename(&s, "b", "c");
        } else {
            w.unlink(&s, "c");
        }
        calls(
            &mut w,
            &m1(),
            "c",
            &[Write::Pwrite {
                offset: 0,
                bytes: "m\n".into(),
            }],
        );
        next(&mut w, TreeId::ROOT);
        next(&mut w, f);
        next(&mut w, f);
        assert_eq!(bytes(&w, f, "c"), "m\n-\n-\n-\n-\n-\n");
        w.close(old);
        drain(&mut w);
        assert_eq!(w.orphans().len(), 1, "the old writer saved nothing");
        assert_eq!(bytes(&w, f, "c"), "m\n-\n-\n-\n-\n-\n");
    }
}

/// Finding 43 (seed 191 of 1 to 500 of `full_lineage`, `adjacent_lines` and
/// `commit_dates_decide_nothing`): R10.9 hides lines the fork itself deleted in a
/// save that lost, and they arrive with the decision. `syncs_deliver` excuses a line the
/// fork holds only as its own delete (the owner, 2026-11-07, his 2, option a), and
/// no other held line.
#[test]
fn finding_43_a_sync_hides_a_line_the_fork_deleted_in_a_lost_save() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let s = Client::Fork(f);
    calls(
        &mut w,
        &m1(),
        "c",
        &[
            Write::Truncate(0),
            Write::Pwrite {
                offset: 0,
                bytes: "1\n2\n3\n4\n5\n6\n".into(),
            },
        ],
    );
    next(&mut w, f);
    calls(
        &mut w,
        &m1(),
        "c",
        &[Write::Pwrite {
            offset: 0,
            bytes: "M".into(),
        }],
    );
    // The fork edits line 1, which collides, and deletes lines 5 and 6.
    calls(
        &mut w,
        &s,
        "c",
        &[
            Write::Truncate(8),
            Write::Pwrite {
                offset: 0,
                bytes: "S".into(),
            },
        ],
    );
    next(&mut w, TreeId::ROOT);
    assert!(!w.collisions.is_empty(), "that save lost");
    // Before that decision reaches it, its next save leaves line 4 unfinished.
    calls(
        &mut w,
        &s,
        "c",
        &[
            Write::Truncate(6),
            Write::Pwrite {
                offset: 6,
                bytes: "yy".into(),
            },
        ],
    );
    next(&mut w, f);
    // Undecided: its unfinished line alone, the upstream's lines 5 and 6 hidden (R10.9).
    assert_eq!(bytes(&w, f, "c"), "M\n2\n3\nyy");
    drain(&mut w);
    // Decided: that save lost to the line the upstream keeps after it (R10.8).
    assert_eq!(bytes(&w, TreeId::ROOT, "c"), "M\n2\n3\n4\n5\n6\n");
    assert_eq!(bytes(&w, f, "c"), "M\n2\n3\n4\n5\n6\n");
}

/// R9.1, finding 46 (the owner, 2026-11-07, "5 a sounds correct, right?"): a landing also
/// locks the names its change touches. A fork's unlink of `a` waits while the root's open of
/// another file holds that name, though its own change does not write that file; the close
/// ends the wait (an event), all its keys are then free at once, and the take goes.
#[test]
fn r9_a_landing_waits_for_an_open_file_that_took_a_name_it_touches() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let held = w.open_write(&m1(), "b");
    ok(&mut w);
    w.rename(&m1(), "b", "a");
    w.unlink(&Client::Fork(f), "a");
    ok(&mut w);
    take(&mut w, &Step::Next(TreeId::ROOT));
    assert!(
        w.waits()[&TreeId::ROOT]
            .values()
            .any(|x| x.until.contains("name:a")),
        "the landing's unlink touches the name the open file took"
    );
    assert!(
        w.waiting_for(TreeId::ROOT).is_empty(),
        "and no file of its own"
    );
    assert!(
        w.last_take().is_none(),
        "it locks nothing while it waits (R9.4)"
    );
    w.close(held);
    ok(&mut w);
    assert!(w.waits().is_empty(), "the close woke it");
    next(&mut w, TreeId::ROOT);
    drain(&mut w);
}

/// R10.5 (finding 30, the owner, 2026-11-05: "A"): a line a transaction
/// appended after its fork's unfinished last line collides at the commit, as
/// R10.4 does at a landing. The fork's line wins, the transaction loses whole
/// and is told, naming its line; no tree shows the two lines as one.
#[test]
fn r10_a_transactions_line_after_the_forks_unfinished_line_loses_whole() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let c = Client::Fork(f);
    w.begin(f).unwrap();
    calls(&mut w, &c, "b", &[Write::Append("n9\n".into())]);
    let staged = w.file(&w.forks[&f].txn.as_ref().expect("open").index, "b");
    let n9 = staged.last().to_string();
    let n9_line = staged.live().last().expect("n9").line.clone();
    // The fork's own unfinished line arrives after begin, by a sync.
    calls(&mut w, &m1(), "b", &[Write::Append("}".into())]);
    next(&mut w, f);
    let brace = w.read(f, "b").last().to_string();
    assert_eq!(&bytes(&w, f, "b")[12..], "}");
    assert_eq!(
        w.commit_txn(f),
        Ok(TxnOutcome::Lost {
            lines: vec![Overwritten {
                path: "b".into(),
                line: n9_line,
                staged: n9.clone(),
                kept: brace,
            }],
        })
    );
    ok(&mut w);
    assert_eq!(&bytes(&w, f, "b")[12..], "}", "the fork keeps its `}}`");
    assert!(!has(&w, f, "b", &n9), "none of the transaction applies");
    drain(&mut w);
    for t in [TreeId::ROOT, f] {
        assert_eq!(&bytes(&w, t, "b")[12..], "}", "{t:?}");
    }
}

/// R10.11 (the owner, 2026-11-12: "A"): a transaction's commit follows every
/// landing rule, R10.8 included. The transaction rewrites its file's last line
/// with no newline; a sync then brings the fork a line after it. At the commit
/// the unfinished line would stand in front of a line the fork keeps: it
/// collides, the fork's line wins and the transaction loses whole and is told.
#[test]
fn r10_a_transactions_unfinished_line_before_a_kept_line_loses_whole() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let c = Client::Fork(f);
    w.begin(f).unwrap();
    calls(
        &mut w,
        &c,
        "c",
        &[
            Write::Pwrite {
                offset: 10,
                bytes: "y".into(),
            },
            Write::Truncate(11),
        ],
    );
    let staged = w.file(&w.forks[&f].txn.as_ref().expect("open").index, "c");
    assert_eq!(staged.bytes(), "-\n-\n-\n-\n-\ny");
    let y = staged.last().to_string();
    let y_line = staged.live().last().expect("y").line.clone();
    calls(&mut w, &m1(), "c", &[Write::Append("z\n".into())]);
    next(&mut w, f);
    let z = w.read(f, "c").last().to_string();
    let fork = bytes(&w, f, "c");
    assert_eq!(fork, "-\n-\n-\n-\n-\n-\nz\n");
    assert_eq!(
        w.commit_txn(f),
        Ok(TxnOutcome::Lost {
            lines: vec![Overwritten {
                path: "c".into(),
                line: y_line,
                staged: y.clone(),
                kept: z,
            }],
        })
    );
    ok(&mut w);
    assert_eq!(bytes(&w, f, "c"), fork, "the fork keeps its file");
    drain(&mut w);
    for t in [TreeId::ROOT, f] {
        assert_eq!(bytes(&w, t, "c"), fork, "{t:?}");
    }
}

/// Rule 8 and 10, a line written in two calls, and two calls over one
/// another: line identity comes from the bytes at close, so a line written
/// in pieces is one version, and of two overlapping writes the bytes of
/// the later one count.
#[test]
fn r8_split_and_overlapping_writes_make_one_version() {
    let mut w = world();
    let pw = |offset: usize, bytes: &str| Write::Pwrite {
        offset,
        bytes: bytes.into(),
    };
    // Line 1 rewritten longer, in two calls: the second rewrites the
    // bytes after it, as an in-place edit must.
    calls(&mut w, &m1(), "c", &[pw(2, "ab"), pw(4, "c\n-\n-\n-\n-\n")]);
    let c = w.read(TreeId::ROOT, "c");
    assert_eq!(c.texts(), ["-", "abc", "-", "-", "-", "-"]);
    assert_eq!(
        c.get(1).expect("a line").line,
        "c1",
        "one version of line 1"
    );
    // A new line at the end, in two calls: one new line.
    calls(&mut w, &m1(), "c", &[pw(14, "de"), pw(16, "f\n")]);
    let c = w.read(TreeId::ROOT, "c");
    assert_eq!(c.live().count(), 7);
    assert!(
        c.get(6)
            .is_some_and(|l| l.text() == Some("def\n") && l.line == l.id)
    );
    calls(&mut w, &m1(), "c", &[pw(0, "xyz\n"), pw(1, "Q")]);
    let c = w.read(TreeId::ROOT, "c");
    // "xyz\n" over "-\nab": line 1 keeps only "c".
    assert_eq!(c.texts()[..2], ["xQz", "c"], "the later write's byte");
    drain(&mut w);
}

/// Rule 8, `ftruncate` and holes: a truncate in the middle of a line
/// leaves it partial, a new version of that line; a `pwrite` past the end
/// fills the gap with NUL bytes, which the line map keeps (escaped). An
/// `O_TRUNC` open that writes nothing saves an empty file: every line
/// deleted, none lost from history.
#[test]
fn r8_truncates_holes_and_an_empty_o_trunc() {
    let mut w = world();
    calls(&mut w, &m1(), "d", &[Write::Truncate(3)]);
    let d = w.read(TreeId::ROOT, "d");
    assert_eq!(d.bytes(), "-\n-");
    assert_eq!(d.get(1).expect("a line").line, "d1");
    let pw = Write::Pwrite {
        offset: 5,
        bytes: "z\n".into(),
    };
    calls(&mut w, &m1(), "d", &[pw]);
    assert_eq!(bytes(&w, TreeId::ROOT, "d"), "-\n-\0\0z\n");
    let h = w.open_trunc(&m1(), "d");
    w.close(h);
    assert_eq!(bytes(&w, TreeId::ROOT, "d"), "");
    assert!(w.read(TreeId::ROOT, "d").lines.iter().all(|l| !l.live()));
    drain(&mut w);
}

// ── rule 9: a take locks the files it overwrites ────────────────────────

/// Rule 9: the root's sync waits while the fork's program has `a` open for
/// writing: its queue head records the wait and its step is not offered.
/// The close (an event) wakes it, and it applies over the saved file. A
/// sync of a file nobody has open does not wait.
#[test]
fn r9_a_sync_waits_for_an_open_writer_then_applies() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let h = w.open_write(&Client::Fork(f), "a");
    let m = w.save(&m1(), "a", 0);
    take(&mut w, &Step::Next(f));
    assert_eq!(w.waiting_for(f), BTreeSet::from(["a".to_string()]));
    assert!(!w.pending().contains(&Step::Next(f)));
    assert!(!has(&w, f, "a", &m), "not written");
    let x = w.edit(h, LineEdit::At(1)).unwrap();
    w.close(h);
    ok(&mut w);
    assert!(w.waits().is_empty(), "the close woke it");
    next(&mut w, f);
    let a = w.read(f, "a");
    assert_eq!((a.at(0), a.at(1)), (m.as_str(), x.as_str()));
    let h = w.open_write(&Client::Fork(f), "b");
    w.save(&m1(), "c", 0);
    next(&mut w, f); // c only: does not wait for b
    w.close(h);
    drain(&mut w);
}

/// R9.18 (the owner, 2026-11-08: "Yes. “Waiting longer” is a feature of locks and
/// transactions, not a bug"): a file open inside a transaction holds back the
/// fork's sync of it until it closes, as an open on the fork does (R9.3).
#[test]
fn r9_a_file_open_in_a_transaction_holds_back_the_forks_sync() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    w.begin(f).unwrap();
    let h = w.open_write(&Client::Fork(f), "a");
    assert!(
        matches!(w.handle(h).target, Target::Txn(..)),
        "in the transaction"
    );
    let m = w.save(&m1(), "a", 0);
    take(&mut w, &Step::Next(f));
    assert_eq!(w.waiting_for(f), BTreeSet::from(["a".to_string()]));
    assert!(!w.pending().contains(&Step::Next(f)));
    assert!(!has(&w, f, "a", &m), "not written");
    let x = w.edit(h, LineEdit::At(1)).unwrap();
    w.close(h);
    ok(&mut w);
    assert!(w.waits().is_empty(), "the close woke it");
    next(&mut w, f);
    assert!(has(&w, f, "a", &m), "the sync took the root's line");
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
    let a = w.read(f, "a");
    assert_eq!((a.at(0), a.at(1)), (m.as_str(), x.as_str()));
    drain(&mut w);
}

/// R9.18 for every take (the owner, 2026-11-12, picking "A file open inside a
/// transaction holds back every take into its fork until it closes"): a
/// landing from the fork's own fork waits for the file open inside the
/// fork's transaction, as R9.1's landing waits for an open on the fork.
#[test]
fn r9_a_file_open_in_a_transaction_holds_back_a_landing_into_its_fork() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let g = w.create_fork(f);
    w.begin(f).unwrap();
    let h = w.open_write(&Client::Fork(f), "a");
    let y = w.save(&Client::Fork(g), "a", 2);
    take(&mut w, &Step::Next(f));
    assert_eq!(w.waiting_for(f), BTreeSet::from(["a".to_string()]));
    assert!(!has(&w, f, "a", &y), "not written");
    w.close(h);
    ok(&mut w);
    assert!(w.waits().is_empty(), "the close woke it");
    next(&mut w, f);
    assert!(has(&w, f, "a", &y), "the landing took g's line");
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Empty));
    drain(&mut w);
}

/// Rule 9, the same rule for a landing into a tree (a choice): the root's
/// queue waits while one of the root's clients has the file open for writing.
#[test]
fn r9_a_landing_waits_for_the_upstreams_open_writer() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let h = w.open_write(&m1(), "a");
    let x = w.save(&Client::Fork(f), "a", 2);
    take(&mut w, &Step::Next(TreeId::ROOT));
    assert!(w.waits().contains_key(&TreeId::ROOT) && !has(&w, TreeId::ROOT, "a", &x));
    let y = w.edit(h, LineEdit::At(0)).unwrap();
    w.close(h);
    ok(&mut w);
    next(&mut w, TreeId::ROOT);
    assert!(has(&w, TreeId::ROOT, "a", &x) && has(&w, TreeId::ROOT, "a", &y));
    drain(&mut w);
}

/// Rule 9 (R9.1): a landing locks every file its changes touch, a losing
/// change's included, not only what its plan writes. Here the fork's unlink
/// of `a` loses (the root's unseen line), so the plan writes nothing; the landing
/// still waits while the root's program has `a` open for writing, and its take,
/// one step, goes once that open closes.
#[test]
fn r9_a_landing_holds_the_files_of_its_losing_changes() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    w.unlink(&Client::Fork(f), "a");
    let m = w.save(&m1(), "a", 0);
    let h = w.open_write(&m1(), "a");
    take(&mut w, &Step::Next(TreeId::ROOT));
    assert!(
        w.waiting_for(TreeId::ROOT).contains("a") && w.last_take().is_none(),
        "the losing unlink's file is locked: the landing waits for its writer"
    );
    w.close(h);
    ok(&mut w);
    next(&mut w, TreeId::ROOT);
    let (_, taken) = w.last_take().expect("the landing is taken");
    assert!(taken.files.contains("a"), "{:?}", taken.files);
    assert!(has(&w, TreeId::ROOT, "a", &m), "the unlink lost");
    let c = w.collisions.last().expect("a collision").clone();
    assert!(c.by == By::File && c.won == [m.clone()], "{c:?}");
    drain(&mut w);
}

/// Rule 9: a take is one step (the owner, 2026-11-08, finding 49: "B"), so
/// a sync locks `a` and writes it with no call in between: a write-open of
/// `a` made after it shows the synced file at once, and one of another file
/// is unaffected.
#[test]
fn r9_a_write_open_after_a_sync_shows_the_synced_file() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let m = w.save(&m1(), "a", 0);
    take(&mut w, &Step::Next(f));
    let (t, taken) = w.last_take().expect("the sync is taken").clone();
    assert!(t == f && taken.files.contains("a"));
    let h = w.open_write(&Client::Fork(f), "a");
    ok(&mut w);
    assert_eq!(
        w.view(h),
        w.read(TreeId::ROOT, "a").bytes(),
        "it shows the synced file"
    );
    let other = w.open_write(&Client::Fork(f), "b");
    let x = w.edit(h, LineEdit::Append).unwrap();
    w.close(h);
    w.close(other);
    let a = w.read(f, "a");
    assert_eq!((a.at(0), a.last()), (m.as_str(), x.as_str()));
    drain(&mut w);
}

/// Rule 9, the client's death (its end event) releases its locks: its
/// open file closes, saving its write calls, which wakes the waiting sync;
/// another fork's client's open closes with it.
#[test]
fn r9_client_death_releases_its_locks() {
    let mut w = world();
    let (f, g) = (w.create_fork(TreeId::ROOT), w.create_fork(TreeId::ROOT));
    let h = w.open_write(&Client::Fork(f), "a");
    let x = w.edit(h, LineEdit::At(1)).unwrap();
    let m = w.save(&m1(), "a", 0);
    take(&mut w, &Step::Next(f));
    assert!(w.waits().contains_key(&f));
    let n = w.save(&m1(), "b", 0);
    take(&mut w, &Step::Next(g));
    w.open_write(&Client::Fork(g), "a");
    w.end_client(f);
    w.end_client(g);
    ok(&mut w);
    assert!(w.waits().is_empty() && w.handles().next().is_none(),);
    drain(&mut w);
    assert!(has(&w, TreeId::ROOT, "a", &x) && has(&w, TreeId::ROOT, "a", &m));
    assert!(has(&w, TreeId::ROOT, "b", &n));
}

/// Rule 9, one of the root's clients dies (its end event) holding `a` open
/// for writing while the root's queue waits for it: its write calls are saved
/// as the kernel's close on exit would, which wakes the queue; and no lock
/// of it outlives it (`locks`).
#[test]
fn r9_a_now_dead_root_client_releases_its_locks() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let x = w.save(&Client::Fork(f), "b", 0);
    take(&mut w, &Step::Next(TreeId::ROOT));
    assert!(w.last_take().is_some_and(|(_, x)| x.files.contains("b")));
    let after = w.open_write(&m1(), "b");
    w.close(after);
    let h = w.open_write(&m1(), "a");
    let y = w.edit(h, LineEdit::At(0)).unwrap();
    let z = w.save(&Client::Fork(f), "a", 2);
    take(&mut w, &Step::Next(TreeId::ROOT));
    assert!(w.waits().contains_key(&TreeId::ROOT));
    let m2 = Client::Root("m2".into());
    let other = w.open_write(&m2, "d");
    w.end_root_client("m1");
    ok(&mut w);
    assert!(w.waits().is_empty(), "the death woke the root's queue");
    assert!(has(&w, TreeId::ROOT, "a", &y), "its write calls were saved");
    assert!(
        w.handles().any(|h| h.id == other),
        "another client's open stays"
    );
    next(&mut w, TreeId::ROOT);
    assert!(has(&w, TreeId::ROOT, "a", &z));
    w.close(other);
    drain(&mut w);
    assert!(has(&w, TreeId::ROOT, "b", &x));
    assert!(w.handles().next().is_none());
}

/// Rule 5 with open files: a file the program opened before begin keeps
/// writing to the fork, so a long-lived open file blocks no transaction.
/// One the transaction never wrote saves at once.
#[test]
fn r5_open_files_and_transactions() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let c = Client::Fork(f);
    let log = w.open_write(&c, "d");
    w.begin(f).unwrap();
    let s = w.save(&c, "a", 1);
    let l = w.edit(log, LineEdit::Append).unwrap();
    w.close(log);
    assert!(has(&w, f, "d", &l), "the log saves to the fork at once");
    assert!(!l.contains('~'), "not a staged line");
    ok(&mut w);
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
    assert!(has(&w, f, "a", &s));
    drain(&mut w);
}

/// R5.2 gone, R5.3 (the owner, 2026-11-05: "it should be able to acquire a
/// file lock"): a commit goes ahead while its own program holds files open,
/// opened before begin or inside the transaction. Both stay open and their
/// writes save into the fork at close, as ordinary later saves.
#[test]
fn r5_a_commit_goes_ahead_with_the_programs_own_files_open() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let t = f;
    let c = Client::Fork(f);
    let before = w.open_write(&c, "a");
    w.begin(f).unwrap();
    let staged = w.save(&c, "a", 1);
    let inside = w.open_write(&c, "b");
    let unsaved = w.edit(inside, LineEdit::Append).unwrap();
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
    let merge = w.head(t);
    ok(&mut w);
    assert!(w.forks[&f].txn.is_none());
    assert!(has(&w, t, "a", &staged), "the commit landed");
    assert!(!has(&w, t, "b", &unsaved), "still private until close");
    assert_eq!(w.handle(inside).target, Target::Tree(t));

    let late = w.edit(before, LineEdit::Append).unwrap();
    w.close(before);
    assert!(has(&w, t, "a", &late));
    assert!(has(&w, t, "a", &staged), "an append keeps the staged line");
    assert_eq!(
        git(w.repo(), &["rev-parse", &format!("{}^1", w.head(t))]),
        merge,
        "an ordinary later save, on the commit"
    );
    w.close(inside);
    assert!(has(&w, t, "b", &unsaved));
    ok(&mut w);
    drain(&mut w);
    assert!(has(&w, TreeId::ROOT, "a", &late) && has(&w, TreeId::ROOT, "b", &unsaved));
}

/// A file only the lost transaction made has no path in the fork, so an
/// open of it left over from the commit saves nothing (R11.3).
#[test]
fn r5_an_open_of_a_file_only_a_lost_transaction_made_saves_nothing() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let t = f;
    let c = Client::Fork(f);
    w.begin(f).unwrap();
    let new = w.create(&c, "n").expect("a new file");
    w.edit(new, LineEdit::Append).unwrap();
    w.save(&c, "a", 1);
    let m = w.save(&m1(), "a", 1);
    next(&mut w, t);
    assert!(has(&w, t, "a", &m));
    assert!(matches!(w.commit_txn(f), Ok(TxnOutcome::Lost { .. })));
    ok(&mut w);
    let n = w.orphans().len();
    let head = w.head(t);
    w.close(new);
    assert_eq!(w.orphans().len(), n + 1, "its write is dropped, and named");
    assert_eq!(w.head(t), head);
    drain(&mut w);
}

/// R5.3: while a transaction is open, another program's close, unlink and
/// rename of its files return at once and change nothing; they apply in the
/// order they were made after the commit, the save on the commit's head.
#[test]
fn r5_io_on_a_transactions_files_is_deferred_past_its_commit() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let t = f;
    let (c, other) = (Client::Fork(f), Client::Outside(f));
    let open = w.open_write(&other, "a");
    w.begin(f).unwrap();
    let staged = w.save(&c, "a", 1);
    let staged_b = w.save(&c, "b", 0);
    let staged_d = w.save(&c, "d", 0);
    let start = w.head(t);
    ok(&mut w);

    let x = w.edit(open, LineEdit::Append).unwrap();
    w.close(open);
    w.unlink(&other, "b"); // returns at once
    w.rename(&other, "d", "d2"); // so does a rename
    let reopened = w.open_write(&other, "a");
    w.write(reopened, Write::Truncate(0));
    w.close(reopened);
    ok(&mut w);
    assert_eq!(w.head(t), start, "none of it moved the fork");
    assert_eq!(w.deferred(f).len(), 4);
    assert!(w.path_of(t, "b").is_some() && w.path_of(t, "d").as_deref() == Some("d"));

    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
    ok(&mut w);
    assert!(w.deferred(f).is_empty());
    assert!(w.handles().next().is_none());
    let result = w.ended.last().unwrap().result.clone().unwrap();
    assert!(has(&w, f, "a", &staged) || w.file(&result, "a").holds(&staged));
    assert!(w.file(&result, "b").holds(&staged_b));
    assert!(w.file(&result, "d").holds(&staged_d));
    assert_eq!(w.path_of(t, "b"), None, "the unlink ran");
    assert_eq!(w.path_of(t, "d").as_deref(), Some("d2"), "the rename ran");
    assert_eq!(
        w.read(t, "a").bytes(),
        "",
        "in order: the truncate came after the save of x {x}"
    );
    drain(&mut w);
}

/// R5.3 with rule 9: a path call made right after a take of its file, whose
/// file is a transaction's, defers: no call waits on a take (one step) or on
/// a transaction.
#[test]
fn r5_a_path_call_after_a_take_defers() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let t = f;
    let (c, other) = (Client::Fork(f), Client::Outside(f));
    w.begin(f).unwrap();
    w.save(&c, "b", 0);
    w.save(&m1(), "b", 1);
    take(&mut w, &Step::Next(t));
    w.unlink(&other, "b");
    let h = *w.deferred(f).last().expect("it defers");
    assert!(w.handle(h).deferred);
    assert!(w.path_of(t, "b").is_some());
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
    ok(&mut w);
    assert_eq!(w.path_of(t, "b"), None);
    drain(&mut w);
}

/// R5.3: a rollback applies the deferred calls the same way, on the
/// fork's head, and nothing of the transaction is in them.
#[test]
fn r5_a_rollback_applies_what_it_deferred() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let t = f;
    let (c, other) = (Client::Fork(f), Client::Outside(f));
    let open = w.open_write(&other, "a");
    w.begin(f).unwrap();
    let staged = w.save(&c, "a", 1);
    let start = w.head(t);
    let x = w.edit(open, LineEdit::Append).unwrap();
    w.close(open);
    w.unlink(&other, "a");
    assert_eq!(w.head(t), start);
    w.rollback(f).unwrap();
    ok(&mut w);
    assert!(w.file(&w.head(t), "a").holds(&x), "the save ran");
    assert!(
        !w.file(&w.head(t), "a").holds(&staged),
        "the staged line went"
    );
    assert_eq!(w.path_of(t, "a"), None, "then the unlink");
    drain(&mut w);
    assert_eq!(w.path_of(TreeId::ROOT, "a"), None);
}

/// R5.3 with R9.6: the client's death rolls the transaction back, the
/// deferred calls apply, and then its open files close.
#[test]
fn r5_a_clients_death_applies_what_its_transaction_deferred() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let t = f;
    let (c, other) = (Client::Fork(f), Client::Outside(f));
    let deferred = w.open_write(&other, "a");
    let stays = w.open_write(&c, "a");
    w.begin(f).unwrap();
    let staged = w.save(&c, "a", 1);
    let x = w.edit(deferred, LineEdit::Append).unwrap();
    w.close(deferred);
    let y = w.edit(stays, LineEdit::Append).unwrap();
    w.end_client(f);
    ok(&mut w);
    let a = w.file(&w.head(t), "a");
    assert!(a.holds(&x) && a.holds(&y) && !a.holds(&staged));
    assert!(w.handles().next().is_none());
    drain(&mut w);
    assert!(has(&w, TreeId::ROOT, "a", &x) && has(&w, TreeId::ROOT, "a", &y));
}

/// R5.3 with rule 12 (the owner, 2026-11-05: "the crash rolls the
/// transaction back and applies the call"): a crash rolls the open
/// transaction back, leaving nothing of it, then applies the calls it
/// deferred, in order; an open of its file is replayed from the journal into
/// the fork, as its close would have saved it.
#[test]
fn r5_a_crash_rolls_back_and_applies_what_it_deferred() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let t = f;
    let (c, other) = (Client::Fork(f), Client::Outside(f));
    let (closed, open) = (w.open_write(&other, "a"), w.open_write(&other, "a"));
    w.begin(f).unwrap();
    let staged = w.save(&c, "a", 1);
    let staged_b = w.save(&c, "b", 0);
    let x = w.edit(closed, LineEdit::Append).unwrap();
    w.close(closed);
    w.unlink(&other, "b");
    let y = w.edit(open, LineEdit::Append).unwrap();
    assert_eq!(w.deferred(f).len(), 2);
    w.crash();
    ok(&mut w);
    assert!(w.forks[&f].txn.is_none(), "the crash rolled it back");
    assert_eq!(w.ended.last().unwrap().how, TxnEnd::Rollback);
    assert!(w.deferred(f).is_empty() && w.handles().next().is_none());
    let a = w.file(&w.head(t), "a");
    assert!(a.holds(&x) && a.holds(&y) && !a.holds(&staged));
    assert!(!w.file(&w.head(t), "b").holds(&staged_b));
    assert_eq!(w.path_of(t, "b"), None, "the deferred unlink applied");
    assert_eq!(w.commit_txn(f), Err(ModelError::NoTransaction));
    drain(&mut w);
    assert!(has(&w, TreeId::ROOT, "a", &x) && has(&w, TreeId::ROOT, "a", &y));
    assert!(!has(&w, TreeId::ROOT, "a", &staged));
}

/// R5.3 with R3c.2 and rule 9: a deferred save has not happened, so its file
/// counts as open: a sync of that file waits for it, and the save is never
/// credited with an upstream's line its program could not read. The transaction
/// ending wakes the sync.
#[test]
fn r5_a_deferred_save_holds_back_a_sync_of_its_file() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let t = f;
    let (c, other) = (Client::Fork(f), Client::Outside(f));
    let open = w.open_write(&other, "a");
    w.begin(f).unwrap();
    w.save(&c, "a", 2);
    w.edit(open, LineEdit::At(1)).unwrap();
    w.close(open);
    let m = w.save(&m1(), "a", 1);
    take(&mut w, &Step::Next(t));
    assert!(w.waits().contains_key(&t), "the sync waits");
    assert!(!has(&w, t, "a", &m));
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
    ok(&mut w);
    assert!(!w.waits().contains_key(&t), "the transaction's end woke it");
    drain(&mut w);
    assert!(
        w.collisions.iter().any(|c| c.tree == TreeId::ROOT),
        "written without reading the root's line, the save collides with it"
    );
    assert!(has(&w, TreeId::ROOT, "a", &m), "and the upstream wins");
}

/// R10.3 (finding 31): the root renames d over c and then unlinks d, while a fork
/// unlinks d. The two deletes agree, so the fork's applies, and d's name is
/// then a version not written over the rename-over's, which is not half a
/// rename-over.
#[test]
fn r10_an_agreeing_delete_of_a_rename_overs_file_agrees() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    w.unlink(&Client::Fork(f), "d");
    ok(&mut w);
    w.rename(&m1(), "d", "c");
    ok(&mut w);
    w.unlink(&m1(), "c");
    ok(&mut w);
    next(&mut w, TreeId::ROOT);
    drain(&mut w);
    assert!(w.collisions.is_empty(), "{:?}", w.collisions);
    for t in [TreeId::ROOT, f] {
        let head = w.head(t);
        assert_eq!(w.lookup(&head, "d"), None);
        assert_eq!(w.lookup(&head, "c"), None);
    }
}

/// R5.1 and R9.9, read from git: a take's decision lives in its own result
/// commit (its tree named), with no notes ref; the losing commit, which cannot
/// be edited, holds none and keeps its collision ref; and the record reaches
/// the mirror with the root's history, a transaction's and a nested fork's
/// included.
#[test]
fn r5_a_decision_is_in_its_result_commit_with_its_tree() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let c = Client::Fork(f);
    w.save(&m1(), "a", 1);
    w.save(&c, "a", 1);
    w.begin(f).unwrap();
    w.save(&Client::Outside(f), "b", 1);
    w.save(&c, "b", 1);
    assert!(matches!(w.commit_txn(f), Ok(TxnOutcome::Lost { .. })));
    ok(&mut w);
    drain(&mut w);
    assert!(w.collisions.iter().any(|x| x.tree == TreeId::ROOT));
    assert!(w.collisions.iter().any(|x| x.tree == f));
    let refs = git(w.repo(), &["for-each-ref", "--format=%(refname)"]);
    assert!(!refs.contains("refs/notes"), "no notes ref: {refs}");
    for x in &w.collisions {
        let d = agentfs_model::world::Decision::of(x, w.names());
        for repo in [w.repo(), w.mirror().repo()] {
            let on = decisions_in(repo, &["-1", &x.result]);
            assert!(on.iter().any(|(_, n)| *n == d), "the result holds it");
            assert!(decisions_in(repo, &["-1", &x.loser]).is_empty());
            let tree = git(
                repo,
                &[
                    "log",
                    "-1",
                    "--format=%(trailers:key=Agentfs-Tree,valueonly)",
                    &x.result,
                ],
            );
            assert_eq!(tree, d.tree, "and names the tree it belongs to");
            assert_eq!(git(repo, &["rev-parse", &x.reference]), x.loser);
        }
    }
}

/// R10.8 (finding 32): a save that makes a line unfinished (no final newline)
/// would land in front of a line the upstream keeps. A first save cuts the file to
/// one unfinished line; the root edits a line it deleted, so that save loses whole;
/// the fork's next save, written over its own first line, would put the
/// upstream's next line on one line of bytes nobody wrote, so it loses whole too.
#[test]
fn r10_an_unfinished_line_before_a_kept_line_loses_whole() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let c = Client::Fork(f);
    let cut = w.open_trunc(&c, "c");
    w.write(
        cut,
        Write::Pwrite {
            offset: 0,
            bytes: "y".into(),
        },
    );
    w.close(cut);
    w.save(&m1(), "c", 1);
    let root = bytes(&w, TreeId::ROOT, "c");
    let edit = w.open_write(&c, "c");
    w.write(
        edit,
        Write::Pwrite {
            offset: 0,
            bytes: "n6".into(),
        },
    );
    w.close(edit);
    let second = w.head(f);
    next(&mut w, TreeId::ROOT);
    next(&mut w, TreeId::ROOT);
    let read = w.read(TreeId::ROOT, "c");
    assert_eq!(read.bytes(), root, "the root reads as before");
    assert_eq!(
        read.bytes().lines().count(),
        read.live().count(),
        "{:?}: only a file's last line can lack a newline",
        read.bytes()
    );
    let lost = w
        .collisions
        .iter()
        .find(|x| x.tree == TreeId::ROOT && x.loser == second)
        .expect("the second save lost, with its record");
    assert_eq!(lost.by, By::Line);
    assert!(
        w.is_ancestor(&second, &w.head(TreeId::ROOT)),
        "and stays in history"
    );
    assert_eq!(git(w.repo(), &["rev-parse", &lost.reference]), second);
    drain(&mut w);
    assert_eq!(bytes(&w, f, "c"), root, "the fork converges");
}

/// R10.7: an edited line keeps its place. When the root appends after the line a
/// fork edits, the file reads the edit, then the append.
#[test]
fn r10_an_edited_line_keeps_its_place_before_an_append() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let old = bytes(&w, TreeId::ROOT, "b");
    let edit = w.open_write(&Client::Fork(f), "b");
    let last = old.trim_end_matches('\n').rfind('\n').map_or(0, |n| n + 1);
    w.write(
        edit,
        Write::Pwrite {
            offset: last,
            bytes: "E".into(),
        },
    );
    w.close(edit);
    calls(&mut w, &m1(), "b", &[Write::Append("m7\n".into())]);
    drain(&mut w);
    assert!(w.collisions.is_empty(), "{:?}", w.collisions);
    let now = bytes(&w, TreeId::ROOT, "b");
    assert_eq!(now.lines().last(), Some("m7"), "{now:?}");
    assert!(now.contains("E"), "{now:?}");
    assert_eq!(now.len(), old.len() + 3, "{now:?}");
}

/// R10.8's guard: an unfinished line past every line the upstream added, with no
/// live line after it, lands: no join and no collision.
#[test]
fn r10_an_unfinished_save_after_the_upstreams_last_line_lands() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    calls(&mut w, &m1(), "b", &[Write::Append("m7\n".into())]);
    calls(&mut w, &Client::Fork(f), "b", &[Write::Append("n9".into())]);
    drain(&mut w);
    assert!(w.collisions.is_empty(), "{:?}", w.collisions);
    let b = bytes(&w, TreeId::ROOT, "b");
    assert!(b.ends_with("m7\nn9"), "{b:?}");
    assert_eq!(bytes(&w, f, "b"), b);
}

/// R10.8 and R10.6's guard: a fork's finished append and an upstream's finished
/// line neither lose nor hide: the sync shows both, and the save lands.
#[test]
fn r10_an_append_after_a_finished_line_still_lands() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    calls(
        &mut w,
        &Client::Fork(f),
        "b",
        &[Write::Append("n9\n".into())],
    );
    calls(&mut w, &m1(), "b", &[Write::Append("m7\n".into())]);
    next(&mut w, f);
    let b = bytes(&w, f, "b");
    assert!(b.ends_with("m7\nn9\n"), "{b:?}");
    drain(&mut w);
    assert!(w.collisions.is_empty(), "{:?}", w.collisions);
    assert_eq!(bytes(&w, TreeId::ROOT, "b"), b);
    assert_eq!(bytes(&w, f, "b"), b);
}

// ── rule 10: line identity ──────────────────────────────────────────────

/// One open, one write call, one close: the version the call made, if any.
fn call(w: &mut World, c: &Client, p: &str, call: LineEdit) -> Option<String> {
    let h = w.open_write(c, p);
    let made = w.edit(h, call);
    w.close(h);
    made
}

/// The live lines' version ids of `p` in `t`.
fn ids(w: &World, t: TreeId, p: &str) -> Vec<String> {
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
            state: State::Live(format!("{t}\n")),
        })
        .collect();
    c
}

fn ends_converged_without_collisions(w: &mut Checked) {
    drain(w);
    assert!(w.collisions.is_empty(), "{:?}", w.collisions);
}

/// Rule 10: an insert shifts the lines after it without changing their
/// identity. The fork inserts a line at the top while the root writes line
/// 2; then the root inserts at the top while the fork writes line 2. Each
/// edit lands on the line its writer meant, now one further down, with no
/// collision.
#[test]
fn r10_an_insert_shifts_lines_without_changing_their_identity() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let x = call(&mut w, &Client::Fork(f), "a", LineEdit::Insert(0)).unwrap();
    let m = w.save(&m1(), "a", 2);
    ends_converged_without_collisions(&mut w);
    let a = w.read(TreeId::ROOT, "a");
    assert_eq!((a.at(0), a.at(3)), (x.as_str(), m.as_str()));
    let n = call(&mut w, &m1(), "b", LineEdit::Insert(0)).unwrap();
    let y = w.save(&Client::Fork(f), "b", 2);
    ends_converged_without_collisions(&mut w);
    let b = w.read(TreeId::ROOT, "b");
    assert_eq!((b.at(0), b.at(3)), (n.as_str(), y.as_str()));
    assert_eq!(ids(&w, f, "b"), ids(&w, TreeId::ROOT, "b"));
}

/// Rule 10, two inserts at one place: both stay, the upstream's first (a
/// design choice), with no collision, whichever reaches the fork first.
#[test]
fn r10_concurrent_inserts_at_one_place_keep_both_the_upstreams_first() {
    for sync_first in [false, true] {
        let mut w = world();
        let f = w.create_fork(TreeId::ROOT);
        let x = call(&mut w, &Client::Fork(f), "a", LineEdit::Insert(1)).unwrap();
        let m = call(&mut w, &m1(), "a", LineEdit::Insert(1)).unwrap();
        if sync_first {
            next(&mut w, f);
            assert_eq!(ids(&w, f, "a")[..4], ["a0", &m, &x, "a1"]);
        }
        ends_converged_without_collisions(&mut w);
        let a = ids(&w, TreeId::ROOT, "a");
        assert_eq!(a[..4], ["a0", &m, &x, "a1"], "{sync_first}");
    }
}

/// Rule 10, a delete against a concurrent edit of the same line: a delete
/// is a version of the line, so the per-line rule decides. The lines hold
/// distinct bytes (among identical ones the diff picks which one went:
/// `r10_repeated_identical_lines_align_by_the_diff`). The fork deletes
/// line 1 while the root writes it: the root's line stays. The root deletes line 1
/// while the fork writes it: the line stays deleted. Both delete it:
/// they agree, no collision.
#[test]
fn r10_a_delete_against_a_concurrent_edit_the_upstream_wins() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let t = distinct(&mut w, "a");
    call(&mut w, &Client::Fork(f), "a", LineEdit::Delete(1));
    let m = w.save(&m1(), "a", 1);
    drain(&mut w);
    assert_eq!(ids(&w, TreeId::ROOT, "a")[..3], [t[0].as_str(), &m, &t[2]],);
    let c = w.collisions.last().expect("a collision").clone();
    assert_eq!((c.lines, c.won), (vec!["a1".to_string()], vec![m.clone()]));
    let t = distinct(&mut w, "b");
    call(&mut w, &m1(), "b", LineEdit::Delete(1));
    let x = w.save(&Client::Fork(f), "b", 1);
    drain(&mut w);
    assert_eq!(ids(&w, TreeId::ROOT, "b")[..2], [t[0].as_str(), &t[2]],);
    assert_eq!(w.collisions.last().map(|c| c.lost.clone()), Some(vec![x]));
    let t = distinct(&mut w, "c");
    let before = w.collisions.len();
    call(&mut w, &Client::Fork(f), "c", LineEdit::Delete(1));
    call(&mut w, &m1(), "c", LineEdit::Delete(1));
    drain(&mut w);
    assert_eq!(w.collisions.len(), before, "two deletes agree");
    assert_eq!(ids(&w, TreeId::ROOT, "c")[..2], [t[0].as_str(), &t[2]]);
}

/// Lines 0 to 3 of `p` given distinct bytes by the root, then every tree
/// synced: their bytes, in order.
fn distinct(w: &mut Checked, p: &str) -> Vec<String> {
    let h = w.open_write(&m1(), p);
    let made: Vec<String> = (0..4).filter_map(|i| w.edit(h, LineEdit::At(i))).collect();
    w.close(h);
    drain(w);
    made
}

/// Rule 10, a move: the save sees only bytes, so its one alignment reads a
/// moved line as a delete plus a new line holding the same bytes. With no
/// concurrent edit of that line it lands as the program wrote it. Against
/// the root's concurrent edit of the moved line, the delete loses to the root's
/// edit, so the whole save loses (rule 3e), its new line too: the root's edit
/// shows at the old place only, and the delete's collision and the new
/// line's (`By::Whole`) name the save. The root's move against the
/// fork's edit of that line: the root's delete wins, the fork's edit
/// loses.
#[test]
fn r10_a_move_is_a_delete_plus_an_insert() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let t = distinct(&mut w, "a");
    let mv = LineEdit::Move { from: 0, to: 3 };
    call(&mut w, &Client::Fork(f), "a", mv.clone());
    let m = w.save(&m1(), "a", 5);
    ends_converged_without_collisions(&mut w);
    let a = w.read(TreeId::ROOT, "a");
    assert_eq!(a.texts(), [&t[1], &t[2], &t[3], &t[0], BLANK, &m],);
    assert!(!a.contains(&t[0]), "the moved line is new");
    let t = distinct(&mut w, "b");
    call(&mut w, &Client::Fork(f), "b", mv.clone());
    let m = w.save(&m1(), "b", 0);
    drain(&mut w);
    assert_eq!(
        w.read(TreeId::ROOT, "b").texts()[..5],
        [m.as_str(), &t[1], &t[2], &t[3], BLANK],
        "the root's edit, and not the old bytes"
    );
    let n = w.collisions.len();
    let (del, new) = (&w.collisions[n - 2], &w.collisions[n - 1]);
    assert_eq!(
        (del.by, &del.lines, &del.won),
        (By::Line, &vec!["b0".to_string()], &vec![m.clone()])
    );
    assert_eq!(
        (new.by, &new.won, &new.loser),
        (By::Whole, &vec![m], &del.loser)
    );
    let t = distinct(&mut w, "c");
    let before = w.collisions.len();
    call(&mut w, &m1(), "c", mv);
    let x = w.save(&Client::Fork(f), "c", 0);
    drain(&mut w);
    assert_eq!(
        w.read(TreeId::ROOT, "c").texts()[..4],
        [t[1].as_str(), &t[2], &t[3], &t[0]],
    );
    let lost: Vec<String> = w.collisions[before..]
        .iter()
        .flat_map(|c| c.lost.clone())
        .collect();
    assert_eq!(lost, [x]);
}

/// Rule 10, repeated lines. Six blank lines, and the fork inserts a
/// blank line at the top while the root writes line 1. In bytes the new line
/// could be any of seven blank lines; git's diff takes the last (it trims
/// the common prefix first), so the root's edit stays on line 1, not line 2,
/// as the fork's program would read it (a cost: git-model.md §16). No
/// line is lost or doubled, and no collision. Then braces: the root writes
/// `{ x } { y }`; the fork's program rewrites the whole file with a
/// third block between, while the root edits `y`. The diff anchors on the
/// lines that occur once, so the rewrite's new braces are new lines and
/// the root's edit stays on `y`'s line, no collision.
#[test]
fn r10_repeated_identical_lines_align_by_the_diff() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let h = w.open_write(&Client::Fork(f), "a");
    w.edit_text(h, LineEdit::Insert(0), BLANK);
    w.close(h);
    let m = w.save(&m1(), "a", 1);
    ends_converged_without_collisions(&mut w);
    let a = w.read(TreeId::ROOT, "a");
    assert_eq!(a.texts(), [BLANK, &m, BLANK, BLANK, BLANK, BLANK, BLANK],);
    let h = w.open_write(&m1(), "b");
    for (i, t) in ["{", "x", "}", "{", "y", "}"].iter().enumerate() {
        w.edit_text(h, LineEdit::At(i), t);
    }
    w.close(h);
    drain(&mut w);
    let block = ["{", "x", "}", "{", "z", "}", "{", "y", "}"];
    let h = w.open_write(&Client::Fork(f), "b");
    w.edit(h, LineEdit::Rewrite(texts(&block)));
    let y = w.save(&m1(), "b", 4);
    w.close(h);
    ends_converged_without_collisions(&mut w);
    let b = w.read(TreeId::ROOT, "b").texts();
    assert_eq!(b, ["{", "x", "}", "{", "z", "}", "{", &y, "}"],);
}

// ── rule 11: deletes and renames ────────────────────────────────────────

/// Rule 11, an unlink against a concurrent edit, in each direction. The
/// fork unlinks `a` while the root writes a line of it: the unlink never
/// saw that line, so it loses whole and `a` stays, with the root's line. The root
/// unlinks `b` while the fork writes a line of it: the root's unlink wins,
/// the fork's line loses. Both are recorded collisions.
#[test]
fn r11_an_unlink_against_a_concurrent_edit_the_upstream_wins() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    w.unlink(&Client::Fork(f), "a");
    assert_eq!(w.path_of(f, "a"), None, "unlinked at once");
    let m = w.save(&m1(), "a", 0);
    drain(&mut w);
    assert!(has(&w, TreeId::ROOT, "a", &m) && has(&w, f, "a", &m));
    let c = w.collisions.last().expect("a collision").clone();
    assert!(
        c.by == By::File && c.lines == ["a"] && c.won == [m.clone()],
        "{c:?}"
    );
    w.unlink(&m1(), "b");
    let x = w.save(&Client::Fork(f), "b", 0);
    drain(&mut w);
    for t in [TreeId::ROOT, f] {
        assert_eq!(w.path_of(t, "b"), None, "{t:?}");
    }
    let c = w.collisions.last().expect("a collision").clone();
    assert!(c.by == By::File && c.lost == [x.clone()], "{c:?}");
}

/// Rule 11, an unlink after the sync: the fork saw the root's line, so its
/// unlink wins upstream by lineage, with no collision.
#[test]
fn r11_an_unlink_after_the_sync_wins() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    w.save(&m1(), "a", 0);
    next(&mut w, f);
    w.unlink(&Client::Fork(f), "a");
    ends_converged_without_collisions(&mut w);
    assert_eq!(w.path_of(TreeId::ROOT, "a"), None);
}

/// Rule 11, a rename and a concurrent edit both apply: a file keeps its
/// identity through a rename, so the edit follows it to its new path, in
/// each direction.
#[test]
fn r11_a_rename_and_a_concurrent_edit_both_apply() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    w.rename(&Client::Fork(f), "a", "e");
    let m = w.save(&m1(), "a", 0);
    ends_converged_without_collisions(&mut w);
    assert!(has(&w, TreeId::ROOT, "e", &m));
    w.rename(&m1(), "b", "g");
    let x = w.save(&Client::Fork(f), "b", 0);
    ends_converged_without_collisions(&mut w);
    assert!(has(&w, TreeId::ROOT, "g", &x) && has(&w, f, "g", &x));
}

/// Rule 11, two concurrent renames of one file: the upstream's name wins.
/// Two files renamed to one path: the upstream's file keeps it, the
/// fork's keeps its old name.
#[test]
fn r11_concurrent_renames_the_upstream_wins() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    w.rename(&Client::Fork(f), "a", "e");
    w.rename(&m1(), "a", "g");
    drain(&mut w);
    for t in [TreeId::ROOT, f] {
        assert_eq!(w.path_of(t, "a").as_deref(), Some("g"), "{t:?}");
    }
    w.rename(&Client::Fork(f), "b", "h");
    w.rename(&m1(), "c", "h");
    // The root's queue decides before the fork syncs: a sync first would
    // show the fork two files named h (not modelled: MODEL.md).
    next(&mut w, TreeId::ROOT);
    drain(&mut w);
    for t in [TreeId::ROOT, f] {
        let names = (w.path_of(t, "b"), w.path_of(t, "c"));
        assert_eq!(names, (Some("b".into()), Some("h".into())), "{t:?}");
    }
    let c = w.collisions.last().expect("a clash").clone();
    assert!(c.by == By::File && c.lines == ["b"], "{c:?}");
}

/// Finding 11, two renames to one path before the upstream decides (a
/// choice, recommended without objection): the fork renames `b` to `h`
/// and the root renames `c` to `h`, and the fork syncs first. It keeps its
/// own name until the root's queue decides, and shows the root's file under its
/// previous name, `c`: never two files at one path. Then the root's queue
/// decides (the root wins) and the sync shows the root's names. Also with
/// rename-overs: both rename a file over `d`.
#[test]
fn r11_two_renames_to_one_path_keep_the_forks_name_until_decided() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let t = f;
    w.rename(&Client::Fork(f), "b", "h");
    w.rename(&m1(), "c", "h");
    next(&mut w, t);
    assert_eq!(w.path_of(t, "b").as_deref(), Some("h"), "its own name");
    assert_eq!(
        w.path_of(t, "c").as_deref(),
        Some("c"),
        "the root's file under its previous name"
    );
    drain(&mut w);
    for t in [TreeId::ROOT, t] {
        let names = (w.path_of(t, "b"), w.path_of(t, "c"));
        assert_eq!(names, (Some("b".into()), Some("h".into())), "{t:?}");
    }
    w.rename(&Client::Fork(f), "a", "d");
    w.rename(&m1(), "b", "d");
    next(&mut w, f);
    assert_eq!(w.path_of(f, "a").as_deref(), Some("d"),);
    assert_eq!(w.path_of(f, "b").as_deref(), Some("b"),);
    drain(&mut w);
    for t in [TreeId::ROOT, f] {
        assert_eq!(w.path_of(t, "b").as_deref(), Some("d"), "{t:?}");
    }
}

/// Rule 11, a rename-over (`rename(2)` onto an existing file, the atomic
/// save idiom) is one call: it applies or loses whole. The fork renames
/// `a` over `b` while the root writes a line of `b`: the unlink of `b` never
/// saw it, so the whole rename-over loses: `a` keeps its name, `b` keeps
/// the root's line. Without the concurrent edit it applies. The root's rename-over
/// takes a fork edit of the renamed file with it, and the replaced
/// file's edit loses.
#[test]
fn r11_a_rename_over_applies_or_loses_whole() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    w.rename(&Client::Fork(f), "a", "b");
    let m = w.save(&m1(), "b", 0);
    drain(&mut w);
    for t in [TreeId::ROOT, f] {
        assert_eq!(w.path_of(t, "a").as_deref(), Some("a"), "{t:?}");
        assert!(has(&w, t, "b", &m));
    }
    let lost: BTreeSet<&str> = w.collisions.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(lost, BTreeSet::from(["a", "b"]), "both halves lose");
    let before = w.collisions.len();
    w.rename(&Client::Fork(f), "c", "d");
    drain(&mut w);
    assert_eq!(w.collisions.len(), before, "it applies");
    assert_eq!(w.path_of(TreeId::ROOT, "c").as_deref(), Some("d"));
    assert_eq!(w.path_of(TreeId::ROOT, "d"), None);
    w.rename(&m1(), "a", "b");
    let kept = w.save(&Client::Fork(f), "a", 1);
    let gone = w.save(&Client::Fork(f), "b", 1);
    drain(&mut w);
    assert!(has(&w, TreeId::ROOT, "b", &kept));
    assert_eq!(w.path_of(TreeId::ROOT, "a").as_deref(), Some("b"));
    assert_eq!(w.path_of(TreeId::ROOT, "b"), None);
    let lost: Vec<String> = w.collisions[before..]
        .iter()
        .flat_map(|c| c.lost.clone())
        .collect();
    assert_eq!(lost, [gone]);
}

/// Rule 11, a rename-over's other half: the fork renames `a` over `b`
/// while the root renames `a` to `g`. The fork's rename loses to the root's
/// name, so its unlink of `b` loses with it (one call): `b` keeps its path
/// and its lines, and `a` is at `g`, in both trees.
#[test]
fn r11_a_rename_over_whose_rename_loses_keeps_the_replaced_file() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let x = w.save(&m1(), "b", 0);
    drain(&mut w);
    w.rename(&Client::Fork(f), "a", "b");
    w.rename(&m1(), "a", "g");
    drain(&mut w);
    for t in [TreeId::ROOT, f] {
        assert_eq!(w.path_of(t, "a").as_deref(), Some("g"), "{t:?}");
        assert_eq!(w.path_of(t, "b").as_deref(), Some("b"), "{t:?}");
        assert!(has(&w, t, "b", &x), "{t:?}");
    }
    let lost: BTreeSet<&str> = w.collisions.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(lost, BTreeSet::from(["a", "b"]), "both halves lose");
}

/// Rule 11 with rule 9 (R9.7): a take is one step (the owner, 2026-11-08,
/// finding 49: "B"), so an unlink or a rename of a file it locked, or onto
/// one, runs after its write, on the synced file. A take that brings an
/// unlink waits for the fork's open writer of that file.
#[test]
fn r11_an_unlink_or_rename_of_a_held_file_runs_after_the_take() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let c = Client::Fork(f);
    let m = w.save(&m1(), "a", 0);
    take(&mut w, &Step::Next(f));
    assert!(w.last_take().is_some_and(|(_, x)| x.files.contains("a")));
    assert!(has(&w, f, "a", &m), "the take wrote a");
    w.unlink(&c, "a");
    assert_eq!(w.path_of(f, "a"), None, "it ran");
    let n = w.save(&m1(), "c", 0);
    take(&mut w, &Step::Next(f));
    assert!(has(&w, f, "c", &n), "the take wrote c");
    w.rename(&c, "d", "c");
    assert_eq!(w.path_of(f, "d").as_deref(), Some("c"));
    drain(&mut w);
    w.unlink(&m1(), "b");
    let open = w.open_write(&c, "b");
    take(&mut w, &Step::Next(f));
    assert!(w.waits().contains_key(&f));
    let x = w.edit(open, LineEdit::At(0)).unwrap();
    w.close(open);
    drain(&mut w);
    assert_eq!(w.path_of(f, "b"), None);
    assert!(w.collisions.iter().any(|c| c.lost.contains(&x)));
}

/// Rule 11 in one tree, as on Linux: an open file follows its file through
/// a rename, and a file unlinked while open saves nothing at close.
#[test]
fn r11_an_open_file_follows_a_rename_and_an_unlinked_one_saves_nothing() {
    let mut w = world();
    let m2 = Client::Root("m2".into());
    let h = w.open_write(&m1(), "a");
    let x = w.edit(h, LineEdit::At(0)).unwrap();
    w.rename(&m2, "a", "e");
    w.close(h);
    assert!(has(&w, TreeId::ROOT, "e", &x));
    let h = w.open_write(&m1(), "b");
    let y = w.edit(h, LineEdit::At(0)).unwrap();
    w.unlink(&m2, "b");
    let head = w.head(TreeId::ROOT);
    w.close(h);
    assert_eq!(w.head(TreeId::ROOT), head, "nothing saved");
    assert!(!w.file(&head, "b").holds(&y));
    drain(&mut w);
}

// ── rule 11: creates, objects and the atomic save (git-model.md §7) ─────

/// An editor's atomic save of `p` as client `c`: create `tmp`, write what
/// it read of `p` with `edit` applied (a new line holding `text`), close,
/// rename `tmp` over `p`. Returns the bytes it saved.
fn atomic_save(
    w: &mut World,
    c: &Client,
    p: &str,
    tmp: &str,
    edit: LineEdit,
    text: &str,
) -> String {
    let read = w.client_reads(c, p).bytes();
    let new = agentfs_model::program::edited(&read, &edit, text);
    let h = w.create(c, tmp).expect("an unborn identity left");
    w.write(h, Write::Append(new.clone()));
    w.close(h);
    w.rename(c, tmp, p);
    new
}

/// Rule 11, creates: a create makes a new file identity (the next unborn
/// one), saved at once with its path; its content is a later save; both
/// land like any save. Two trees creating files at once both keep theirs.
#[test]
fn r11_a_create_lands_like_any_save() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let h = w.create(&Client::Fork(f), "t1").unwrap();
    w.write(h, Write::Append("x\n".into()));
    w.close(h);
    let k = w.create(&m1(), "t2").unwrap();
    w.close(k);
    assert_eq!(w.lookup(&w.head(f), "t1").as_deref(), Some("g"));
    assert_eq!(w.lookup(&w.head(TreeId::ROOT), "t2").as_deref(), Some("h"));
    assert!(
        w.lookup(&w.head(TreeId::ROOT), "t1").is_none(),
        "landing is a queued step"
    );
    drain(&mut w);
    for t in [TreeId::ROOT, f] {
        assert_eq!(w.read(t, "t1").bytes(), "x\n", "{t:?}");
        assert_eq!(w.read(t, "t2").bytes(), "", "{t:?}");
    }
}

/// Rule 11, the atomic save: a rename-over from a new file is a rewrite of
/// the replaced file, which keeps its identity, so it merges per line with
/// the upstream's concurrent edit of another line (a plain rename-over would
/// lose whole: `r11_a_rename_over_applies_or_loses_whole`). The new file ends.
#[test]
fn r11_an_atomic_save_keeps_the_files_identity_and_merges_per_line() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let c = Client::Fork(f);
    atomic_save(&mut w, &c, "a", "a.tmp", LineEdit::At(0), "x");
    let m = w.save(&m1(), "a", 4);
    let fork = w.read(f, "a");
    assert_eq!(fork.name.line, "a", "the replaced file keeps its identity");
    assert_eq!(w.lookup(&w.head(f), "a.tmp"), None, "the new file ended");
    assert!(!w.file(&w.head(f), "g").name.live());
    drain(&mut w);
    let root = w.read(TreeId::ROOT, "a");
    assert_eq!(root.texts()[0], "x", "the atomic save landed");
    assert!(
        root.contains(&m),
        "and the root's edit of another line stayed"
    );
    assert!(w.collisions.is_empty(), "{:?}", w.collisions);
    assert_eq!(w.read(f, "a"), root);
}

/// Rule 11 with git-model.md §7's objects: an old writer of the file an
/// editor replaces by an atomic save keeps its object; its writes never
/// reach the replacement, its close saves nothing and an `OrphanDropped`
/// event names its calls. A take of the file no longer waits for that
/// writer's close, since no path names the old object. In the root (two
/// clients) and in a fork (one program with both opens).
#[test]
fn r11_an_old_writer_after_an_atomic_save_saves_nothing() {
    for in_fork in [false, true] {
        let mut w = world();
        let f = w.create_fork(TreeId::ROOT);
        let (t, old, editor) = if in_fork {
            (f, Client::Fork(f), Client::Fork(f))
        } else {
            (TreeId::ROOT, Client::Root("m2".into()), m1())
        };
        let h = w.open_write(&old, "a");
        w.write(h, Write::Append("old\n".into()));
        let new = atomic_save(&mut w, &editor, "a", "a.tmp", LineEdit::At(0), "x");
        assert_eq!(w.read(t, "a").bytes(), new);
        let g = w.create_fork(t);
        w.save(&Client::Fork(g), "a", 3);
        take(&mut w, &Step::Next(t));
        assert!(!w.waits().contains_key(&t), "the landing does not wait");
        w.write(
            h,
            Write::Pwrite {
                offset: 0,
                bytes: "zz".into(),
            },
        );
        let (head, now) = (w.head(t), w.read(t, "a").bytes());
        assert!(now.starts_with("x\n") && !now.contains("old"), "{now:?}");
        w.close(h);
        ok(&mut w);
        assert_eq!(w.head(t), head, "the orphan saved nothing");
        assert_eq!(
            w.read(t, "a").bytes(),
            now,
            "its writes never reached the replacement"
        );
        let _ = new;
        let e = w.orphans().last().expect("an OrphanDropped event");
        assert_eq!((e.handle, e.path.as_str(), e.bytes), (h, "a", 6),);
        assert_eq!(
            e.writer,
            if in_fork {
                format!("c{f}")
            } else {
                "m2".into()
            }
        );
        drain(&mut w);
        let a = w.read(TreeId::ROOT, "a");
        assert_eq!(a.texts()[0], "x", "{in_fork}");
        assert!(!a.bytes().contains("old") && !a.bytes().contains("zz"));
        assert_eq!(a.texts().len(), 6, "the landing's line 3 merged per line");
        assert!(w.collisions.is_empty(), "{:?}", w.collisions);
    }
}

/// Rule 11 with objects: an open of the new file still open at the atomic
/// save follows its object, now the replaced file's: its later writes and
/// its close save into that file.
#[test]
fn r11_an_open_on_the_new_file_follows_it_through_the_atomic_save() {
    let mut w = world();
    let h = w.create(&m1(), "a.tmp").unwrap();
    w.write(h, Write::Append("x\n".into()));
    w.rename(&m1(), "a.tmp", "a");
    ok(&mut w);
    assert_eq!(
        w.read(TreeId::ROOT, "a").bytes(),
        "",
        "the new file was empty at the rename"
    );
    w.write(h, Write::Append("y\n".into()));
    w.close(h);
    ok(&mut w);
    let a = w.read(TreeId::ROOT, "a");
    assert_eq!((a.name.line.as_str(), a.bytes().as_str()), ("a", "x\ny\n"),);
    assert!(w.orphans().is_empty());
    drain(&mut w);
}

// ── rule 12: every write call is recorded before it returns (git-model.md §3)

/// Rule 12: a crash of every agentfs process saves each open from the
/// journal as its close would have, so every write call that returned
/// survives, each save naming its open (`Agentfs-Open`).
#[test]
fn r12_a_crash_saves_every_open_from_the_journal() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let h = w.open_write(&m1(), "a");
    w.write(h, Write::Append("x\n".into()));
    let k = w.open_write(&Client::Fork(f), "b");
    w.write(
        k,
        Write::Pwrite {
            offset: 0,
            bytes: "y".into(),
        },
    );
    let g = w.create_fork(f);
    w.save(&Client::Fork(g), "b", 1);
    take(&mut w, &Step::Next(f));
    assert!(w.waits().contains_key(&f), "the landing waits for k");
    w.crash();
    ok(&mut w);
    assert_eq!(w.handles().count(), 0, "every open ended with the mount");
    assert!(w.read(TreeId::ROOT, "a").bytes().ends_with("-\nx\n"),);
    assert!(w.read(f, "b").bytes().starts_with("y\n"),);
    assert!(w.saved(h) && w.saved(k));
    assert!(
        w.waits().is_empty(),
        "waits are found again at the next take"
    );
    drain(&mut w);
    assert!(w.read(TreeId::ROOT, "a").bytes().ends_with("-\nx\n"));
}

/// Rule 12: a crash after a close's save but before its journal records
/// are dropped: the replay finds the save naming the open and does not
/// save it again. A take is one step (finding 49), so the take before it
/// wrote whole and stays.
#[test]
fn r12_a_crash_during_a_close_saves_the_open_once() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    w.save(&Client::Fork(f), "b", 0);
    take(&mut w, &Step::Next(TreeId::ROOT));
    assert!(w.last_take().is_some());
    let h = w.open_write(&m1(), "a");
    w.write(h, Write::Append("x\n".into()));
    let head = w.head(TreeId::ROOT);
    w.close_then_crash(h);
    ok(&mut w);
    let saves = git(
        w.repo(),
        &["rev-list", "--all", &format!("--grep=^Agentfs-Open: {h}$")],
    );
    assert_eq!(saves.lines().count(), 1, "saved once");
    assert_eq!(
        git(
            w.repo(),
            &["rev-parse", &format!("{}^1", w.head(TreeId::ROOT))]
        ),
        head
    );
    assert_eq!(w.queue_len(TreeId::ROOT), 0, "the take stays taken");
    drain(&mut w);
    assert_eq!(w.read(TreeId::ROOT, "a").bytes().matches("x\n").count(), 1);
    assert_eq!(w.read(TreeId::ROOT, "b"), w.read(f, "b"));
}

/// The calls of finding 26: a nested
/// fork f3 deletes line 3 of `a`; its upstream f2 deletes line 3 too and
/// edits line 0, which the root edits concurrently, so f2's save loses whole at
/// the root. Then f2 takes f3's save: its delete agrees with f2's, so it
/// applies there, an ordinary version of f3's change (the owner,
/// 2026-11-05, rejecting R3e.3). Returns the root's and f3's lines once drained.
fn nested_agreeing_delete() -> (Vec<String>, Vec<String>) {
    let mut w = world();
    let d = w.open_write(&m1(), "a");
    for i in 0..6 {
        w.edit(d, LineEdit::At(i));
    }
    w.close(d);
    let f2 = w.create_fork(TreeId::ROOT);
    let f3 = w.create_fork(f2);
    let line3 = w.read(TreeId::ROOT, "a").lines[3].line.clone();
    let version = |w: &World, t: TreeId| w.read(t, "a").member(&line3).map(|l| l.id.clone());
    call(&mut w, &Client::Fork(f3), "a", LineEdit::Delete(3));
    let mine = version(&w, f3).expect("f3's delete");
    let h = w.open_write(&Client::Fork(f2), "a");
    w.edit(h, LineEdit::Delete(3));
    w.edit(h, LineEdit::At(0));
    w.close(h);
    w.save(&m1(), "a", 0);
    next(&mut w, TreeId::ROOT);
    assert!(
        w.collisions.iter().any(|c| c.by == By::Whole),
        "f2's save lost whole at the root"
    );
    next(&mut w, f2);
    assert_eq!(
        version(&w, f2),
        Some(mine.clone()),
        "f3's delete applied in f2"
    );
    // f2 syncs the root (its own delete lost there) before f3 syncs: f2 keeps
    // f3's delete, which the root's queue has not decided yet (rule 3d).
    while w.pending().contains(&Step::Next(f2)) {
        next(&mut w, f2);
    }
    assert_eq!(w.read(f2, "a").texts().len(), 5);
    assert_eq!(w.read(f3, "a").texts().len(), 5);
    next(&mut w, f3);
    drain(&mut w);
    assert_eq!(
        version(&w, TreeId::ROOT),
        Some(mine),
        "f3's delete carried upward and applied in the root"
    );
    (w.read(TreeId::ROOT, "a").texts(), w.read(f3, "a").texts())
}

/// Rule 3e with the agreeing delete an ordinary change (the owner,
/// 2026-11-05: "(a) agree -- why would it be dropped?"): a nested
/// fork's delete that agrees with its upstream's applies there as part of
/// the change that won, is carried upward with the upstream's landing, and is
/// judged again in the root, where it was written over the root's line: it applies,
/// though the upstream's own delete lost there. Finding 26's calls.
#[test]
fn r3_a_nested_forks_agreeing_delete_is_carried_upward() {
    let (root, f3) = nested_agreeing_delete();
    assert_eq!(root.len(), 5, "line 3 deleted in the root");
    assert_eq!(f3, root, "and in the nested fork");
}

/// R10.3, finding 28 (2026-11-05, found removing R3e.3): the root edits line 1, then deletes it; the
/// fork deletes line 1 concurrently. The two deletes agree: both are
/// recorded, no collision is reported, and the line stays deleted. The root's
/// edit is not lost: the root's own delete replaced it first. The checks
/// called it a loss until the owner's rule R10.3.
#[test]
fn r10_two_deletes_of_one_line_agree_over_an_unseen_edit() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    distinct(&mut w, "a");
    let line1 = w.read(TreeId::ROOT, "a").lines[1].line.clone();
    w.save(&m1(), "a", 1);
    call(&mut w, &m1(), "a", LineEdit::Delete(1));
    call(&mut w, &Client::Fork(f), "a", LineEdit::Delete(1));
    drain(&mut w);
    assert!(w.collisions.is_empty(), "{:?}", w.collisions);
    for t in [TreeId::ROOT, f] {
        let a = w.read(t, "a");
        let v = a.member(&line1).expect("line 1");
        assert!(!v.live(), "{t:?}: line 1 deleted, {v:?}");
    }
}

/// R10.3, finding 28: a fork's edit of line 1 loses to the root's delete, and
/// a later save deletes line 1 over that edit. That delete agrees with
/// the root's: it applies, the line stays deleted, and only the edit is named
/// lost.
#[test]
fn r10_a_later_delete_agrees_with_the_winning_delete() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    distinct(&mut w, "a");
    let line1 = w.read(TreeId::ROOT, "a").lines[1].line.clone();
    w.save(&Client::Fork(f), "a", 1);
    call(&mut w, &m1(), "a", LineEdit::Delete(1));
    next(&mut w, f);
    call(&mut w, &Client::Fork(f), "a", LineEdit::Delete(1));
    drain(&mut w);
    assert_eq!(
        w.collisions.len(),
        1,
        "only the edit lost: {:?}",
        w.collisions
    );
    for t in [TreeId::ROOT, f] {
        let a = w.read(t, "a");
        let v = a.member(&line1).expect("line 1");
        assert!(!v.live(), "{t:?}: line 1 deleted, {v:?}");
    }
}

/// Finding 25: a fork
/// renames `a` to `e` (save 1), then renames `c` over `e` (save 2, one call:
/// `c` named `e`, `a` replaced). The root edited `a` meanwhile, unseen by save 2,
/// so save 2's unlink of `a` loses to that line (the file rule) and save 2
/// loses whole: its rename of `c` lost with its change (`By::Whole`), not to
/// the name `a` keeps only because the change lost.
#[test]
fn r11_a_losing_rename_over_loses_its_rename_with_its_change() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let c = Client::Fork(f);
    w.rename(&c, "a", "e");
    w.save(&m1(), "a", 0);
    w.rename(&c, "c", "e");
    drain(&mut w);
    let named: Vec<_> = w.collisions.iter().filter(|x| x.path == "c").collect();
    assert!(
        !named.is_empty() && named.iter().all(|x| x.by == By::Whole),
        "{named:?}"
    );
    for t in [TreeId::ROOT, f] {
        assert_eq!(w.path_of(t, "a").as_deref(), Some("e"), "{t:?}");
        assert_eq!(w.path_of(t, "c").as_deref(), Some("c"), "{t:?}");
    }
}

/// Rule 3e with the agreeing delete an ordinary change (the owner,
/// 2026-11-05, rejecting R3e.3), finding 19's calls: a transaction deletes
/// a line the fork's own save deletes too. At commit the two deletes
/// agree, so the transaction's delete applies in the fork, part of the
/// change that won (`Committed`). The fork's save then loses whole at
/// the root (another of its lines collided); the transaction's commit, judged
/// on its own (rule 4), carries its delete upward, and it applies in the root.
/// When the transaction's change loses instead (another of its lines
/// collided with the fork's), its agreeing delete loses with it and the
/// answer names it.
#[test]
fn r3_a_transactions_agreeing_delete_is_part_of_its_change() {
    let mut w = world();
    // Distinct bytes, so the alignment's pick among identical lines
    // plays no part (rule 10).
    let d = w.open_write(&m1(), "a");
    for i in 0..6 {
        w.edit(d, LineEdit::At(i));
    }
    w.close(d);
    let f = w.create_fork(TreeId::ROOT);
    let c = Client::Fork(f);
    let line3 = w.read(TreeId::ROOT, "a").lines[3].line.clone();
    let version = |w: &World, t: TreeId| w.read(t, "a").member(&line3).map(|l| l.id.clone());
    let h = w.open_write(&c, "a");
    w.begin(f).unwrap();
    let k = w.open_write(&c, "a");
    w.edit(k, LineEdit::Delete(3));
    w.close(k);
    w.edit(h, LineEdit::Delete(3));
    w.edit(h, LineEdit::At(0));
    w.close(h);
    let theirs = version(&w, f).expect("the fork's delete");
    w.save(&m1(), "a", 0);
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
    let mine = version(&w, f).expect("the transaction's delete");
    assert_ne!(mine, theirs, "applied in the fork");
    drain(&mut w);
    assert_eq!(
        w.read(TreeId::ROOT, "a").texts().len(),
        5,
        "line 3 stays deleted"
    );
    assert_eq!(version(&w, TreeId::ROOT), Some(mine), "carried upward");

    // The loss: the transaction deletes line 3 and edits line 0; the
    // fork's save deletes line 3 and edits line 0 concurrently.
    let line3 = w.read(f, "a").get(3).expect("a line 3").line.clone();
    let h = w.open_write(&c, "a");
    w.begin(f).unwrap();
    // The fork saves before the transaction has the file (R5.3 would
    // defer it after).
    w.edit(h, LineEdit::Delete(3));
    w.edit(h, LineEdit::At(0));
    w.close(h);
    let k = w.open_write(&c, "a");
    w.edit(k, LineEdit::Delete(3));
    w.edit(k, LineEdit::At(0));
    w.close(k);
    let Ok(TxnOutcome::Lost { lines }) = w.commit_txn(f) else {
        panic!("the transaction lost");
    };
    let gone = lines
        .iter()
        .find(|o| o.line == line3)
        .map(|o| o.staged.clone())
        .unwrap_or_else(|| panic!("its agreeing delete is named: {lines:?}"));
    assert!(
        w.collisions
            .iter()
            .any(|x| x.by == By::Whole && x.lost.contains(&gone)),
        "lost with its change"
    );
    drain(&mut w);
}

// ── rule 9, progress (git-model.md §6: P1 and P2; P3 deleted) ───────────

/// Rule 9 with P1: a root writer holding a file open for long holds back
/// only the landings that touch that file. An unrelated fork's change
/// goes ahead of the waiting one; a later change of the waiting one's own
/// fork does not (each fork's order is kept), even of another file,
/// nor does another fork's change of the held file.
#[test]
fn r9_a_long_lived_root_writer_holds_back_only_its_own_file() {
    let mut w = world();
    let (f, g) = (w.create_fork(TreeId::ROOT), w.create_fork(TreeId::ROOT));
    let k = w.create_fork(TreeId::ROOT);
    let m2 = Client::Root("m2".into());
    let log = w.open_write(&m2, "a");
    w.write(log, Write::Append("log\n".into()));
    let x = w.save(&Client::Fork(f), "a", 0);
    let y = w.save(&Client::Fork(g), "b", 0);
    let z = w.save(&Client::Fork(f), "c", 0);
    let v = w.save(&Client::Fork(k), "a", 1);
    take(&mut w, &Step::Next(TreeId::ROOT));
    assert!(
        w.waits().contains_key(&TreeId::ROOT),
        "f's landing of a waits for the writer"
    );
    take(&mut w, &Step::Next(TreeId::ROOT));
    assert!(w.last_take().is_some(), "g's landing is taken");
    assert!(
        has(&w, TreeId::ROOT, "b", &y),
        "g's unrelated change went ahead"
    );
    assert!(
        !has(&w, TreeId::ROOT, "c", &z),
        "f's later save keeps its place behind its first"
    );
    assert!(
        !w.pending().contains(&Step::Next(TreeId::ROOT)),
        "nothing else is free to go: k's change shares the waiting one's file"
    );
    w.write(log, Write::Append("more\n".into()));
    w.close(log);
    ok(&mut w);
    drain(&mut w);
    assert!(has(&w, TreeId::ROOT, "a", &x) && has(&w, TreeId::ROOT, "c", &z),);
    assert!(has(&w, TreeId::ROOT, "a", &v));
    assert!(w.read(TreeId::ROOT, "a").bytes().ends_with("log\nmore\n"));
}

/// Rule 9 without P3 (the owner, 2026-11-04: "it seems low risk for them
/// to be indefinitely delayed while an application is keeping the fs busy
/// with real work"): once a take waits for a file, a new write-open or
/// `unlink` of it is still granted at once, so overlapping writers can
/// postpone the take for as long as they keep the file open, and the take
/// goes once the last one closes.
#[test]
fn r9_new_writers_postpone_a_waiting_take() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let m2 = Client::Root("m2".into());
    let first = w.open_write(&m1(), "a");
    let x = w.save(&Client::Fork(f), "a", 0);
    take(&mut w, &Step::Next(TreeId::ROOT));
    assert!(w.waits().contains_key(&TreeId::ROOT));
    let second = w.open_write(&m2, "a");
    assert!(
        w.handles().any(|h| h.id == second),
        "the new open is granted, not made to wait behind the take"
    );
    w.close(first);
    ok(&mut w);
    assert!(
        w.waits().contains_key(&TreeId::ROOT),
        "the second writer still has it open: the take still waits"
    );
    let third = w.open_write(&Client::Root("m3".into()), "a");
    assert!(w.handles().any(|h| h.id == third));
    w.close(second);
    ok(&mut w);
    assert!(w.waits().contains_key(&TreeId::ROOT));
    w.close(third);
    ok(&mut w);
    assert!(
        !w.waits().contains_key(&TreeId::ROOT),
        "the last close woke it"
    );
    next(&mut w, TreeId::ROOT);
    assert!(
        w.file(&w.head(TreeId::ROOT), "a").contains(&x),
        "the take went"
    );
    drain(&mut w);
}

/// Rule 9, finding 20: a program that holds open a file a waiting take
/// waits for is never made to wait behind that take, or each would wait
/// for the other (the program blocked in its open cannot close the file).
/// Without P3 no open waits behind a waiting take at all: another
/// client's open of the take's other file is granted too; the take goes
/// once both close.
#[test]
fn r9_a_writer_holding_a_waited_file_is_not_made_to_wait() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let c = Client::Fork(f);
    w.begin(f).unwrap();
    for p in ["a", "b"] {
        let h = w.open_write(&c, p);
        w.edit(h, LineEdit::At(0));
        w.close(h);
    }
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
    let a = w.open_write(&m1(), "a");
    take(&mut w, &Step::Next(TreeId::ROOT));
    assert!(
        w.waits().contains_key(&TreeId::ROOT),
        "the landing of a and b waits for a"
    );
    let b = w.open_write(&m1(), "b");
    assert!(
        w.handles().any(|h| h.id == b),
        "m1 holds a: not made to wait"
    );
    let other = w.open_write(&Client::Root("m2".into()), "b");
    assert!(
        w.handles().any(|h| h.id == other),
        "another client's open of b is granted too"
    );
    w.close(b);
    w.close(a);
    ok(&mut w);
    assert!(w.waits().contains_key(&TreeId::ROOT), "m2 still has b open");
    w.close(other);
    ok(&mut w);
    next(&mut w, TreeId::ROOT);
    drain(&mut w);
    assert_eq!(w.read(TreeId::ROOT, "a"), w.read(f, "a"));
}

/// Rule 12 inside a transaction (R5: "a failure is an implicit rollback"):
/// a crash rolls the open transaction back, so an open in it ends unsaved,
/// as at a rollback; nothing of it reaches any tree or any commit.
#[test]
fn r12_a_crash_rolls_back_an_open_transaction() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let start = w.head(f);
    w.begin(f).unwrap();
    let h = w.open_write(&Client::Fork(f), "a");
    w.write(h, Write::Append("txn\n".into()));
    w.crash();
    ok(&mut w);
    assert!(!w.saved(h), "the open is not saved");
    assert_eq!(w.handles().count(), 0);
    assert_eq!(w.head(f), start);
    assert_eq!(w.commit_txn(f), Err(ModelError::NoTransaction));
    drain(&mut w);
    assert!(!w.read(TreeId::ROOT, "a").bytes().ends_with("txn\n"));
}

/// Rule 9 with P1 (the review of 2026-11-04, P1-c): a long-held file holds
/// back only the saves that touch it. A fork saves `a`, then `b`, while a
/// root client holds `b` open; the save of `a` lands at once, and the save of
/// `b` waits on its own. And in the other order, a first save that waits
/// waits alone.
#[test]
fn r9_a_held_file_holds_back_only_its_saves() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let m2 = Client::Root("m2".into());
    let held = w.open_write(&m2, "b");
    let x = w.save(&Client::Fork(f), "a", 0);
    let y = w.save(&Client::Fork(f), "b", 0);
    take(&mut w, &Step::Next(TreeId::ROOT));
    assert!(
        w.last_take().is_some(),
        "the save of a is taken, not held back by b"
    );
    assert!(has(&w, TreeId::ROOT, "a", &x));
    assert!(!has(&w, TreeId::ROOT, "b", &y));
    take(&mut w, &Step::Next(TreeId::ROOT));
    assert!(
        w.waits().contains_key(&TreeId::ROOT),
        "the save of b waits for its writer"
    );
    w.close(held);
    ok(&mut w);
    drain(&mut w);
    assert!(has(&w, TreeId::ROOT, "b", &y));
    // The other order: the save of `a` waits for its writer, alone, so its
    // lock does not reach `b`, which only the save behind it touches: a new
    // open of `b` is granted.
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let held = w.open_write(&Client::Root("m2".into()), "a");
    w.save(&Client::Fork(f), "a", 0);
    w.save(&Client::Fork(f), "b", 0);
    take(&mut w, &Step::Next(TreeId::ROOT));
    assert!(w.waits().contains_key(&TreeId::ROOT));
    let other = w.open_write(&Client::Root("m3".into()), "b");
    assert!(
        w.handles().any(|h| h.id == other),
        "the waiting save of a does not lock b"
    );
    w.close(other);
    w.close(held);
    ok(&mut w);
    drain(&mut w);
}

/// Rule 9 without P3, finding 24 (the review of 2026-11-04, P0-1, decided
/// by the owner on 2026-11-04): cooperating programs never deadlock. m1
/// keeps `a` open, then starts m2 and waits for it (a child, or a pipe); m2
/// opens `a` for append. A fork's change to `a` waits for m1's close;
/// m2's open is granted, as Linux grants it (no open waits on a waiting
/// take), so m2 finishes, m1 closes, and the take goes. Under P3 m2 waited
/// behind the take, which waited for m1, which waited for m2.
#[test]
fn r9_cooperating_programs_never_deadlock() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let log = w.open_write(&m1(), "a");
    w.write(log, Write::Append("m1\n".into()));
    let x = w.save(&Client::Fork(f), "a", 0);
    take(&mut w, &Step::Next(TreeId::ROOT));
    assert!(
        w.waits().contains_key(&TreeId::ROOT),
        "the landing waits for m1"
    );
    let child = w.open_write(&Client::Root("m2".into()), "a");
    assert!(
        w.handles().any(|h| h.id == child),
        "m2's open is granted: nothing waits on m1's close but the take"
    );
    w.write(child, Write::Append("m2\n".into()));
    w.close(child);
    w.close(log);
    ok(&mut w);
    drain(&mut w);
    let a = w.read(TreeId::ROOT, "a");
    assert!(a.contains(&x), "the take went once both closed");
    assert!(a.bytes().contains("m2\n") && a.bytes().contains("m1\n"));
}

/// Progress rule P2, the prefix sync (the owner, 2026-11-04: "agreed"): a
/// sync delivers the upstream's commits in order up to the first that would
/// overwrite a file the fork has open for writing, and holds back that
/// one and every later one until the file closes. The root saves `b`, then
/// `a`, then `c` while the fork has `a` open: the sync delivers `b`
/// only (not `c`, which comes after the change to `a`), then waits; the
/// close delivers the rest.
#[test]
fn r9_a_sync_delivers_up_to_the_first_change_to_an_open_file() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let t = f;
    let h = w.open_write(&Client::Fork(f), "a");
    let mb = w.save(&m1(), "b", 0);
    let ma = w.save(&m1(), "a", 0);
    let mc = w.save(&m1(), "c", 0);
    take(&mut w, &Step::Next(t));
    assert!(
        w.last_take().is_some() && w.waits().get(&t).is_some_and(|m| m.len() == 1),
        "it delivers a prefix and waits for the rest"
    );
    assert!(
        has(&w, t, "b", &mb),
        "b, before the change to a, is delivered"
    );
    assert!(!has(&w, t, "a", &ma), "a is open: not overwritten");
    assert!(
        !has(&w, t, "c", &mc),
        "c comes after the change to a: held back too"
    );
    assert_eq!(w.prefix_syncs, 1);
    assert!(
        !w.pending().contains(&Step::Next(t)),
        "the rest waits for the close"
    );
    let x = w.edit(h, LineEdit::At(1)).unwrap();
    w.close(h);
    ok(&mut w);
    assert!(w.waits().get(&t).is_none(), "the close woke it");
    drain(&mut w);
    let a = w.read(t, "a");
    assert!(a.contains(&ma) && a.contains(&x));
    assert!(has(&w, t, "c", &mc));
}

/// Finding 11's rule on a chain (review of 2026-11-04): the fork renames
/// `b` to `h`; the root renames `c` to `h`, then `d` to `c`. The sync keeps the
/// fork's `h`, shows the root's c-file under its previous name `c`, and so
/// the root's d-file, now named `c` in the root, under its previous name `d`:
/// never two files at one path. The root's queue then decides and the fork
/// shows the root's names.
#[test]
fn r11_a_chain_of_renames_to_one_path_keeps_every_previous_name() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let t = f;
    w.rename(&Client::Fork(f), "b", "h");
    w.rename(&m1(), "c", "h");
    w.rename(&m1(), "d", "c");
    next(&mut w, t);
    let names = |w: &World, t| (w.path_of(t, "b"), w.path_of(t, "c"), w.path_of(t, "d"));
    assert_eq!(
        names(&w, t),
        (Some("h".into()), Some("c".into()), Some("d".into())),
        "its own name, the root's files under their previous names"
    );
    drain(&mut w);
    for t in [TreeId::ROOT, t] {
        assert_eq!(
            names(&w, t),
            (Some("b".into()), Some("h".into()), Some("c".into())),
            "{t:?}"
        );
    }
}

/// P2, causal order (review of 2026-11-04, P0-1): a prefix sync delivers no
/// upstream decision before the commit that carries it. G is a fork in
/// P and keeps `b` open; G's x and the root's m both write over line 0 of `a`.
/// P saves `c`, then `b`, then takes G's x; the root takes P's three commits
/// and x loses to m. G's sync of P delivers only `c` (P's `b` is open in G)
/// and holds back the landing that carried x up, so the root's decision
/// against x is not delivered yet: G keeps x, never the version both were
/// written over. Then `b` closes and G shows m.
#[test]
fn r9_a_prefix_sync_delivers_no_upstream_decision_before_its_commit() {
    let mut w = world();
    let p = w.create_fork(TreeId::ROOT);
    let g = w.create_fork(p);
    let (tp, tg) = (p, g);
    let hb = w.open_write(&Client::Fork(g), "b");
    let x = w.save(&Client::Fork(g), "a", 0);
    let m = w.save(&m1(), "a", 0);
    w.save(&Client::Fork(p), "c", 0);
    w.save(&Client::Fork(p), "b", 0);
    while !has(&w, tp, "a", &x) {
        assert!(w.pending().contains(&Step::Next(tp)), "P can take");
        next(&mut w, tp);
    }
    while w.pending().contains(&Step::Next(TreeId::ROOT)) {
        next(&mut w, TreeId::ROOT);
    }
    assert!(has(&w, TreeId::ROOT, "a", &m) && !has(&w, TreeId::ROOT, "a", &x));
    take(&mut w, &Step::Next(tg));
    assert!(w.prefix_syncs > 0, "a prefix sync");
    assert!(has(&w, tg, "a", &x), "G keeps x until the decision arrives");
    w.close(hb);
    drain(&mut w);
    assert!(has(&w, tg, "a", &m));
}

/// P2, a held-back commit's collision records are held back with it (review
/// of 2026-11-04, P1-2): G is a fork in P and keeps `b` open; G's x and
/// P's p both write over line 0 of `a`. P then saves `c` and `b`, and takes
/// G's x, which loses to p. G's sync of P delivers p and `c` and holds back
/// `b` and the landing: that landing's decision against x is not delivered,
/// so G keeps x over p. Then `b` closes and G shows p.
#[test]
fn r9_a_prefix_sync_holds_back_the_decisions_of_the_commits_it_holds_back() {
    let mut w = world();
    let p = w.create_fork(TreeId::ROOT);
    let g = w.create_fork(p);
    let (tp, tg) = (p, g);
    let hb = w.open_write(&Client::Fork(g), "b");
    let x = w.save(&Client::Fork(g), "a", 0);
    let pa = w.save(&Client::Fork(p), "a", 0);
    w.save(&Client::Fork(p), "c", 0);
    w.save(&Client::Fork(p), "b", 0);
    while w.pending().contains(&Step::Next(tp)) {
        next(&mut w, tp);
    }
    assert!(
        has(&w, tp, "a", &pa) && !has(&w, tp, "a", &x),
        "x lost in P"
    );
    take(&mut w, &Step::Next(tg));
    assert!(w.prefix_syncs > 0, "a prefix sync");
    assert!(has(&w, tg, "a", &x), "G keeps x until the decision arrives");
    w.close(hb);
    drain(&mut w);
    assert!(has(&w, tg, "a", &pa));
}

/// P2, each change by its unit: a transaction's commit is one change, so
/// a sync holds back the whole of it when it touches an open file, even its
/// files nobody has open; and a commit that leaves the fork's open file
/// as the fork has it (its own save, come back from the upstream) touches
/// nothing and does not stop the sync.
#[test]
fn r9_a_prefix_sync_holds_back_a_transaction_whole() {
    let mut w = world();
    let (f, g) = (w.create_fork(TreeId::ROOT), w.create_fork(TreeId::ROOT));
    let t = f;
    // f's own save of a lands in the root; f opens a again.
    let own = w.save(&Client::Fork(f), "a", 0);
    next(&mut w, TreeId::ROOT);
    let h = w.open_write(&Client::Fork(f), "a");
    let mb = w.save(&m1(), "b", 0);
    // g's transaction writes c and a, and lands in the root whole.
    w.begin(g).unwrap();
    let mut staged = Vec::new();
    for p in ["c", "a"] {
        let x = w.open_write(&Client::Fork(g), p);
        staged.push(w.edit(x, LineEdit::At(2)).unwrap());
        w.close(x);
    }
    let gc = staged[0].clone();
    assert_eq!(w.commit_txn(g), Ok(TxnOutcome::Committed));
    while w.queue_len(TreeId::ROOT) > 0 {
        next(&mut w, TreeId::ROOT);
    }
    assert!(has(&w, TreeId::ROOT, "c", &gc));
    // f's queue: syncs of the root's landing of f's own save, of b, and of g's
    // transaction. The first two deliver; the transaction waits whole.
    while w.pending().contains(&Step::Next(t)) {
        take(&mut w, &Step::Next(t));
    }
    assert!(has(&w, t, "a", &own) && has(&w, t, "b", &mb));
    assert!(
        !has(&w, t, "c", &gc),
        "the transaction touches a, open: its c is held back with it"
    );
    w.close(h);
    ok(&mut w);
    drain(&mut w);
    assert!(has(&w, t, "c", &gc));
}

/// Finding 50 (seed 401 of the random runs at 500 seeds, 2026-11-08; closed by the
/// owner's option A the same day): f2 is a fork of f1, made while f1's save x of `b` is
/// undecided. f1's upstream's save m of `b` wins against x, and f1, having no other
/// commit, then syncs by fast-forwarding to that landing, so f1's first parents are its
/// upstream's. f2 keeps `b` open, so its prefix sync (R9.3) delivers only the upstream's
/// save of `c`, and holds back m and the decision against x (R9.9): f2 keeps x, with `b`
/// and its decision waiting. Once `b` closes f2 has received every line it was owed.
#[test]
fn finding_50_a_prefix_sync_holds_back_a_decision_on_the_upstreams_line_until_the_close() {
    let mut w = world();
    let f1 = w.create_fork(TreeId::ROOT);
    let x = w.save(&Client::Fork(f1), "b", 0);
    let c = w.save(&m1(), "c", 0);
    let m = w.save(&m1(), "b", 0);
    let f2 = w.create_fork(f1);
    let t2 = f2;
    let held = w.open_write(&Client::Fork(f2), "b");
    next(&mut w, TreeId::ROOT);
    assert!(has(&w, TreeId::ROOT, "b", &m) && !has(&w, TreeId::ROOT, "b", &x));
    next(&mut w, f1);
    assert_eq!(
        w.head(f1),
        w.head(TreeId::ROOT),
        "f1 fast-forwards to its upstream"
    );
    take(&mut w, &Step::Next(t2));
    assert!(w.prefix_syncs > 0, "a prefix sync");
    assert!(has(&w, t2, "c", &c), "f2 gets the unlocked c");
    assert!(
        has(&w, t2, "b", &x) && !has(&w, t2, "b", &m),
        "b and its decision wait"
    );
    w.close(held);
    drain(&mut w);
    assert!(
        has(&w, t2, "b", &m) && !has(&w, t2, "b", &x),
        "the decision arrives when b closes"
    );
    for p in ["b", "c"] {
        assert_eq!(
            w.read(t2, p),
            w.read(f1, p),
            "f2 has every line of {p} it was owed"
        );
    }
}

/// Finding 50's excuse needs the lock: the same prefix sync, judged only after `b` has
/// closed. No file holds it back any more, so `syncs_deliver` refuses f2's x, on the
/// real commits of that sync.
#[test]
fn finding_50_a_held_back_decision_is_excused_only_while_a_lock_holds_it() {
    let mut w = world();
    let f1 = w.create_fork(TreeId::ROOT);
    let x = w.save(&Client::Fork(f1), "b", 0);
    w.save(&m1(), "c", 0);
    w.save(&m1(), "b", 0);
    let f2 = w.create_fork(f1);
    let held = w.open_write(&Client::Fork(f2), "b");
    next(&mut w, TreeId::ROOT);
    next(&mut w, f1);
    w.step(&Step::Next(f2));
    assert!(w.prefix_syncs > 0, "a prefix sync");
    w.close(held);
    let e = w.check().expect_err("no lock holds that sync back now");
    assert!(e.contains(&format!("at {x}, which")), "{e}");
}

/// The root's save of a new line to `c` (at its third byte), as finding 47's seed made it.
fn root_edits_c(w: &mut World) {
    calls(
        w,
        &m1(),
        "c",
        &[Write::Pwrite {
            offset: 2,
            bytes: "m\n".into(),
        }],
    );
}

/// Finding 47, seed 484 (R10.3): the fork's unlink agrees with the root's unlink of the same
/// file, though the root edited the file first and the fork never saw that edit. The landing
/// takes, with no collision. The calls of the conformance replay's test of the same name.
#[test]
fn finding_47_two_unlinks_agree_over_an_unseen_edit() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    root_edits_c(&mut w);
    w.unlink(&m1(), "c");
    w.unlink(&Client::Fork(f), "c");
    ok(&mut w);
    next(&mut w, TreeId::ROOT);
    assert!(w.last_take().is_some(), "the root takes the fork's unlink");
    assert!(w.collisions.is_empty(), "{:?}", w.collisions);
    drain(&mut w);
}

/// Finding 47, seed 484, as the seed had it: the fork's delete is half of a rename-over,
/// which lands whole, so `b` takes the name `c` in the root.
#[test]
fn finding_47_a_rename_over_agrees_with_an_unlink_over_an_unseen_edit() {
    use agentfs_model::calls::{TreeCalls, TreeId};
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    root_edits_c(&mut w);
    w.unlink(&m1(), "c");
    w.rename(&Client::Fork(f), "b", "c");
    ok(&mut w);
    next(&mut w, TreeId::ROOT);
    assert!(
        w.last_take().is_some(),
        "the root takes the fork's rename-over"
    );
    assert!(w.collisions.is_empty(), "{:?}", w.collisions);
    let paths = TreeCalls::paths(&**w, TreeId::ROOT).unwrap();
    assert!(
        paths.contains(&"c".to_string()) && !paths.contains(&"b".to_string()),
        "{paths:?}"
    );
    drain(&mut w);
}

/// Finding 46, seed 124 (R9.1): a sync that changes no line or name of the fork's own view
/// of a file locks no name of it. The fork replaced `d` with `a` by a rename-over,
/// undecided, so the root's edit of the old `d` keeps no name there, and the fork's open of
/// the new `d` does not hold the sync back: it takes at once.
#[test]
fn finding_46_a_sync_of_a_file_the_fork_replaced_passes_the_name_it_took() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let s = Client::Fork(f);
    w.rename(&s, "a", "d");
    calls(
        &mut w,
        &m1(),
        "d",
        &[Write::Pwrite {
            offset: 2,
            bytes: "m\n".into(),
        }],
    );
    let held = w.open_write(&s, "d");
    ok(&mut w);
    next(&mut w, f);
    assert!(w.last_take().is_some(), "the sync takes past the open d");
    w.close(held);
    drain(&mut w);
}
