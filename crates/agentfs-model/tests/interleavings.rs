//! Seeded random interleavings of client calls and queue steps across
//! main, several forks (nested ones included) and transactions, with every
//! invariant checked after each step (`agentfs_model::check`), and, once
//! writes stop and the queues drain, convergence.
//!
//! Clients open files, make write calls (appends, writes at an offset,
//! inserts, deletes and moves of lines, some with bytes other lines repeat,
//! and whole-file rewrites of an earlier read) and close them, so takes
//! wait for write-opens and opens wait for takes (rule 9); they unlink and
//! rename files (rule 11), to a new path or over another file. On top of
//! `check`, the run itself asserts that nothing else waits: every close
//! returns at once, and every queue step takes its queue's head (plus,
//! under `Squashed`, the same fork's plain saves it carries as one
//! changeset), leaving the rest of the queue in order, or records that it
//! waits. And it keeps its own record of every write call's bytes: each
//! close it makes saves exactly the bytes those calls leave on the head
//! then (computed here, not by the model's replay).
//!
//! The seeds are every seed in `1..=SEEDS`, never a set picked because it
//! passes. `AGENTFS_MODEL_SEED=<n>` runs one seed; `AGENTFS_MODEL_SEEDS=<n>`
//! runs seeds 1..=n. Each seed runs under `catch_unwind`, so a harness
//! panic too reports its seed and the event log. The generator is a
//! SplitMix64 written here: fixed seeds replay exactly.

use agentfs_model::check::{check, check_converged, step_checked};
use agentfs_model::content::{File, PATHS, REPEATED};
use agentfs_model::git::with_reversed_dates;
use agentfs_model::world::{
    Client, CollisionScope, Direction, ForkId, HandleId, HistoryPolicy, Kind, Life, ModelError,
    Policies, Step, Tree, TxnEnd, TxnOutcome, World, Write,
};
use std::collections::BTreeMap;

