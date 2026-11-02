//! Seeded random interleavings of client calls and queue steps across
//! main, several forks (nested ones included) and transactions, with every
//! invariant checked after each step (`agentfs_model::check`), and, once
//! writes stop and the queues drain, convergence.
//!
//! On top of `check`, the run itself asserts "nothing is ever blocked or
//! held": every save returns at once, and every step takes its queue's
//! head (plus, under `Squashed`, the same fork's plain saves it carries as
//! one changeset), leaving the rest of the queue in order.
//!
//! The seeds are every seed in `1..=SEEDS`, never a set picked because it
//! passes. `AGENTFS_MODEL_SEED=<n>` runs one seed; `AGENTFS_MODEL_SEEDS=<n>`
//! runs seeds 1..=n. Each seed runs under `catch_unwind`, so a harness
//! panic too reports its seed and the event log. The generator is a
//! SplitMix64 written here: fixed seeds replay exactly.

use agentfs_model::check::{check, check_converged, step_checked};
use agentfs_model::content::{Layout, PATHS};
use agentfs_model::git::with_reversed_dates;
use agentfs_model::world::{
    Client, CollisionScope, Direction, ForkId, HistoryPolicy, Life, MergeRule, Policies, Step,
    Tree, TxnEnd, TxnOutcome, World,
};

/// CI runs seeds 1..=SEEDS.
const SEEDS: u64 = 64;
const STEPS: usize = 80;
/// Slots the random clients write: three of four, so edits collide often.
const HOT_SLOTS: usize = 3;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<T: Clone>(&mut self, v: &[T]) -> Option<T> {
        (!v.is_empty()).then(|| v[self.below(v.len())].clone())
    }
}

#[derive(Default, Debug)]
struct Tally {
    steps: usize,
    fast_forwards: usize,
    squashed_runs: usize,
    landing_collisions: usize,
    transaction_collisions: usize,
    committed: usize,
    overwritten: usize,
    empty: usize,
    rolled_back: usize,
    forks: usize,
    nested: usize,
}

fn seeds() -> Vec<u64> {
    if let Ok(s) = std::env::var("AGENTFS_MODEL_SEED") {
        return vec![s.parse().expect("AGENTFS_MODEL_SEED is a number")];
    }
    if let Ok(n) = std::env::var("AGENTFS_MODEL_SEEDS") {
        let n: u64 = n.parse().expect("AGENTFS_MODEL_SEEDS is a number");
        return (1..=n).collect();
    }
    (1..=SEEDS).collect()
}

fn live_clients(w: &World) -> Vec<ForkId> {
    w.forks
        .values()
        .filter(|f| f.life == Life::Open)
        .map(|f| f.id)
        .collect()
}

fn in_txn(w: &World) -> Vec<ForkId> {
    w.forks
        .values()
        .filter(|f| f.txn.is_some())
        .map(|f| f.id)
        .collect()
}

/// One step ([`step_checked`]), asserting it took its queue's head (and, under `Squashed`,
/// the entries it says it carried with it), and left the rest in order.
fn take(w: &mut World, s: &Step, tally: &mut Tally) -> Result<(), String> {
    w.note_step(s);
    let before = match s {
        Step::Next(t) => w.queue(*t),
        Step::Drop(_) => Vec::new(),
    };
    let taken = step_checked(w, s)?;
    tally.steps += 1;
    if let Step::Next(t) = s
        && w.alive(*t)
    {
        let after = w.queue(*t);
        let mut rest = before.iter().skip(1);
        let in_order = after.iter().all(|e| rest.any(|b| b == e));
        let squash = w.policies.history == HistoryPolicy::Squashed;
        if taken == 0 || (taken > 1 && !squash) || after.len() + taken != before.len() || !in_order
        {
            return Err(format!(
                "{s:?} took {taken} entries: not its queue's head, or the rest out of order"
            ));
        }
    }
    Ok(())
}

fn drain(w: &mut World, tally: &mut Tally, at: usize) -> Result<(), (usize, String)> {
    for _ in 0..100_000 {
        let Some(s) = w.pending().into_iter().next() else {
            return check_converged(w).map_err(|e| (at, e));
        };
        take(w, &s, tally).map_err(|e| (at, e))?;
        check(w).map_err(|e| (at, e))?;
    }
    Err((at, "the queues never drained".into()))
}

