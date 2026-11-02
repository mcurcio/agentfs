//! Seeded random interleavings of client calls and landing steps across
//! main, several forks (nested ones included) and transactions, with every
//! global invariant checked after each step (`agentfs_model::check`).
//!
//! The seeds are fixed, so a failure replays: it prints its seed and the
//! event log. `AGENTFS_MODEL_SEED=<n>` runs one seed; `AGENTFS_MODEL_SEEDS=<n>`
//! runs seeds 1..=n instead of the recorded ones. The generator is a
//! SplitMix64 written here rather than a property-testing crate: fixed seeds
//! replay exactly, and shrinking is not worth a dependency for logs this
//! short.

use agentfs_model::check::{Checks, check, check_quiescent};
use agentfs_model::content::PATHS;
use agentfs_model::world::{
    Client, ForkId, HistoryPolicy, Life, Policies, SealConflict, Tree, TxnState, World,
};

/// The recorded seeds every run replays.
const SEEDS: [u64; 16] = [
    1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144, 233, 377, 610, 987, 1597,
];
const STEPS: usize = 80;
/// Slots the random clients write: two of the four, so edits collide often
/// enough to hold runs, defer syncs and conflict seals.
const HOT_SLOTS: usize = 2;

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
    held_seals_left: usize,
    forks_waiting_for_an_actor: usize,
    seals_landed: usize,
    seals_rolled_back: usize,
    runs_held: usize,
    syncs_deferred: usize,
    syncs_joined_across_writers: usize,
}

fn seeds() -> Vec<u64> {
    if let Ok(s) = std::env::var("AGENTFS_MODEL_SEED") {
        return vec![s.parse().expect("AGENTFS_MODEL_SEED is a number")];
    }
    if let Ok(n) = std::env::var("AGENTFS_MODEL_SEEDS") {
        let n: u64 = n.parse().expect("AGENTFS_MODEL_SEEDS is a number");
        return (1..=n).collect();
    }
    SEEDS.to_vec()
}

fn live_clients(w: &World) -> Vec<ForkId> {
    w.forks
        .values()
        .filter(|f| !f.is_txn() && f.life == Life::Open)
        .map(|f| f.id)
        .collect()
}

fn open_txns(w: &World) -> Vec<ForkId> {
    w.forks
        .values()
        .filter(|f| f.txn == Some(TxnState::Open))
        .map(|f| f.id)
        .collect()
}

