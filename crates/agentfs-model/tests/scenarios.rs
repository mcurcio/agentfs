//! One or more scenarios per rule of MODEL.md, named `r<N>_...` after the
//! rule they show. Every scenario checks every invariant after each step.
//! A test marked `#[ignore = "coherence finding N: ..."]` asserts what the
//! rules intend but do not deliver; run it with `--ignored`.

use agentfs_model::check::{Checks, check, check_converged};
use agentfs_model::git::git;
use agentfs_model::world::{
    Client, CollisionScope, Direction, HistoryPolicy, Life, ModelError, Policies, Step, SyncBase,
    Tree, TxnOutcome, World,
};

const BOTH: [HistoryPolicy; 2] = [HistoryPolicy::FullLineage, HistoryPolicy::Squashed];

fn world(history: HistoryPolicy, collision: CollisionScope) -> World {
    World::new(Policies::new(history, collision))
}

fn ok(w: &mut World) {
    if let Err(e) = check(w, Checks::default()) {
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
        let mut w = world(history, CollisionScope::Hunk);
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
    let mut w = world(HistoryPolicy::Squashed, CollisionScope::Hunk);
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
/// history, commit for commit (git's own dates are pinned in `git.rs`);
/// and the model's source names no clock.
#[test]
fn r2_no_clock_same_calls_same_history() {
    let run = || {
        let mut w = world(HistoryPolicy::FullLineage, CollisionScope::Hunk);
        let f = w.create_fork(Tree::Main);
        w.save(&Client::Fork(f), "a", 0);
        w.save(&m1(), "a", 0);
        w.begin(f).unwrap();
        w.save(&Client::Fork(f), "b", 1);
        w.commit_txn(f).unwrap();
        drain(&mut w);
        git(
            w.repo(),
            &["for-each-ref", "--format=%(objectname) %(refname)"],
        )
    };
    assert_eq!(run(), run());
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

// ── rule 3: fast-forward, else line merge, upstream wins ────────────────

/// Rule 3: an edit whose base is main's current version replaces it.
/// `FullLineage`: main's head becomes the fork's commit. `Squashed`: one
/// single-parent commit with that commit's tree, named for the sandbox.
#[test]
fn r3_fast_forward_when_the_base_is_current() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Hunk);
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
                assert_eq!(show(&w, &w.main_head, "%P").split(' ').count(), 1);
                assert_eq!(main_writers(&w)[0], "c1");
            }
        }
        drain(&mut w);
    }
}

/// Rule 3, fork into main: a concurrent same-line edit loses to main's;
/// the loss is recorded as a collision naming main's commit as winner,
/// and the losing commit stays reachable. The fork then takes main's line.
#[test]
fn r3_collision_on_landing_upstream_wins() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Hunk);
        let f = w.create_fork(Tree::Main);
        let x = w.save(&Client::Fork(f), "a", 0);
        let fc = w.head(Tree::Fork(f));
        let m = w.save(&m1(), "a", 0);
        next(&mut w, Tree::Main);
        assert!(has(&w, Tree::Main, "a", &m) && !has(&w, Tree::Main, "a", &x));
        let c = w.collisions.last().expect("a collision").clone();
        assert_eq!(
            (c.tree, c.direction, c.lost.clone()),
            (Tree::Main, Direction::Landing, vec![x.clone()])
        );
        assert_eq!(git(w.repo(), &["rev-parse", &c.reference]), fc);
        drain(&mut w);
        assert!(has(&w, Tree::Fork(f), "a", &m), "{history:?}");
    }
}

/// Rule 3, main into a fork: the sync reaches the fork before its own
/// commit lands; main's line wins there too (flagged in the fork), and the
/// later landing loses again in main.
#[test]
fn r3_collision_on_sync_upstream_wins() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Hunk);
        let f = w.create_fork(Tree::Main);
        let x = w.save(&Client::Fork(f), "a", 0);
        let m = w.save(&m1(), "a", 0);
        next(&mut w, Tree::Fork(f));
        assert!(has(&w, Tree::Fork(f), "a", &m) && !has(&w, Tree::Fork(f), "a", &x));
        let c = w.collisions.last().expect("a collision").clone();
        assert_eq!(
            (c.tree, c.direction, c.lost),
            (Tree::Fork(f), Direction::Sync, vec![x.clone()])
        );
        let head = w.head(Tree::Fork(f));
        assert_eq!(
            show(&w, &head, "%(trailers:key=Agentfs-Cause,valueonly)"),
            "Sync"
        );
        drain(&mut w);
        assert!(has(&w, Tree::Main, "a", &m) && !has(&w, Tree::Main, "a", &x));
        assert!(
            w.collisions
                .iter()
                .any(|c| c.tree == Tree::Main && c.lost == [x.clone()])
        );
    }
}