/// One seed: [`run`] under `catch_unwind`. `Ok` carries the run's
/// [`World::fingerprint`]; `Err` the seed, step, violation and log.
fn run_seed(seed: u64, policies: Policies, tally: &mut Tally) -> Result<String, String> {
    let mut w = World::new(policies);
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut w, seed, tally)));
    let (step, e) = match out {
        Ok(Ok(())) => return Ok(w.fingerprint()),
        Ok(Err(e)) => e,
        Err(panic) => {
            let msg = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            (usize::MAX, format!("harness panic: {msg}"))
        }
    };
    Err(format!(
        "seed {seed}, step {step}: {e}\nreplay: AGENTFS_MODEL_SEED={seed}\nlog:\n  {}",
        w.log.join("\n  ")
    ))
}

fn run(w: &mut World, seed: u64, tally: &mut Tally) -> Result<(), (usize, String)> {
    let mut rng = Rng(seed);
    // The line a fork wrote last. Main and forks write it again half the
    // time, so runs often build "a fork edits over its own undecided line
    // while main writes it concurrently" (coherence finding 8).
    let mut focus: Option<(&str, usize)> = None;
    for step in 0..STEPS {
        let (p, slot) = match focus {
            Some(f) if rng.below(2) == 0 => f,
            _ => (PATHS[rng.below(PATHS.len())], rng.below(HOT_SLOTS)),
        };
        match rng.below(20) {
            0..=2 => {
                let who = if rng.below(2) == 0 { "m1" } else { "m2" };
                w.save(&Client::Main(who.into()), p, slot);
            }
            3 => {
                let forks = live_clients(w);
                if forks.len() < 4 {
                    let parent = match rng.pick(&forks) {
                        Some(f) if rng.below(3) == 0 => {
                            tally.nested += 1;
                            Tree::Fork(f)
                        }
                        _ => Tree::Main,
                    };
                    w.create_fork(parent);
                    tally.forks += 1;
                }
            }
            4..=6 => {
                if let Some(f) = rng.pick(&live_clients(w)) {
                    w.save(&Client::Fork(f), p, slot);
                    focus = Some((p, slot));
                }
            }
            7 => {
                if let Some(f) = rng.pick(&live_clients(w)) {
                    let _ = w.begin(f);
                }
            }
            8 => {
                // Mostly commit a transaction that staged something, so
                // commits meet concurrent lines.
                if let Some(f) = rng.pick(&in_txn(w))
                    && (w.forks[&f]
                        .txn
                        .as_ref()
                        .is_some_and(|t| !t.staged.is_empty())
                        || rng.below(4) == 0)
                {
                    w.commit_txn(f).expect("open");
                }
            }
            9 => {
                if let Some(f) = rng.pick(&in_txn(w))
                    && rng.below(2) == 0
                {
                    w.rollback(f).expect("open");
                    tally.rolled_back += 1;
                }
            }
            10 => {
                if let Some(f) = rng.pick(&live_clients(w))
                    && rng.below(3) == 0
                {
                    w.end_client(f);
                }
            }
            _ => {
                if let Some(s) = rng.pick(&w.pending()) {
                    take(w, &s, tally).map_err(|e| (step, e))?;
                }
            }
        }
        check(w).map_err(|e| (step, e))?;
    }
    // Writes stop: every open transaction commits or rolls back; the
    // queues drain; every fork equals main.
    for f in in_txn(w) {
        if rng.below(2) == 0 {
            w.commit_txn(f).expect("open");
        } else {
            w.rollback(f).expect("open");
            tally.rolled_back += 1;
        }
    }
    check(w).map_err(|e| (STEPS, e))?;
    drain(w, tally, STEPS)?;
    // Every program exits: every fork drops.
    for f in live_clients(w) {
        w.end_client(f);
    }
    drain(w, tally, STEPS + 1)?;
    if let Some(f) = w.forks.values().find(|f| f.life != Life::Dropped) {
        return Err((STEPS + 1, format!("f{} never dropped", f.id)));
    }
    tally.fast_forwards += w.fast_forwards;
    tally.squashed_runs += w.squashed_runs;
    for c in &w.collisions {
        match c.direction {
            Direction::Landing => tally.landing_collisions += 1,
            Direction::Transaction => tally.transaction_collisions += 1,
        }
    }
    for e in &w.ended {
        match &e.how {
            TxnEnd::Commit(TxnOutcome::Committed { overwritten }) => {
                tally.committed += 1;
                tally.overwritten += overwritten.len();
            }
            TxnEnd::Commit(TxnOutcome::Empty) => tally.empty += 1,
            TxnEnd::Rollback => {}
        }
    }
    Ok(())
}

