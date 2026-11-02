//! Seeded random interleavings of client calls and queue steps across
//! main, several forks (nested ones included) and transactions, with every
//! invariant checked after each step (`agentfs_model::check`), and, once
//! writes stop and the queues drain, convergence.
//!
//! On top of `check`, the run itself asserts "nothing is ever blocked or
//! held": every save returns at once, and every step takes its queue's
//! head (the queue shrinks by exactly one).
//!
//! The seeds are every seed in `1..=SEEDS`, never a set picked because it
//! passes. `AGENTFS_MODEL_SEED=<n>` runs one seed; `AGENTFS_MODEL_SEEDS=<n>`
//! runs seeds 1..=n. Each seed runs under `catch_unwind`, so a harness
//! panic too reports its seed and the event log. The generator is a
//! SplitMix64 written here: fixed seeds replay exactly.

use agentfs_model::check::{Checks, check, check_converged};
use agentfs_model::content::PATHS;
use agentfs_model::world::{
    Client, CollisionScope, Direction, ForkId, HistoryPolicy, Life, Policies, Step, SyncBase, Tree,
    TxnOutcome, World,
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
    landing_collisions: usize,
    sync_collisions: usize,
    committed: usize,
    rejected: usize,
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

/// One step, asserting it took its queue's head and nothing else of it.
fn take(w: &mut World, s: &Step, tally: &mut Tally) -> Result<(), String> {
    w.note_step(s);
    let before = match s {
        Step::Next(t) => w.queue_len(*t),
        Step::Drop(_) => 0,
    };
    w.step(s);
    tally.steps += 1;
    if let Step::Next(t) = s
        && w.alive(*t)
        && w.queue_len(*t) + 1 != before
    {
        return Err(format!("{s:?} did not take exactly its queue's head"));
    }
    Ok(())
}

fn drain(
    w: &mut World,
    checks: Checks,
    tally: &mut Tally,
    at: usize,
) -> Result<(), (usize, String)> {
    for _ in 0..100_000 {
        let Some(s) = w.pending().into_iter().next() else {
            return check_converged(w).map_err(|e| (at, e));
        };
        take(w, &s, tally).map_err(|e| (at, e))?;
        check(w, checks).map_err(|e| (at, e))?;
    }
    Err((at, "the queues never drained".into()))
}

/// One seed: [`run`] under `catch_unwind`. `Err` carries the seed, step,
/// violation and log.
fn run_seed(
    seed: u64,
    policies: Policies,
    checks: Checks,
    tally: &mut Tally,
) -> Result<(), String> {
    let mut w = World::new(policies);
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(&mut w, seed, checks, tally)
    }));
    let (step, e) = match out {
        Ok(Ok(())) => return Ok(()),
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

fn run(w: &mut World, seed: u64, checks: Checks, tally: &mut Tally) -> Result<(), (usize, String)> {
    let mut rng = Rng(seed);
    for step in 0..STEPS {
        let p = PATHS[rng.below(PATHS.len())];
        let slot = rng.below(HOT_SLOTS);
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
                }
            }
            7 => {
                if let Some(f) = rng.pick(&live_clients(w)) {
                    let _ = w.begin(f);
                }
            }
            8 => {
                if let Some(f) = rng.pick(&in_txn(w)) {
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
        check(w, checks).map_err(|e| (step, e))?;
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
    check(w, checks).map_err(|e| (STEPS, e))?;
    drain(w, checks, tally, STEPS)?;
    // Every program exits: every fork drops.
    for f in live_clients(w) {
        w.end_client(f);
    }
    drain(w, checks, tally, STEPS + 1)?;
    if let Some(f) = w.forks.values().find(|f| f.life != Life::Dropped) {
        return Err((STEPS + 1, format!("f{} never dropped", f.id)));
    }
    tally.fast_forwards += w.fast_forwards;
    for c in &w.collisions {
        match c.direction {
            Direction::Landing => tally.landing_collisions += 1,
            Direction::Sync => tally.sync_collisions += 1,
        }
    }
    for (_, _, o) in &w.outcomes {
        match o {
            TxnOutcome::Committed => tally.committed += 1,
            TxnOutcome::Rejected => tally.rejected += 1,
            TxnOutcome::Empty => tally.empty += 1,
        }
    }
    Ok(())
}

fn run_all(policies: Policies, checks: Checks) {
    let mut tally = Tally::default();
    let mut failures = Vec::new();
    for seed in seeds() {
        if let Err(e) = run_seed(seed, policies, checks, &mut tally) {
            failures.push(e);
        }
    }
    eprintln!(
        "{policies:?} {checks:?}, {} seeds: {tally:?}",
        seeds().len()
    );
    assert!(
        failures.is_empty(),
        "{} of {} seeds failed:\n\n{}",
        failures.len(),
        seeds().len(),
        failures.join("\n\n")
    );
}

/// The model's choices: full lineage, per hunk, each change merged on its
/// own base. Every invariant.
#[test]
fn random_interleavings_full_lineage() {
    run_all(
        Policies::new(HistoryPolicy::FullLineage, CollisionScope::Hunk),
        Checks::default(),
    );
}

/// `Squashed`, every invariant but "a collision is real" (finding 3).
#[test]
fn random_interleavings_squashed() {
    run_all(
        Policies::new(HistoryPolicy::Squashed, CollisionScope::Hunk),
        Checks {
            real_collisions: false,
        },
    );
}

/// Coherence finding 3: under `Squashed` main's ancestry forgets which
/// fork commit a landing carried, so a sync brings a fork's own landed
/// edit back as a "collision" over its newer edit, which loses.
#[test]
#[ignore = "coherence finding 3: Squashed (rule 7) loses the ancestry rule 2 needs: a fork's own landed edit beats its newer one"]
fn random_interleavings_squashed_collisions_are_real() {
    run_all(
        Policies::new(HistoryPolicy::Squashed, CollisionScope::Hunk),
        Checks::default(),
    );
}

/// Coherence finding 2: git's own recursive merge base, under the crossing
/// merges `FullLineage` makes, manufactures collisions and diverges.
#[test]
#[ignore = "coherence finding 2: git's recursive merge base of crossing syncs and landings manufactures collisions"]
fn random_interleavings_git_merge_base() {
    run_all(
        Policies {
            sync_base: SyncBase::Git,
            ..Policies::new(HistoryPolicy::FullLineage, CollisionScope::Hunk)
        },
        Checks::default(),
    );
}

/// Coherence finding 4: per file, the fork's sync result (the parent's
/// whole file) is recorded in git ancestry as the fork's own change; a
/// later merge keeps it, and the trees never converge.
#[test]
#[ignore = "coherence finding 4: per-file collisions do not converge"]
fn random_interleavings_per_file_full_lineage() {
    run_all(
        Policies::new(HistoryPolicy::FullLineage, CollisionScope::File),
        Checks::default(),
    );
}