/// Rule 3's design choice, per hunk or per file. One fork commit (a
/// transaction's) changes slots 0 and 2 of a; main changed slot 0. Per
/// hunk, slot 2 still lands; per file, main keeps its whole file and the
/// clean slot 2 is lost too (recorded). Both converge.
#[test]
fn r3_collision_scope_per_hunk_keeps_clean_hunks() {
    for scope in [CollisionScope::Hunk, CollisionScope::File] {
        for history in BOTH {
            let mut w = world(history, scope);
            let f = w.create_fork(Tree::Main);
            w.begin(f).unwrap();
            let x = w.save(&Client::Fork(f), "a", 0);
            let y = w.save(&Client::Fork(f), "a", 2);
            assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
            let m = w.save(&m1(), "a", 0);
            next(&mut w, Tree::Main);
            drain(&mut w);
            assert!(has(&w, Tree::Main, "a", &m) && !has(&w, Tree::Main, "a", &x));
            let lost: Vec<String> = w.collisions.iter().flat_map(|c| c.lost.clone()).collect();
            match scope {
                CollisionScope::Hunk => {
                    assert!(has(&w, Tree::Main, "a", &y), "{history:?}");
                    assert!(!lost.contains(&y));
                }
                CollisionScope::File => {
                    assert!(!has(&w, Tree::Main, "a", &y), "{history:?}");
                    assert!(lost.contains(&y));
                }
            }
        }
    }
}

/// Rule 3's design choice, per hunk, in the sync direction: two fork
/// commits (slot 0, then slot 2); main's sync collides on slot 0 only, so
/// the fork keeps its slot 2 throughout, and everything converges.
#[test]
fn r3_collision_scope_per_hunk_sync_keeps_the_clean_hunk() {
    let mut w = world(HistoryPolicy::FullLineage, CollisionScope::Hunk);
    let f = w.create_fork(Tree::Main);
    w.save(&Client::Fork(f), "a", 0);
    let y = w.save(&Client::Fork(f), "a", 2);
    w.save(&m1(), "a", 0);
    next(&mut w, Tree::Fork(f));
    assert!(has(&w, Tree::Fork(f), "a", &y));
    drain(&mut w);
    assert!(has(&w, Tree::Fork(f), "a", &y) && has(&w, Tree::Main, "a", &y));
}

/// Coherence finding 4: per file breaks convergence. The same calls as
/// above under `File`: main's sync takes the fork's whole file, so the
/// fork loses its clean slot 2 (recorded); the fork's second commit still
/// lands slot 2 cleanly in main; but the sync's merge commit records the
/// fork's file without slot 2 on top of the commit that wrote it, so the
/// next merge reads that as the fork deleting slot 2 on purpose and keeps
/// it. Main has slot 2, the fork never gets it back, no queue holds
/// anything.
#[test]
#[ignore = "coherence finding 4: per-file collisions do not converge"]
fn finding_4_per_file_sync_converges() {
    let mut w = world(HistoryPolicy::FullLineage, CollisionScope::File);
    let f = w.create_fork(Tree::Main);
    w.save(&Client::Fork(f), "a", 0);
    let y = w.save(&Client::Fork(f), "a", 2);
    w.save(&m1(), "a", 0);
    next(&mut w, Tree::Fork(f));
    assert!(
        !has(&w, Tree::Fork(f), "a", &y),
        "per file: the sync took it"
    );
    drain(&mut w);
}

/// Rule 3 with crossing merges (`FullLineage`): the fork syncs main's
/// colliding line while main lands the fork's losing commit, so the two
/// merges cross. The fork then writes over main's line. The next sync
/// applies only main's commits the fork lacks, each on its own base, and
/// skips main's landing of the fork's own commit: the fork's newer line
/// (it saw main's) stays, and lands.
#[test]
fn r3_crossing_merges_keep_a_newer_edit() {
    let (w, y) = crossing(SyncBase::EachEdit);
    assert!(has(&w, Tree::Main, "a", &y));
}

/// Coherence finding 2: the same calls with git's own recursive merge base.
/// git merges the two bases leaving conflict markers in the base, so the
/// fork's newer line (written over main's) "collides" with main's and
/// loses to it: a recorded collision between edits that saw each other.
#[test]
#[ignore = "coherence finding 2: git's recursive merge base of crossing syncs and landings manufactures collisions"]
fn finding_2_git_merge_base_on_crossing_merges() {
    crossing(SyncBase::Git);
}