fn run_all(policies: Policies) {
    let mut tally = Tally::default();
    let mut failures = Vec::new();
    for seed in seeds() {
        if let Err(e) = run_seed(seed, policies, &mut tally) {
            failures.push(e);
        }
    }
    eprintln!("{policies:?}, {} seeds: {tally:?}", seeds().len());
    assert!(
        failures.is_empty(),
        "{} of {} seeds failed:\n\n{}",
        failures.len(),
        seeds().len(),
        failures.join("\n\n")
    );
    // Rule 7: across the CI seeds (or more), squashing really happened.
    // One seed alone may have no run of waiting saves.
    if policies.history == HistoryPolicy::Squashed && seeds().len() as u64 >= SEEDS {
        assert!(
            tally.squashed_runs > 0,
            "Squashed: no run of saves landed as one changeset in {} seeds",
            seeds().len()
        );
    }
}

/// The model's choices: full lineage, per line, per-line syncs. Every
/// invariant.
#[test]
fn random_interleavings_full_lineage() {
    run_all(Policies::new(
        HistoryPolicy::FullLineage,
        CollisionScope::Line,
    ));
}

/// `Squashed`, every invariant: a fork's waiting saves land as one
/// changeset, main's stored history holds only main's commits, and the
/// graft beside it keeps the lineage rules 2 and 3 read.
#[test]
fn random_interleavings_squashed() {
    run_all(Policies::new(HistoryPolicy::Squashed, CollisionScope::Line));
}

/// The per-line rule does not depend on slots being separate git hunks:
/// the same runs with every slot on a neighbouring line.
#[test]
fn random_interleavings_adjacent_lines() {
    run_all(Policies {
        layout: Layout::Adjacent,
        ..Policies::new(HistoryPolicy::FullLineage, CollisionScope::Line)
    });
}

/// Rule 2, "no clock is read anywhere", on the random runs: seeds 1..=8
/// under each history setting run again with every commit dated before its
/// parents decide exactly the same trees and collisions
/// ([`World::fingerprint`]). An order taken from commit dates would differ.
#[test]
fn random_interleavings_commit_dates_decide_nothing() {
    for history in [HistoryPolicy::FullLineage, HistoryPolicy::Squashed] {
        let policies = Policies::new(history, CollisionScope::Line);
        for seed in seeds().into_iter().take(8) {
            let mut tally = Tally::default();
            let pinned = run_seed(seed, policies, &mut tally);
            let reversed = with_reversed_dates(|| run_seed(seed, policies, &mut tally));
            assert_eq!(pinned, reversed, "{history:?} seed {seed}");
            assert!(pinned.is_ok(), "{history:?} seed {seed}: {pinned:?}");
        }
    }
}

/// Coherence finding 1: git's own merge base cannot tell a fork line the
/// parent already decided against from one still on its way.
#[test]
#[ignore = "coherence finding 1: git's own merge base of crossing syncs and landings keeps a fork line the parent already decided against"]
fn random_interleavings_git_merge_base() {
    run_all(Policies {
        merge: MergeRule::GitMergeBase,
        ..Policies::new(HistoryPolicy::FullLineage, CollisionScope::Line)
    });
}

/// Coherence finding 3: per file, the parent's file reverts the fork's
/// newer clean lines.
#[test]
#[ignore = "coherence finding 3: per file, the parent's winning file reverts the fork's newer clean lines"]
fn random_interleavings_per_file_full_lineage() {
    run_all(Policies::new(
        HistoryPolicy::FullLineage,
        CollisionScope::File,
    ));
}
