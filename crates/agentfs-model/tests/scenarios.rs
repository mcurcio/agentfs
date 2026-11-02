//! One scenario per documented flow and invariant of the target design
//! (docs/target-architecture/system.md), each naming its section or
//! invariant. Every scenario checks all global invariants after each step.
//!
//! Not modelled: `Lock` isolation (only `Merge`, the default), approval and
//! `WhenDetached`/`OnApply` timing (everything here is `Continuous`),
//! integrator commit sources, revert, reconcile of a dead client's unclosed
//! writes, database groups, deletes and renames, truncation of the journal
//! (bases never name a record, so it cannot matter: I11 is enforced by the
//! `Base` type), service restarts and recovery, space limits, views' caches
//! and consistency settings. `resolve` and restore are atomic (their own
//! `Writing` at once).
//!
//! Tests marked `#[ignore = "design finding N: ..."]` assert the intended
//! behaviour the docs do not deliver; run them with `--ignored`.

use agentfs_model::check::{Checks, check, check_quiescent};
use agentfs_model::world::{
    Base, Client, EndedForkCopies, HistoryPolicy, Life, Policies, SealAnswer, SealConflict, Step,
    Tree, TxnState, World,
};

fn world(history: HistoryPolicy, seal_conflict: SealConflict) -> World {
    World::new(Policies {
        history,
        seal_conflict,
        ..Policies::default()
    })
}

fn ok(w: &mut World) {
    if let Err(e) = check(w, Checks::default()) {
        panic!("{e}\nlog:\n{}", w.log.join("\n"));
    }
}

fn settle(w: &mut World) {
    w.settle();
    ok(w);
}

fn m1() -> Client {
    Client::Main("m1".into())
}

fn main_has(w: &World, p: &str, token: &str) -> bool {
    w.read(Tree::Main, p).contains(token)
}