fn crossing(sync_base: SyncBase) -> (World, String) {
    let mut w = World::new(Policies {
        sync_base,
        ..Policies::new(HistoryPolicy::FullLineage, CollisionScope::Hunk)
    });
    let f = w.create_fork(Tree::Main);
    w.save(&Client::Fork(f), "a", 0);
    let m = w.save(&m1(), "a", 0);
    next(&mut w, Tree::Fork(f)); // sync: main's line wins in F
    next(&mut w, Tree::Main); // landing: main's line wins in main; crossed
    assert!(has(&w, Tree::Fork(f), "a", &m));
    let y = w.save(&Client::Fork(f), "a", 0); // written over main's line
    next(&mut w, Tree::Fork(f));
    assert!(
        has(&w, Tree::Fork(f), "a", &y),
        "{sync_base:?}: the newer line stays"
    );
    drain(&mut w);
    (w, y)
}

/// Coherence finding 3: under `Squashed`, main's commit carrying the
/// fork's landed line does not name the fork's commit, so the next sync's
/// merge base predates it: the fork's own older line comes back as a
/// "collision" with its newer one and wins (upstream). The newer line
/// lands later and syncs back, so the trees converge; the fork's program
/// saw its newer write undone meanwhile, and a collision was recorded
/// between two edits of one writer.
#[test]
#[ignore = "coherence finding 3: Squashed (rule 7) loses the ancestry rule 2 needs: a fork's own landed edit beats its newer one"]
fn finding_3_squashed_sync_never_undoes_the_forks_newer_line() {
    let mut w = world(HistoryPolicy::Squashed, CollisionScope::Hunk);
    let f = w.create_fork(Tree::Main);
    w.save(&Client::Fork(f), "a", 0);
    next(&mut w, Tree::Main); // the fork's x lands
    let y = w.save(&Client::Fork(f), "a", 0); // y over x
    next(&mut w, Tree::Fork(f)); // main's commit carrying x syncs back
    assert!(
        has(&w, Tree::Fork(f), "a", &y),
        "the fork's newer line was undone"
    );
}

// ── rule 4: nothing waits; nested forks ─────────────────────────────────

/// Rule 4: a commit that collides does not hold back the next one; every
/// step takes its queue's head; the queues drain.
#[test]
fn r4_nothing_waits_behind_a_collision() {
    let mut w = world(HistoryPolicy::FullLineage, CollisionScope::Hunk);
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

/// Rule 4, nested forks: G over F over main. In F, F's line wins over G's;
/// in main, main's wins over F's (carrying G's). A clean change of G's
/// reaches main named for G. Everything converges.
#[test]
fn r4_nested_forks_follow_the_same_rules() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Hunk);
        let f = w.create_fork(Tree::Main);
        let g = w.create_fork(Tree::Fork(f));
        let (cf, cg) = (Client::Fork(f), Client::Fork(g));
        let ga = w.save(&cg, "a", 0);
        let fa = w.save(&cf, "a", 0);
        next(&mut w, Tree::Fork(f)); // G's a into F: F wins.
        assert!(has(&w, Tree::Fork(f), "a", &fa) && !has(&w, Tree::Fork(f), "a", &ga));
        assert!(
            w.collisions
                .iter()
                .any(|c| c.tree == Tree::Fork(f) && c.lost == [ga.clone()])
        );
        let gb = w.save(&cg, "b", 0);
        let mb = w.save(&m1(), "b", 0);
        let gc = w.save(&cg, "c", 0);
        drain(&mut w);
        assert!(has(&w, Tree::Main, "b", &mb) && !has(&w, Tree::Main, "b", &gb));
        assert!(has(&w, Tree::Main, "c", &gc) && has(&w, Tree::Main, "a", &fa));
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
        let mut w = world(history, CollisionScope::Hunk);
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

/// Rule 5, rejected: a sync landed a same-line change in a file the
/// transaction touched; the commit is rejected, the branch dropped, the
/// program reads the fork's head, and the staged edit is nowhere.
#[test]
fn r5_transaction_rejected_on_a_same_line_collision() {
    let mut w = world(HistoryPolicy::FullLineage, CollisionScope::Hunk);
    let f = w.create_fork(Tree::Main);
    let c = Client::Fork(f);
    w.begin(f).unwrap();
    let s = w.save(&c, "a", 0);
    w.save(&c, "b", 0);
    let m = w.save(&m1(), "a", 0);
    drain(&mut w);
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Rejected));
    ok(&mut w);
    assert!(w.client_reads(&c, "a").contains(&m) && !w.client_reads(&c, "a").contains(&s));
    assert!(w.discarded.contains(&s));
    let refs = git(
        w.repo(),
        &["for-each-ref", "--format=%(refname)", "refs/heads/txn"],
    );
    assert!(refs.is_empty());
    drain(&mut w);
}