/// CI runs seeds 1..=SEEDS.
const SEEDS: u64 = 64;
const STEPS: usize = 80;
/// Positions the random clients write at: the first three lines, so edits
/// collide often.
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
    /// Queue heads that waited for a write-open, and opens that waited for
    /// a take.
    take_waits: usize,
    open_waits: usize,
    rewrites: usize,
    /// Appends, and those two opens made to one file at once.
    appends: usize,
    concurrent_appends: usize,
    inserts: usize,
    deletes: usize,
    moves: usize,
    /// New lines whose bytes repeat other lines'.
    repeated: usize,
    unlinks: usize,
    renames: usize,
    rename_overs: usize,
    main_deaths: usize,
    /// Commits syncs replayed, and those that became empty.
    rebased: usize,
    rebased_empty: usize,
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
/// the entries it says it carried with it), and left the rest in order, or
/// waits and left the queue as it was.
fn take(w: &mut World, s: &Step, tally: &mut Tally) -> Result<(), String> {
    w.note_step(s);
    let before = match s {
        Step::Next(t) | Step::Finish(t) => w.queue(*t),
        Step::Drop(_) | Step::Replicate => Vec::new(),
    };
    let taken = step_checked(w, s)?;
    tally.steps += 1;
    if let Step::Finish(t) = s
        && w.queue(*t) != before
    {
        return Err(format!("{s:?} changed {t:?}'s own queue"));
    }
    if let Step::Next(t) = s
        && w.alive(*t)
    {
        let after = w.queue(*t);
        if taken == 0 {
            tally.take_waits += 1;
            return if after == before && w.waits().contains_key(t) {
                Ok(())
            } else {
                Err(format!("{s:?} took nothing but does not wait"))
            };
        }
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

/// Each open's write calls with the bytes each wrote, as the harness made
/// them.
type Written = BTreeMap<HandleId, Vec<(Write, String)>>;

/// The bytes a program's write calls leave on `img`, the head's lines at
/// close: the harness's own account, kept apart from the model's replay.
fn bytes_after(mut img: File, calls: &[(Write, String)]) -> File {
    for (call, x) in calls {
        match call {
            Write::Append => img.push(x.clone()),
            Write::At(i) if *i < img.len() => img[*i] = x.clone(),
            Write::At(_) => img.push(x.clone()),
            Write::Insert(i) => img.insert((*i).min(img.len()), x.clone()),
            Write::Delete(i) if *i < img.len() => {
                img.remove(*i);
            }
            Write::Delete(_) => {}
            Write::Move { from, to } if from != to && *from < img.len() => {
                let l = img.remove(*from);
                img.insert((*to).min(img.len()), l);
            }
            Write::Move { .. } => {}
            Write::Rewrite(c) => img = c.clone(),
        }
    }
    img
}

/// Closes `h`, asserting that the save holds exactly the bytes its write
/// calls leave on the head now (none if its file left the tree meanwhile).
fn close_checked(w: &mut World, written: &mut Written, h: HandleId) -> Result<(), String> {
    let (target, file) = (w.handle(h).target, w.handle(h).file.clone());
    let before = file
        .as_ref()
        .and_then(|p| w.file(&w.target_head(target), p));
    let calls = written.remove(&h).unwrap_or_default();
    w.close(h);
    let (Some(p), Some(before)) = (file, before) else {
        return Ok(());
    };
    let expect = bytes_after(before, &calls);
    let after = w.file(&w.target_head(target), &p);
    if after.as_ref() != Some(&expect) {
        return Err(format!(
            "close h{h} of {p}: saved {after:?}, but its write calls leave {expect:?}"
        ));
    }
    Ok(())
}

/// Every program closes the files it has open (granted ones; a waiting
/// open is closed once a take's release grants it).
fn close_all_checked(w: &mut World, written: &mut Written) -> Result<(), String> {
    let open: Vec<_> = w
        .handles()
        .filter(|h| !h.waiting && h.kind == Kind::Write)
        .map(|h| h.id)
        .collect();
    for h in open {
        close_checked(w, written, h)?;
    }
    Ok(())
}

/// [`close_all_checked`] for opens with no write calls recorded.
fn close_all(w: &mut World) {
    let open: Vec<_> = w
        .handles()
        .filter(|h| !h.waiting && h.kind == Kind::Write)
        .map(|h| h.id)
        .collect();
    for h in open {
        w.close(h);
    }
}

fn drain(w: &mut World, tally: &mut Tally, at: usize) -> Result<(), (usize, String)> {
    for _ in 0..100_000 {
        close_all(w);
        check(w).map_err(|e| (at, e))?;
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

/// Opens `p` for writing as `c` and, if granted, writes line `slot` and
/// closes: a save. A waiting open stays, to be written once granted.
fn save(w: &mut World, c: &Client, p: &str, slot: usize, tally: &mut Tally) {
    let h = w.open_write(c, p);
    if w.handle(h).waiting {
        tally.open_waits += 1;
        return;
    }
    let x = w.write(h, Write::At(slot)).expect("a write at an offset");
    let mut written = Written::from([(h, vec![(Write::At(slot), x)])]);
    close_checked(w, &mut written, h).expect("a save holds its bytes");
}

/// Path number `i` (mod how many) of those client `c` sees, if any.
fn path(w: &World, c: &Client, i: usize) -> Option<String> {
    let ps = w.client_paths(c);
    (!ps.is_empty()).then(|| ps[i % ps.len()].clone())
}

fn run(w: &mut World, seed: u64, tally: &mut Tally) -> Result<(), (usize, String)> {
    let mut rng = Rng(seed);
    // The line a fork wrote last (which of its client's paths, and the
    // position). Main and forks write it again half the time, so runs
    // often build "a fork edits over its own undecided line while main
    // writes it concurrently" (coherence finding 8).
    let mut focus: Option<(usize, usize)> = None;
    // Each client's last read of each path, which a later whole-file
    // rewrite writes back: often stale by then.
    let mut reads: BTreeMap<(String, String), File> = BTreeMap::new();
    // Main's live clients: one may die (its end event) and a new one start.
    let mut mains: Vec<String> = vec!["m1".into(), "m2".into()];
    let mut next_main = 3;
    let mut next_name = 1;
    let mut written = Written::new();
    for step in 0..STEPS {
        let (f, slot) = match focus {
            Some(x) if rng.below(2) == 0 => x,
            _ => (rng.below(PATHS.len() + 2), rng.below(HOT_SLOTS)),
        };
        let mut clients: Vec<Client> = mains.iter().cloned().map(Client::Main).collect();
        clients.extend(live_clients(w).into_iter().map(Client::Fork));
        let open: Vec<_> = w
            .handles()
            .filter(|h| !h.waiting && h.kind == Kind::Write)
            .map(|h| h.id)
            .collect();
        match rng.below(30) {
            0..=2 => {
                let who = Client::Main(mains[rng.below(mains.len())].clone());
                if let Some(p) = path(w, &who, f) {
                    save(w, &who, &p, slot, tally);
                }
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
                if let Some(c) = rng.pick(&live_clients(w)).map(Client::Fork)
                    && let Some(p) = path(w, &c, f)
                {
                    save(w, &c, &p, slot, tally);
                    focus = Some((f, slot));
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
                    let _ = w.commit_txn(f);
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
                if rng.below(4) == 0 {
                    // One of main's clients dies; a new one starts.
                    let i = rng.below(mains.len());
                    w.end_main_client(&mains[i]);
                    tally.main_deaths += 1;
                    mains[i] = format!("m{next_main}");
                    next_main += 1;
                } else if let Some(f) = rng.pick(&live_clients(w))
                    && rng.below(3) == 0
                {
                    w.end_client(f);
                }
            }
            11 | 12 => {
                if let Some(c) = rng.pick(&clients)
                    && let Some(p) = path(w, &c, f)
                {
                    let h = w.open_write(&c, &p);
                    tally.open_waits += usize::from(w.handle(h).waiting);
                }
            }
            13..=15 => {
                if let Some(h) = rng.pick(&open) {
                    let (c, file) = (w.handle(h).client.clone(), w.handle(h).file.clone());
                    let call = match rng.below(6) {
                        0 => {
                            tally.appends += 1;
                            let target = w.handle(h).target;
                            tally.concurrent_appends += usize::from(w.handles().any(|o| {
                                o.id != h
                                    && o.kind == Kind::Write
                                    && o.target == target
                                    && o.file == file
                            }));
                            Write::Append
                        }
                        1 => Write::At(slot),
                        2 => {
                            tally.inserts += 1;
                            Write::Insert(slot)
                        }
                        3 => {
                            tally.deletes += 1;
                            Write::Delete(slot)
                        }
                        4 => {
                            tally.moves += 1;
                            Write::Move {
                                from: slot,
                                to: rng.below(HOT_SLOTS + 2),
                            }
                        }
                        _ => {
                            let Some(file) = file.clone() else { continue };
                            let read = reads.get(&(format!("{c:?}"), file.clone())).cloned();
                            let Some(read) = read.or_else(|| w.client_reads(&c, &file)) else {
                                continue;
                            };
                            tally.rewrites += 1;
                            Write::Rewrite(read)
                        }
                    };
                    let makes = matches!(call, Write::Append | Write::At(_) | Write::Insert(_));
                    let made = if makes && rng.below(3) == 0 {
                        tally.repeated += 1;
                        let text = REPEATED[rng.below(REPEATED.len())];
                        w.write_text(h, call.clone(), text);
                        text.to_string()
                    } else {
                        w.write(h, call.clone()).unwrap_or_default()
                    };
                    written.entry(h).or_default().push((call, made));
                }
            }
            16 => {
                if let Some(h) = rng.pick(&open) {
                    close_checked(w, &mut written, h).map_err(|e| (step, e))?;
                }
            }
            17 => {
                if let Some(c) = rng.pick(&clients)
                    && let Some(p) = path(w, &c, f)
                {
                    let read = w.client_reads(&c, &p).expect("a path it sees");
                    reads.insert((format!("{c:?}"), p), read);
                }
            }
            18 => {
                if rng.below(3) == 0
                    && let Some(c) = rng.pick(&clients)
                    && let Some(p) = path(w, &c, f)
                {
                    tally.unlinks += 1;
                    w.unlink(&c, &p);
                }
            }
            19 => {
                if rng.below(3) == 0
                    && let Some(c) = rng.pick(&clients)
                    && let Some(p) = path(w, &c, f)
                {
                    // A third of renames go over another file (rename-over).
                    let over = w
                        .client_paths(&c)
                        .into_iter()
                        .filter(|g| *g != p)
                        .collect::<Vec<_>>();
                    match rng.pick(&over) {
                        Some(q) if rng.below(3) == 0 => {
                            tally.rename_overs += 1;
                            w.rename(&c, &p, &q);
                        }
                        _ => {
                            tally.renames += 1;
                            w.rename(&c, &p, &format!("e{next_name}"));
                            next_name += 1;
                        }
                    }
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
    // Writes stop: every open file closes, every open transaction commits
    // or rolls back; the queues drain; every fork equals main.
    close_all_checked(w, &mut written).map_err(|e| (STEPS, e))?;
    for f in in_txn(w) {
        if rng.below(2) == 0 {
            // A commit first finishes the sandbox's take, which may grant
            // an open of a file the commit writes: close it too.
            if w.commit_txn(f) != Err(ModelError::OpenFiles) {
                continue;
            }
            close_all(w);
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
    tally.rebased += w.rebased;
    tally.rebased_empty += w.rebased_empty;
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

/// The model's choices: full lineage, git's hunks. Every invariant.
#[test]
fn random_interleavings_full_lineage() {
    run_all(Policies::new(
        HistoryPolicy::FullLineage,
        CollisionScope::Hunk,
    ));
}

/// `Squashed`, every invariant: a fork's waiting saves land as one
/// changeset, main's stored history holds only main's commits, and the
/// graft beside it keeps the lineage a sync reads.
#[test]
fn random_interleavings_squashed() {
    run_all(Policies::new(HistoryPolicy::Squashed, CollisionScope::Hunk));
}

/// Rule 2, "no clock is read anywhere", on the random runs: seeds 1..=8
/// under each history setting run again with every commit dated before its
/// parents decide exactly the same trees and collisions.
#[test]
fn random_interleavings_commit_dates_decide_nothing() {
    for history in [HistoryPolicy::FullLineage, HistoryPolicy::Squashed] {
        let policies = Policies::new(history, CollisionScope::Hunk);
        for seed in seeds().into_iter().take(8) {
            let mut tally = Tally::default();
            let pinned = run_seed(seed, policies, &mut tally);
            let reversed = with_reversed_dates(|| run_seed(seed, policies, &mut tally));
            assert_eq!(pinned, reversed, "{history:?} seed {seed}");
            assert!(pinned.is_ok(), "{history:?} seed {seed}: {pinned:?}");
        }
    }
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