/// One random run; `Err` carries the seed, step and log.
fn run(seed: u64, policies: Policies, checks: Checks, tally: &mut Tally) -> Result<(), String> {
    let mut w = World::new(policies);
    let mut rng = Rng(seed);
    let fail = |w: &World, step: usize, e: String| {
        format!(
            "seed {seed}, step {step}: {e}\nreplay: AGENTFS_MODEL_SEED={seed}\nlog:\n  {}",
            w.log.join("\n  ")
        )
    };
    for step in 0..STEPS {
        let p = PATHS[rng.below(PATHS.len())];
        let slot = rng.below(HOT_SLOTS);
        match rng.below(21) {
            0..=2 => {
                let who = if rng.below(2) == 0 { "m1" } else { "m2" };
                w.save(&Client::Main(who.into()), p, slot);
            }
            3 => {
                let mains: Vec<_> = w
                    .open_handles()
                    .into_iter()
                    .filter(|h| matches!(w.handle_client(*h), Some(Client::Main(_))))
                    .collect();
                if mains.is_empty() {
                    w.open(&Client::Main("m1".into()), p);
                } else if let Some(h) = rng.pick(&mains) {
                    w.close(h, slot);
                }
            }
            4 => {
                let forks = live_clients(&w);
                if forks.len() < 4 {
                    let parent = match rng.pick(&forks) {
                        Some(f) if rng.below(3) == 0 => Tree::Fork(f),
                        _ => Tree::Main,
                    };
                    w.create_fork(parent);
                }
            }
            5..=7 => {
                if let Some(f) = rng.pick(&live_clients(&w)) {
                    w.save(&Client::Fork(f), p, slot);
                }
            }
            8 => {
                let forks: Vec<_> = w
                    .open_handles()
                    .into_iter()
                    .filter(|h| matches!(w.handle_client(*h), Some(Client::Fork(_))))
                    .collect();
                if forks.is_empty() || rng.below(2) == 0 {
                    if let Some(f) = rng.pick(&live_clients(&w)) {
                        w.open(&Client::Fork(f), p);
                    }
                } else if let Some(h) = rng.pick(&forks) {
                    w.close(h, slot);
                }
            }
            9 | 19 => {
                if let Some(f) = rng.pick(&live_clients(&w)) {
                    let q = PATHS[rng.below(PATHS.len())];
                    let _ = w.begin(f, &[p, q]);
                }
            }
            10 => {
                if let Some(t) = rng.pick(&open_txns(&w)) {
                    w.seal(t);
                }
            }
            11 => {
                if let Some(t) = rng.pick(&open_txns(&w))
                    && rng.below(2) == 0
                {
                    w.abandon(t);
                }
            }
            12 => {
                if let Some(f) = rng.pick(&live_clients(&w))
                    && rng.below(3) == 0
                {
                    w.end_client(f);
                }
            }
            13 => {
                let old = rng.pick(&w.main_commits).expect("main has a commit");
                if rng.below(3) == 0 {
                    w.restore(p, &old);
                }
            }
            14 => {
                let held: Vec<_> = w
                    .forks
                    .values()
                    .filter(|f| f.held.is_some() || f.txn == Some(TxnState::Held))
                    .map(|f| f.id)
                    .collect();
                if let Some(f) = rng.pick(&held)
                    && rng.below(2) == 0
                {
                    if w.forks[&f].is_txn() {
                        w.withdraw_seal(f);
                    } else {
                        w.withdraw(f);
                    }
                }
            }
            _ => {
                if let Some(s) = rng.pick(&w.pending()) {
                    w.note_step(&s);
                    w.step(&s);
                }
            }
        }
        check(&mut w, checks).map_err(|e| fail(&w, step, e))?;
    }
    // Quiesce: every program closes its files and seals; landings settle.
    for h in w.open_handles() {
        w.close(h, 0);
    }
    for t in open_txns(&w) {
        w.seal(t);
    }
    w.settle();
    check(&mut w, checks).map_err(|e| fail(&w, STEPS, e))?;
    check_quiescent(&w).map_err(|e| fail(&w, STEPS, e))?;
    for f in w.forks.values() {
        match f.txn {
            Some(TxnState::Held) => tally.held_seals_left += 1,
            Some(TxnState::Landed) => tally.seals_landed += 1,
            Some(TxnState::RolledBack) => tally.seals_rolled_back += 1,
            _ => {}
        }
        if !f.is_txn() && f.held.is_some() {
            tally.runs_held += 1;
        }
    }
    // Every program exits: which forks can still not drop without an actor?
    for f in live_clients(&w) {
        w.end_client(f);
    }
    w.settle();
    check(&mut w, checks).map_err(|e| fail(&w, STEPS + 1, e))?;
    tally.forks_waiting_for_an_actor += w
        .forks
        .values()
        .filter(|f| !f.is_txn() && f.life == Life::Ended)
        .count();
    tally.syncs_deferred += w.deferred;
    tally.syncs_joined_across_writers += w.joined_across_writers;
    Ok(())
}

fn run_all(history: HistoryPolicy, seal_conflict: SealConflict, checks: Checks) {
    let mut tally = Tally::default();
    let mut failures = Vec::new();
    for seed in seeds() {
        let policies = Policies {
            history,
            seal_conflict,
            ..Policies::default()
        };
        if let Err(e) = run(seed, policies, checks, &mut tally) {
            failures.push(e);
        }
    }
    eprintln!("{history:?} {seal_conflict:?} {checks:?}: {tally:?}");
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn random_interleavings_full_lineage_seal_held() {
    run_all(
        HistoryPolicy::FullLineage,
        SealConflict::Held,
        Checks::default(),
    );
}

#[test]
fn random_interleavings_full_lineage_seal_rollback() {
    run_all(
        HistoryPolicy::FullLineage,
        SealConflict::Rollback,
        Checks::default(),
    );
}

#[test]
fn random_interleavings_squashed_seal_held() {
    run_all(
        HistoryPolicy::Squashed,
        SealConflict::Held,
        Checks::default(),
    );
}

#[test]
fn random_interleavings_squashed_seal_rollback() {
    run_all(
        HistoryPolicy::Squashed,
        SealConflict::Rollback,
        Checks::default(),
    );
}

/// The same runs with the one-writer check extended to fork lines and squash
/// chains: what design findings 1 to 3 break.
#[test]
#[ignore = "design findings 1-3: fork-line and squash commits carry other writers' changes"]
fn random_interleavings_one_writer_on_fork_lines() {
    run_all(
        HistoryPolicy::FullLineage,
        SealConflict::Held,
        Checks { fork_lines: true },
    );
    run_all(
        HistoryPolicy::Squashed,
        SealConflict::Rollback,
        Checks { fork_lines: true },
    );
}