/// Rule 5, rollback and client death: the branch is dropped, nothing of it
/// shows anywhere; the fork's own commits from before still land.
#[test]
fn r5_transaction_rollback_and_client_death_leave_nothing() {
    let mut w = world(HistoryPolicy::Squashed, CollisionScope::Hunk);
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

/// Coherence finding 1: a committed transaction is all-or-nothing only
/// against the fork's head at commit. Rule 3 (upstream wins per hunk) and
/// rule 4 (nothing waits) then apply to it like any commit: (a) a sync
/// already queued into the fork overwrites part of it in the fork; (b) a
/// concurrent main write overwrites part of it in main. The program was
/// told "committed"; main ends with half of it.
#[test]
#[ignore = "coherence finding 1: a committed transaction can land in main partly (rules 3-4 vs 5)"]
fn finding_1_a_committed_transaction_reaches_main_whole_or_not_at_all() {
    // (b): the commit is clean in the fork; main changes a[0] before the
    // landing.
    let mut w = world(HistoryPolicy::FullLineage, CollisionScope::Hunk);
    let f = w.create_fork(Tree::Main);
    w.begin(f).unwrap();
    let sa = w.save(&Client::Fork(f), "a", 0);
    let sb = w.save(&Client::Fork(f), "b", 0);
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
    w.save(&m1(), "a", 0);
    drain(&mut w);
    assert_eq!(
        has(&w, Tree::Main, "a", &sa),
        has(&w, Tree::Main, "b", &sb),
        "main holds half a transaction"
    );
}

/// Coherence finding 1 (a): the same in the fork itself, by a sync that
/// was queued before the commit.
#[test]
#[ignore = "coherence finding 1: a committed transaction can be partly overwritten in its own fork"]
fn finding_1_a_committed_transaction_stays_whole_in_its_fork() {
    let mut w = world(HistoryPolicy::FullLineage, CollisionScope::Hunk);
    let f = w.create_fork(Tree::Main);
    w.begin(f).unwrap();
    let sa = w.save(&Client::Fork(f), "a", 0);
    let sb = w.save(&Client::Fork(f), "b", 0);
    w.save(&m1(), "a", 0); // queued into F, not yet synced
    assert_eq!(w.commit_txn(f), Ok(TxnOutcome::Committed));
    next(&mut w, Tree::Fork(f));
    assert_eq!(
        has(&w, Tree::Fork(f), "a", &sa),
        has(&w, Tree::Fork(f), "b", &sb),
        "the fork holds half a committed transaction"
    );
}

// ── rule 6: attribution ─────────────────────────────────────────────────

/// Rule 6: a sync is a merge commit labelled `Sync` with the parent's
/// commit as second parent; it originates nothing (the invariant checks
/// every commit: one writer each; a non-merge commit carries only its
/// writer's edits; a merge adds only what a non-first parent holds).
#[test]
fn r6_a_sync_is_a_merge_with_the_parent_second() {
    let mut w = world(HistoryPolicy::Squashed, CollisionScope::Hunk);
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

/// Rule 7: `FullLineage` main reaches the fork's commits; `Squashed` holds
/// only main's own. Either way a collision's losing commit stays reachable
/// after its fork is dropped.
#[test]
fn r7_history_setting_and_losers_stay_reachable() {
    for history in BOTH {
        let mut w = world(history, CollisionScope::Hunk);
        let f = w.create_fork(Tree::Main);
        w.save(&Client::Fork(f), "a", 0);
        let fc = w.head(Tree::Fork(f));
        w.save(&m1(), "a", 0);
        w.end_client(f);
        drain(&mut w);
        assert_eq!(w.forks[&f].life, Life::Dropped);
        assert_eq!(
            w.is_ancestor(&fc, &w.main_head),
            history == HistoryPolicy::FullLineage
        );
        let c = &w.collisions[0];
        assert_eq!(
            git(w.repo(), &["rev-parse", &c.reference]),
            fc,
            "{history:?}"
        );
    }
}

// ── convergence ─────────────────────────────────────────────────────────

/// When writes stop and the queues drain, every fork equals main
/// (`check_converged`), here with siblings, a nested fork and collisions
/// in both directions.
#[test]
fn convergence_when_writes_stop() {
    for scope in [CollisionScope::Hunk, CollisionScope::File] {
        for history in BOTH {
            let mut w = world(history, scope);
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
}