/// The writer trailers of main's first-parent commits, newest first.
fn main_writers(w: &World) -> Vec<String> {
    agentfs_model::git::git(
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

/// §5 "A sandbox's commits land live in main, whole and in order", I2, I3:
/// two saves land as ONE main commit named for the sandbox, whose second
/// parent is the fork's commit (`FullLineage`).
#[test]
fn s5_sandbox_save_lands_in_main_as_one_commit() {
    let mut w = world(HistoryPolicy::FullLineage, SealConflict::Held);
    let f = w.create_fork(Tree::Main);
    let c = Client::Fork(f);
    let p = w.save(&c, "a", 0);
    let q = w.save(&c, "b", 1);
    ok(&mut w);
    assert!(!main_has(&w, "a", &p), "nothing lands before its step");
    settle(&mut w);
    assert!(main_has(&w, "a", &p) && main_has(&w, "b", &q));
    assert_eq!(main_writers(&w), ["c1", "agentfs"]);
    let parents = agentfs_model::git::git(w.repo(), &["rev-list", "--parents", "-n1", "main"]);
    assert!(parents.contains(&w.forks[&f].commits[1].git));
}

/// §5 "Main's change syncs into a fork's own copy" (R17, R18): a merge into
/// the fork's copy, one commit on its line named for main's writer with
/// main's commit as second parent; each side's change in the other's view.
/// The fork holds its copy with no run waiting (an open handle keeps it from
/// trimming); otherwise its earlier run lands first and main's landing
/// record joins the sync (finding 3).
#[test]
fn s5_main_change_syncs_into_fork_copy() {
    let mut w = world(HistoryPolicy::FullLineage, SealConflict::Held);
    let f = w.create_fork(Tree::Main);
    let c = Client::Fork(f);
    let h = w.open(&c, "a");
    let mine = w.save(&c, "a", 0);
    settle(&mut w);
    assert!(main_has(&w, "a", &mine) && w.forks[&f].own.contains("a"));
    let theirs = w.save(&m1(), "a", 1);
    settle(&mut w);
    assert!(
        !w.read(Tree::Fork(f), "a").contains(&theirs),
        "the sync waits for F's write-open of a"
    );
    let later = w.close(h, 2).unwrap();
    settle(&mut w);
    let seen = w.client_reads(&c, "a");
    assert!(seen.contains(&mine) && seen.contains(&theirs) && seen.contains(&later));
    assert!(main_has(&w, "a", &later) && main_has(&w, "a", &theirs));
    let line = agentfs_model::git::git(
        w.repo(),
        &[
            "log",
            "--format=%(trailers:key=Agentfs-Writer,valueonly,separator=%x2C)%x09%(trailers:key=Agentfs-Cause,valueonly,separator=%x2C)",
            &format!("fork/{f}"),
        ],
    );
    let sync = line
        .lines()
        .find(|l| l.ends_with("Landing Sync"))
        .expect("a sync commit on F's line");
    assert!(sync.starts_with("m1\t"), "named for main's writer: {sync}");
    check(&mut w, Checks { fork_lines: true }).unwrap();
}

/// §3.6, A8, I3: a true conflict holds the commit and every later one of
/// that fork; the fork's next commit re-judges; `withdraw` rejects the rest.
#[test]
fn a8_true_conflict_holds_and_later_commits_wait() {
    let mut w = world(HistoryPolicy::FullLineage, SealConflict::Held);
    let f = w.create_fork(Tree::Main);
    let c = Client::Fork(f);
    let held = w.save(&c, "a", 0);
    let main_side = w.save(&m1(), "a", 0);
    let later = w.save(&c, "b", 0);
    settle(&mut w);
    assert_eq!(w.forks[&f].landed, 0);
    assert!(w.forks[&f].held.is_some());
    assert!(main_has(&w, "a", &main_side) && !main_has(&w, "a", &held));
    assert!(!main_has(&w, "b", &later), "a later commit waits (A8)");
    let again = w.save(&c, "c", 0);
    ok(&mut w);
    assert!(
        w.pending().contains(&Step::Run(f)),
        "the next commit re-judges"
    );
    settle(&mut w);
    assert!(w.forks[&f].held.is_some() && !main_has(&w, "c", &again));
    w.withdraw(f);
    settle(&mut w);
    assert!(!main_has(&w, "c", &again));
    check_quiescent(&w).unwrap();
}

/// A12 (owner, 2026-11-02: "The sandboxes are generally peers and
/// parallelism is assumed"): a held commit holds back only its own fork.
#[test]
fn a12_sibling_forks_are_peers() {
    let mut w = world(HistoryPolicy::FullLineage, SealConflict::Held);
    let f = w.create_fork(Tree::Main);
    let g = w.create_fork(Tree::Main);
    w.save(&Client::Fork(f), "a", 0);
    w.save(&m1(), "a", 0);
    let fb = w.save(&Client::Fork(f), "b", 0);
    let ga = w.save(&Client::Fork(g), "a", 2);
    let gb = w.save(&Client::Fork(g), "b", 1);
    settle(&mut w);
    assert!(w.forks[&f].held.is_some());
    assert!(main_has(&w, "a", &ga) && main_has(&w, "b", &gb));
    assert!(!main_has(&w, "b", &fb));
    // G's landing of b syncs into F's copy of b while F is held.
    assert!(w.client_reads(&Client::Fork(f), "b").contains(&gb));
}

/// §3.6 "Seal", I3, R13: a transaction lands in its fork as one fork commit,
/// then in main as ONE commit carrying every staged file; until then other
/// clients of F see only F.
#[test]
fn s3_6_transaction_sealed_clean() {
    for policy in [SealConflict::Held, SealConflict::Rollback] {
        let mut w = world(HistoryPolicy::FullLineage, policy);
        let f = w.create_fork(Tree::Main);
        let c = Client::Fork(f);
        let t = w.begin(f, &["a", "b"]).unwrap();
        assert_eq!(
            w.begin(f, &["c"]),
            Err(agentfs_model::world::ModelError::InTransaction)
        );
        let ta = w.save(&c, "a", 0);
        let tb = w.save(&c, "b", 0);
        let outside = w.save(&c, "c", 0);
        settle(&mut w);
        assert!(w.client_reads(&c, "a").contains(&ta));
        assert!(
            !w.read(Tree::Fork(f), "a").contains(&ta),
            "F's other readers see F"
        );
        assert!(!main_has(&w, "a", &ta) && main_has(&w, "c", &outside));
        let before = main_writers(&w).len();
        w.seal(t);
        settle(&mut w);
        assert_eq!(w.txn_state(t), TxnState::Landed);
        assert_eq!(w.seal_answer(t), Some(SealAnswer::Landed));
        assert!(main_has(&w, "a", &ta) && main_has(&w, "b", &tb));
        assert_eq!(main_writers(&w).len(), before + 1, "one main commit");
        check_quiescent(&w).unwrap();
    }
}

/// §3.6 "Upstream landing meanwhile", `Merge` isolation: a sync lands in F's
/// file while the transaction stages it; the seal merges against F's newest
/// content (commit after pull).
#[test]
fn s3_6_seal_merges_upstream_landed_mid_transaction() {
    let mut w = world(HistoryPolicy::FullLineage, SealConflict::Held);
    let f = w.create_fork(Tree::Main);
    let c = Client::Fork(f);
    let first = w.save(&c, "a", 0);
    let t = w.begin(f, &["a"]).unwrap();
    let staged = w.save(&c, "a", 1);
    let upstream = w.save(&m1(), "a", 2);
    settle(&mut w);
    assert!(
        w.read(Tree::Fork(f), "a").contains(&upstream),
        "the sync landed in F"
    );
    assert!(
        !w.client_reads(&c, "a").contains(&upstream),
        "the client reads T"
    );
    w.seal(t);
    settle(&mut w);
    assert_eq!(w.txn_state(t), TxnState::Landed);
    for tok in [&first, &staged, &upstream] {
        assert!(main_has(&w, "a", tok), "{tok} in main");
    }
}

/// A13, A15, I3: a rollback leaves no content and no commit anywhere, and
/// the client reads F's latest, including what landed from upstream since.
#[test]
fn a13_rollback_leaves_no_trace() {
    let mut w = world(HistoryPolicy::FullLineage, SealConflict::Held);
    let f = w.create_fork(Tree::Main);
    let c = Client::Fork(f);
    w.save(&c, "a", 0);
    let t = w.begin(f, &["a", "b"]).unwrap();
    let staged = w.save(&c, "a", 1);
    w.save(&c, "b", 1);
    let upstream = w.save(&m1(), "a", 2);
    settle(&mut w);
    w.abandon(t);
    settle(&mut w);
    assert_eq!(w.txn_state(t), TxnState::RolledBack);
    let seen = w.client_reads(&c, "a");
    assert!(!seen.contains(&staged) && seen.contains(&upstream));
    assert!(w.rolled_back.contains(&staged));
    let refs = agentfs_model::git::git(w.repo(), &["for-each-ref", "--format=%(refname)"]);
    assert!(!refs.contains(&format!("txn/{t}")));
}

/// §3.6 "Rollback, client death", A13: the client dies with its
/// transaction open: it rolls back; F's own commits still land.
#[test]
fn a13_client_death_mid_transaction_rolls_back() {
    let mut w = world(HistoryPolicy::FullLineage, SealConflict::Held);
    let f = w.create_fork(Tree::Main);
    let c = Client::Fork(f);
    let saved = w.save(&c, "c", 0);
    let t = w.begin(f, &["a"]).unwrap();
    let h = w.open(&c, "a");
    w.save(&c, "a", 0);
    w.end_client(f);
    assert_eq!(w.close(h, 1), None, "the dead client's handle is gone");
    settle(&mut w);
    assert_eq!(w.txn_state(t), TxnState::RolledBack);
    assert!(main_has(&w, "c", &saved));
    assert_eq!(w.forks[&f].life, Life::Dropped);
    check_quiescent(&w).unwrap();
}

/// The last review's P0, §3.6 "Seal" and §3.5 "An ended fork's intake
/// drains": the program seals with a file still open in T, then exits. The
/// seal is not rolled back; its run lands in F once the dead client's
/// write-opens go, F's ending waits for T, and F's drain takes it to main.
#[test]
fn p0_seal_then_program_exits_still_reaches_main() {
    for policy in [SealConflict::Held, SealConflict::Rollback] {
        let mut w = world(HistoryPolicy::FullLineage, policy);
        let f = w.create_fork(Tree::Main);
        let c = Client::Fork(f);
        let t = w.begin(f, &["a", "b"]).unwrap();
        let staged = w.save(&c, "a", 0);
        let _h = w.open(&c, "b");
        w.seal(t);
        settle(&mut w);
        assert_eq!(
            w.txn_state(t),
            TxnState::Sealed,
            "waits for T's last write-open"
        );
        assert_eq!(w.seal_answer(t), None, "seal has not answered yet");
        w.end_client(f);
        ok(&mut w);
        assert!(!w.pending().contains(&Step::Drop(f)), "F's end waits for T");
        settle(&mut w);
        assert_eq!(w.txn_state(t), TxnState::Landed);
        assert!(
            main_has(&w, "a", &staged),
            "{policy:?}: the seal reached main"
        );
        assert_eq!(w.forks[&f].life, Life::Dropped);
        check_quiescent(&w).unwrap();
    }
}

/// §3.6 "Nested fork", §3.5 "A fork run": G over F lands in F as a fork
/// commit named for G's writer; F's run lands it in main alone, named for G.
#[test]
fn s3_6_nested_fork_lands_through_its_parent() {
    for history in [HistoryPolicy::FullLineage, HistoryPolicy::Squashed] {
        let mut w = world(history, SealConflict::Held);
        let f = w.create_fork(Tree::Main);
        let g = w.create_fork(Tree::Fork(f));
        let fa = w.save(&Client::Fork(f), "a", 0);
        let ga = w.save(&Client::Fork(g), "a", 1);
        settle(&mut w);
        assert!(main_has(&w, "a", &fa) && main_has(&w, "a", &ga));
        let writers = main_writers(&w);
        assert_eq!(
            writers[..2],
            ["c2", "c1"],
            "{history:?}: G's commit alone, named for G"
        );
        w.end_client(g);
        w.end_client(f);
        settle(&mut w);
        assert_eq!(w.forks[&f].life, Life::Dropped);
        check_quiescent(&w).unwrap();
    }
}

/// §5 "Other sources": a restore lands in main as the operator's record and
/// syncs into the copies forks hold.
#[test]
fn s5_restore_lands_and_syncs_into_copies() {
    let mut w = world(HistoryPolicy::FullLineage, SealConflict::Held);
    let old = w.main_head.clone();
    let gone = w.save(&m1(), "a", 0);
    let f = w.create_fork(Tree::Main);
    let fa = w.save(&Client::Fork(f), "a", 1);
    settle(&mut w);
    w.restore("a", &old);
    settle(&mut w);
    assert!(!main_has(&w, "a", &gone) && !main_has(&w, "a", &fa));
    assert!(!w.client_reads(&Client::Fork(f), "a").contains(&gone));
    assert_eq!(main_writers(&w)[0], "op");
}

/// R12, A7, I4: under `FullLineage` main's ancestry reaches the landed fork
/// commits and never a held one; under `Squashed` it reaches none of the
/// fork's line, only squash commits. `check` asserts both after each step.
#[test]
fn r12_main_ancestry_holds_only_landed_commits() {
    for history in [HistoryPolicy::FullLineage, HistoryPolicy::Squashed] {
        let mut w = world(history, SealConflict::Held);
        let f = w.create_fork(Tree::Main);
        let c = Client::Fork(f);
        w.save(&c, "a", 0);
        w.save(&c, "b", 0);
        settle(&mut w);
        w.save(&c, "c", 0);
        w.save(&m1(), "c", 0);
        w.save(&c, "d", 0);
        settle(&mut w);
        assert_eq!(w.forks[&f].landed, 2);
        let reach = agentfs_model::git::git(w.repo(), &["rev-list", "main"]);
        let fork = &w.forks[&f];
        let reached: Vec<bool> = fork
            .commits
            .iter()
            .map(|c| reach.contains(&c.git))
            .collect();
        let expected = history == HistoryPolicy::FullLineage;
        assert_eq!(reached, [expected, expected, false, false], "{history:?}");
    }
}

/// I2, R5, R6: every commit on every ref names exactly one writer, and main's
/// commits carry only their writer's changes, across main, two forks, a
/// nested fork, a sync and a sealed transaction.
#[test]
fn i2_one_writer_per_commit_everywhere() {
    for history in [HistoryPolicy::FullLineage, HistoryPolicy::Squashed] {
        let mut w = world(history, SealConflict::Held);
        let f = w.create_fork(Tree::Main);
        let g = w.create_fork(Tree::Fork(f));
        let h = w.create_fork(Tree::Main);
        w.save(&Client::Fork(f), "a", 0);
        w.save(&m1(), "a", 1);
        w.save(&Client::Main("m2".into()), "b", 1);
        w.save(&Client::Fork(g), "a", 2);
        w.save(&Client::Fork(h), "b", 2);
        let t = w.begin(h, &["c", "d"]).unwrap();
        w.save(&Client::Fork(h), "c", 3);
        w.save(&Client::Fork(h), "d", 3);
        w.seal(t);
        settle(&mut w);
        assert_eq!(w.txn_state(t), TxnState::Landed);
    }
}

/// The open question under §3.6, under each policy: a seal whose merge
/// conflicts with what landed in F meanwhile. `Held`: the seal answers held
/// and only an actor's `withdraw` settles it; F cannot drop until then.
/// `Rollback`: T drops at once, the program can retry, F drops normally.
#[test]
fn q_seal_conflict_under_each_policy() {
    for policy in [SealConflict::Held, SealConflict::Rollback] {
        let mut w = world(HistoryPolicy::FullLineage, policy);
        let f = w.create_fork(Tree::Main);
        let c = Client::Fork(f);
        w.save(&c, "a", 0);
        let t = w.begin(f, &["a"]).unwrap();
        let staged = w.save(&c, "a", 1);
        let upstream = w.save(&m1(), "a", 1);
        settle(&mut w);
        w.seal(t);
        settle(&mut w);
        assert!(main_has(&w, "a", &upstream) && !main_has(&w, "a", &staged));
        w.end_client(f);
        settle(&mut w);
        match policy {
            SealConflict::Held => {
                assert_eq!(w.txn_state(t), TxnState::Held);
                assert_eq!(w.seal_answer(t), Some(SealAnswer::Held));
                assert_eq!(w.forks[&f].life, Life::Ended, "F waits for an actor");
                w.withdraw_seal(t);
                settle(&mut w);
            }
            SealConflict::Rollback => {
                assert_eq!(w.txn_state(t), TxnState::RolledBack);
                assert_eq!(w.seal_answer(t), Some(SealAnswer::RolledBack));
            }
        }
        assert_eq!(w.forks[&f].life, Life::Dropped);
        assert!(w.rolled_back.contains(&staged));
        check_quiescent(&w).unwrap();
    }
}

/// landing.md §2 ("At the fork's next commit, the held commit and the new
/// one merge as one change ... the sandbox fixes a conflict by saving
/// again"), A8: the sandbox saves main's line over its conflicting one; the
/// held commit, the later one and the fix land as ONE record, and the run is
/// no longer held (the regression behind seeds 50, 72, 98 ... of the last
/// review: a held run that lands on re-judge must clear its hold).
#[test]
fn a8_saving_again_fixes_a_held_conflict() {
    let mut w = world(HistoryPolicy::FullLineage, SealConflict::Held);
    let f = w.create_fork(Tree::Main);
    let c = Client::Fork(f);
    w.save(&c, "a", 0);
    let main_side = w.save(&m1(), "a", 0);
    let mine = w.save(&c, "a", 1);
    let later = w.save(&c, "b", 0);
    settle(&mut w);
    assert_eq!(w.forks[&f].held, Some(0));
    let before = main_writers(&w).len();
    assert!(w.save_target_lines(f));
    settle(&mut w);
    assert_eq!(w.forks[&f].held, None, "landing clears the hold");
    assert_eq!(w.forks[&f].landed, w.forks[&f].commits.len());
    assert!(main_has(&w, "a", &main_side) && main_has(&w, "a", &mine));
    assert!(main_has(&w, "b", &later));
    assert_eq!(main_writers(&w).len(), before + 1, "one record, one commit");
    check_quiescent(&w).unwrap();
}

/// §3.5 Held to Waiting "or resolve" (landing.md §3: "an actor's merge,
/// named for that actor"): the held commit lands alone, named for the
/// actor, its conflicting line replaced by the actor's own that saw both;
/// the later commits follow as the fork's next run, named for the sandbox.
#[test]
fn s3_5_resolve_lands_the_held_commit_named_for_the_actor() {
    let mut w = world(HistoryPolicy::FullLineage, SealConflict::Held);
    let f = w.create_fork(Tree::Main);
    let c = Client::Fork(f);
    let held = w.save(&c, "a", 0);
    let main_side = w.save(&m1(), "a", 0);
    let later = w.save(&c, "b", 0);
    settle(&mut w);
    assert!(w.forks[&f].held.is_some());
    w.resolve(f);
    ok(&mut w);
    settle(&mut w);
    let a = w.read(Tree::Main, "a");
    assert!(a.slot(0).starts_with("actor."));
    assert!(w.lineage.descends(a.slot(0), &held) && w.lineage.descends(a.slot(0), &main_side));
    assert!(main_has(&w, "b", &later));
    assert_eq!(main_writers(&w)[..2], ["c1", "actor"]);
    check_quiescent(&w).unwrap();
}

/// §3.5 `Writing`: "verify (tracked unchanged, else one re-merge under the
/// gate)". The run is prepared, main changes another line of the same file,
/// and the run lands the re-merge: both changes stay. The run's base then
/// moves to the fork's own commit (`Kept`, I11), never to merged content.
#[test]
fn s3_5_writing_re_merges_once_when_the_target_moved() {
    let mut w = world(HistoryPolicy::FullLineage, SealConflict::Held);
    let f = w.create_fork(Tree::Main);
    let mine = w.save(&Client::Fork(f), "a", 0);
    assert!(w.step(&Step::Run(f)), "prepared in Waiting");
    let theirs = w.save(&m1(), "a", 1);
    ok(&mut w);
    assert!(w.step(&Step::Run(f)), "Writing: verify, re-merge, land");
    ok(&mut w);
    assert_eq!(w.re_merges, 1);
    assert!(main_has(&w, "a", &mine) && main_has(&w, "a", &theirs));
    assert_eq!(
        w.base_ref(f, "a", u64::MAX),
        Base::Kept {
            commit: w.forks[&f].commits[0].git.clone()
        }
    );
    settle(&mut w);
}

/// landing.md §2 "What ahead means": an earlier waiting landing whose scope
/// overlaps in the same tree goes first. G's run of a was made after F's,
/// so it waits while F's waits for main's write-open of a.
#[test]
fn s3_5_a_later_overlapping_landing_waits_behind_an_earlier_one() {
    let mut w = world(HistoryPolicy::FullLineage, SealConflict::Held);
    let f = w.create_fork(Tree::Main);
    let g = w.create_fork(Tree::Main);
    let h = w.open(&m1(), "a");
    let fa = w.save(&Client::Fork(f), "a", 0);
    let ga = w.save(&Client::Fork(g), "a", 1);
    settle(&mut w);
    assert!(!main_has(&w, "a", &fa) && !main_has(&w, "a", &ga));
    w.close(h, 2);
    settle(&mut w);
    assert!(main_has(&w, "a", &fa) && main_has(&w, "a", &ga));
    assert_eq!(main_writers(&w)[..3], ["c2", "c1", "m1"]);
}

/// history.md §2, commit group: on main, consecutive records of one writer
/// and one cause become one commit at the commit window; a landing record
/// is a group of its own. A sync's second parent is the newest main commit
/// at or below its record (§5).
#[test]
fn h2_main_commits_groups_at_the_window() {
    let mut w = world(HistoryPolicy::FullLineage, SealConflict::Held);
    w.save(&m1(), "a", 0);
    w.save(&m1(), "b", 0);
    w.save(&Client::Main("m2".into()), "c", 0);
    w.save(&m1(), "d", 0);
    assert_eq!(main_writers(&w), ["agentfs"], "nothing committed yet");
    assert!(w.step(&Step::Window));
    assert_eq!(main_writers(&w), ["m1", "m2", "m1", "agentfs"]);
    ok(&mut w);
}

/// Design finding 6 (system.md §3.5: "After `ForkEnded` a waiting sync into
/// the fork lands"; forks.md §5 events, `ForkEnded`: "`copies` stops naming
/// it", §3: "`copies` names open editable children"): F's sync of a waits;
/// F's client ends; F's own run of a lands in main, and that newer record no
/// longer joins the waiting sync. The stale sync then lands on the base the
/// run moved and reverts F's own landed line in F's copy. The model's fix,
/// `EndedForkCopies::UntilDropped`, keeps the join.
#[test]
#[ignore = "design finding 6: a stale sync into an ended fork reverts the fork's own landed change"]
fn finding_6_stale_sync_into_an_ended_fork() {
    ended_fork_sync(EndedForkCopies::StopAtEnd);
}

#[test]
fn finding_6_fix_copies_name_a_fork_until_it_drops() {
    ended_fork_sync(EndedForkCopies::UntilDropped);
}

fn ended_fork_sync(ended_fork_copies: EndedForkCopies) {
    let mut w = World::new(Policies {
        ended_fork_copies,
        ..Policies::default()
    });
    let f = w.create_fork(Tree::Main);
    let g = w.create_fork(Tree::Fork(f));
    let mine = w.save(&Client::Fork(f), "a", 0);
    w.save(&m1(), "a", 1);
    ok(&mut w);
    w.end_client(f);
    // F's run was made first, so it lands before the sync (one path base,
    // one order); then the sync.
    assert!(w.step(&Step::Run(f)) && w.step(&Step::Run(f)));
    ok(&mut w);
    w.settle();
    ok(&mut w);
    assert!(w.read(Tree::Fork(f), "a").contains(&mine));
    assert!(
        w.client_reads(&Client::Fork(g), "a").contains(&mine),
        "F's live child G reads F's own landed line"
    );
}

/// Design finding 1 (system.md §3.6 and history.md §1 "fork point", H3; R5,
/// R6): a fork's line starts at main's head when its first commit is built,
/// and later reads main live. A file it copies up after that carries main's
/// newer changes, so its fork commit's diff holds main's writer's change
/// under the sandbox's name.
#[test]
#[ignore = "design finding 1: a copy-up after the fork point puts main's change in a sandbox-named fork commit"]
fn finding_1_copy_up_after_fork_point_names_main_change_for_sandbox() {
    let mut w = world(HistoryPolicy::FullLineage, SealConflict::Held);
    let f = w.create_fork(Tree::Main);
    w.save(&Client::Fork(f), "a", 0);
    w.save(&m1(), "b", 0);
    w.save(&Client::Fork(f), "b", 1);
    w.settle();
    check(&mut w, Checks { fork_lines: true }).unwrap();
}

/// Design finding 2 (history.md §1 "squash chain", §2; system.md §4
/// `Squashed`): the squash chain starts at the fork point and holds the
/// landed paths, so a squash commit named for the sandbox also carries what
/// main changed in those files since the fork point.
#[test]
#[ignore = "design finding 2: a squash commit on the fork point carries main's changes under the sandbox's name"]
fn finding_2_squash_commit_carries_main_changes() {
    let mut w = world(HistoryPolicy::Squashed, SealConflict::Held);
    let f = w.create_fork(Tree::Main);
    // F's first commit holds a: its fork point is main's head before m1's
    // change; F's run merges m1's change, and its squash commit sits on the
    // fork point holding the merged a.
    w.save(&Client::Fork(f), "a", 0);
    let theirs = w.save(&m1(), "a", 1);
    settle(&mut w);
    let squash = agentfs_model::git::git(w.repo(), &["show", &format!("refs/squash/{f}")]);
    assert!(
        !squash.contains(&format!("+s1: {theirs}")),
        "the squash commit named c1 carries {theirs}:\n{squash}"
    );
}

/// Design finding 3 (system.md §3.5 "A sync": "one open sync per fork and
/// file: the parent's newer record joins it", "named for the parent
/// record's writer"; I2): two main records of p by different writers join
/// one sync while the fork holds p open, and the fork-line commit names one
/// writer for both changes. A deferred sync re-judged at a newer record does
/// the same, and so does §5's second flow whenever the fork's commit of the
/// file came before main's record: its run was made first, so landing.md §2's
/// ordering ("one path base, one order") lands it first, and main's landing
/// record joins the sync still open into the fork.
#[test]
#[ignore = "design finding 3: a sync joining two writers' records lands both under one writer"]
fn finding_3_joined_sync_names_one_writer_for_two() {
    let mut w = world(HistoryPolicy::FullLineage, SealConflict::Held);
    let f = w.create_fork(Tree::Main);
    let c = Client::Fork(f);
    w.save(&c, "a", 0);
    let h = w.open(&c, "a");
    w.save(&m1(), "a", 1);
    w.save(&Client::Main("m2".into()), "a", 2);
    w.close(h, 3);
    w.settle();
    assert!(w.joined_across_writers > 0);
    check(&mut w, Checks { fork_lines: true }).unwrap();
}

/// Design finding 4 (forks.md §3 `copies(over, p)`, §6 "Fork of a fork";
/// R18): a sync goes only to forks directly over the tree that recorded p.
/// When F reads p live from main and its child G holds its own p, main's
/// change of p never reaches G's copy: G drifts until its own commit of p
/// merges in F.
#[test]
#[ignore = "design finding 4: main's change never syncs into a grandchild's copy when the middle fork reads the file live"]
fn finding_4_grandchild_copy_misses_main_change() {
    let mut w = world(HistoryPolicy::FullLineage, SealConflict::Held);
    let f = w.create_fork(Tree::Main);
    let g = w.create_fork(Tree::Fork(f));
    // F's client holds a open, so G's run of a waits in F and G keeps its copy.
    let _h = w.open(&Client::Fork(f), "a");
    w.save(&Client::Fork(g), "a", 0);
    let upstream = w.save(&m1(), "a", 1);
    settle(&mut w);
    assert!(!w.forks[&f].own.contains("a"), "F reads a live");
    assert!(w.forks[&g].own.contains("a"), "G holds its own a");
    assert!(
        w.client_reads(&Client::Fork(g), "a").contains(&upstream),
        "main's change reached G's copy"
    );
}
