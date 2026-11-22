//! The compactor's acceptance tests: a compaction parked at each of its gaps,
//! the state area copied there (a kill), and what a reopen reads; the switch
//! record and the step heads (docs/trees.md, "Recording and durability").
//!
//! A bound on a wait only fails a test that would hang: every wait is on an
//! event.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use agentfs_contracts::pod::PodContext;
use git2::Oid;

use super::{COMPACT_FROM, compact_at, compact_once};
use crate::tree_actor::history::built_log::{self, BuiltLog};
use crate::tree_actor::history::refs;
use crate::tree_actor::journal::compaction::{
    ChannelGate, CompactionError, Gap, JournalCompaction, checkpoint_in,
};
use crate::tree_actor::journal::durable::RecordNo;
use crate::tree_actor::journal::{
    Change, DropStats, Entry, Frame, Journal, Switched, Target, fold, segment,
};
use crate::tree_actor::meta::{SetTime, Times};
use crate::tree_actor::rows::{Chain, RowEvent, RowWrite};
use crate::tree_actor::test_support::{
    ATTRS, compact_now, never_due, new_actor_with, reopen_with, rowless_fork, same_pointers, writer,
};
use crate::tree_actor::{BUILT_LOG, History, JOURNAL, TreeActor, TreeId, Upkeep};

const ROOT: TreeId = TreeId::Root;
const BOUND: Duration = Duration::from_secs(10);
const ROOT_BRANCH: &str = "refs/heads/root";

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}

/// A kill: the state area as it is now, in a directory of its own.
fn killed(dir: &Path) -> tempfile::TempDir {
    let copy = tempfile::tempdir().unwrap();
    copy_dir(dir, copy.path());
    copy
}

/// Saves `bytes` as `path`: its save's step.
fn put(v: &mut TreeActor, tree: TreeId, path: &str, bytes: &[u8]) -> RecordNo {
    let h = v.write_open(tree, path, &writer("w"), Some(ATTRS)).unwrap();
    v.truncate(h, 0).unwrap();
    v.write(h, 0, bytes, false).unwrap();
    v.close(h).unwrap();
    v.durability().appended()
}

fn row(chain: Chain, text: &str) -> RowWrite {
    RowWrite {
        chain,
        row: text.to_owned(),
        event: RowEvent {
            kind: "VolumeCreated",
            detail: String::new(),
            by: "agentfs".to_owned(),
            at: PodContext::default(),
        },
    }
}

/// One rows-only step on the volume's chain.
fn put_row(v: &mut TreeActor, text: &str) -> RecordNo {
    v.record_rows(|_| Ok(vec![row(Chain::Volume, text)]))
        .unwrap()
}

/// Every file of every tree, and every row.
type Reads = (BTreeMap<(String, String), Vec<u8>>, Vec<(Chain, String)>);

fn reads(v: &TreeActor) -> Reads {
    let mut files = BTreeMap::new();
    for tree in v.trees() {
        for (path, _, _) in v.files(tree).unwrap() {
            let bytes = v.read(tree, &path).unwrap();
            files.insert((tree.to_string(), path), bytes);
        }
    }
    let rows = v.rows(crate::tree_actor::rows::RowDomain::Volume);
    (files, rows)
}

fn frames_of(root: &Path) -> Vec<Frame> {
    Journal::open(&root.join(JOURNAL), RecordNo::ZERO)
        .unwrap()
        .1
}

/// Every switch record of the journal in `root`, in order.
fn switches(root: &Path) -> Vec<(RecordNo, Vec<Switched>)> {
    frames_of(root)
        .into_iter()
        .filter_map(|(record, _)| match record.entry {
            Entry::Switch { pointers } => Some((record.seq, pointers)),
            _ => None,
        })
        .collect()
}

fn step_of(v: &TreeActor, name: &str) -> RecordNo {
    match v.pointers.get(name) {
        Some(Target::Step(step)) => step,
        other => panic!("{name} is {other:?}, not a step"),
    }
}

fn oid_of(v: &TreeActor, name: &str) -> String {
    match v.pointers.get(name) {
        Some(Target::Oid(oid)) => oid,
        other => panic!("{name} is {other:?}, not an oid"),
    }
}

/// The volume in `dir` opens: its journal folds as it was left, and with
/// the closes recovery appends for its live opens, the fold gives exactly
/// the pointers the actor serves.
fn opened(dir: &Path) -> TreeActor {
    fold(&frames_of(dir)).unwrap();
    let v = reopen_with(dir, never_due());
    same_pointers(&fold(&frames_of(dir)).unwrap().pointers, &v);
    v
}

/// Waits, with no call, until the built point reaches the last record.
fn at_rest(history: &History, v: &TreeActor) {
    let deadline = std::time::Instant::now() + BOUND;
    let mut epoch = 0;
    while history.built_through() < v.durability().appended() {
        assert!(
            std::time::Instant::now() < deadline,
            "at rest built through {:?} of {:?}",
            history.built_through(),
            v.durability().appended()
        );
        epoch = history.wait_epoch_for(epoch, Duration::from_millis(100));
    }
}

/// A volume's compaction gate in a test's hands.
struct Parked {
    reached: Receiver<Gap>,
    verdicts: Sender<io::Result<()>>,
}

impl Parked {
    fn pair() -> (Arc<ChannelGate>, Self) {
        let (gate, reached, verdicts) = ChannelGate::pair();
        (gate, Self { reached, verdicts })
    }

    /// The next gap the compactor reaches, where it stays parked.
    fn next(&self) -> Gap {
        self.reached
            .recv_timeout(BOUND)
            .expect("the compactor reaches a gap")
    }

    fn go(&self) {
        self.verdicts.send(Ok(())).unwrap();
    }

    fn fail(&self) {
        self.verdicts
            .send(Err(io::Error::other("a test's fault")))
            .unwrap();
    }

    /// Answers `go` at each gap until the compactor is parked at `gap`.
    fn until(&self, gap: Gap) {
        while self.next() != gap {
            self.go();
        }
    }

    /// Ends the gate: every gap from now on passes.
    fn end(self) {}
}

/// A new volume whose compactor parks at `parked`, which is dropped first:
/// a failed assertion ends the gate before the volume's shutdown joins its
/// compactor.
fn gated(dir: &Path, upkeep: impl FnOnce(Arc<ChannelGate>) -> Upkeep) -> (TreeActor, Parked) {
    let (gate, parked) = Parked::pair();
    (new_actor_with(dir, upkeep(gate)), parked)
}

/// Holds the builder: each batch parks at its start until released.
struct Held {
    parked: Receiver<()>,
    release: Sender<()>,
}

fn hold_builder(history: &History) -> Held {
    let (at, parked) = mpsc::channel();
    let (release, gate) = mpsc::channel::<()>();
    history.on_batch_read(move |read| {
        if read.is_none() {
            let _ = at.send(());
            let _ = gate.recv();
        }
    });
    Held { parked, release }
}

impl Held {
    fn parked(&self) {
        self.parked.recv_timeout(BOUND).expect("a batch starts");
    }

    fn release(self, history: &History) {
        history.on_batch_read(|_| {});
        drop(self.release);
    }
}

/// One compaction of `v` on a thread of its own, through `gate`.
fn compaction(
    v: &TreeActor,
    gate: Arc<ChannelGate>,
) -> std::thread::JoinHandle<Result<Option<DropStats>, CompactionError>> {
    let journal = JournalCompaction::new(
        v.journal.dir().to_owned(),
        Arc::clone(v.durability()),
        Some(gate),
    );
    let history = v.history().clone();
    std::thread::spawn(move || compact_once(&history, &journal))
}

#[test]
fn a_compaction_threshold_is_constants() {
    assert_eq!(compact_at(COMPACT_FROM, 0), 64 << 20);
    assert_eq!(compact_at(COMPACT_FROM, 10 << 20), 64 << 20);
    assert_eq!(compact_at(COMPACT_FROM, 100 << 20), 200 << 20);
    assert_eq!(compact_at(4096, 0), 4096);
    assert_eq!(compact_at(4096, 4096), 8192);
}

/// 2.1: a kill at any gap of a compaction loses nothing returned, and the
/// volume opens with the same reads. While the compactor is parked before
/// its checkpoint is in place, History still reports the last published
/// one (C3), so the builder's batch trims nothing a restart needs.
#[test]
fn a_kill_at_each_gap_keeps_every_returned_call() {
    for gap in Gap::ALL {
        let tmp = tempfile::tempdir().unwrap();
        let (mut v, parked) = gated(tmp.path(), |gate| Upkeep::gated(gate));
        let before = (
            v.history().checkpointed(),
            v.history().checkpointed_through(),
        );
        put(&mut v, ROOT, "a", b"one");
        parked.until(gap);
        let counted = (
            v.history().checkpointed(),
            v.history().checkpointed_through(),
        );
        // A step that sets a pointer, and its built event: a builder batch
        // runs while the compactor is parked, and trims at its start.
        put(&mut v, ROOT, "b", b"two");
        v.settle().unwrap();
        if matches!(gap, Gap::Covered | Gap::Written | Gap::Renamed) {
            assert_eq!(counted, before, "{gap:?}: counted before it is in place");
            assert_eq!(
                (
                    v.history().checkpointed(),
                    v.history().checkpointed_through()
                ),
                before,
                "{gap:?}"
            );
        }
        let copy = killed(tmp.path());
        parked.end();
        let c = opened(copy.path());
        assert_eq!(reads(&c), reads(&v), "{gap:?}");
        assert_eq!(c.head(ROOT).unwrap(), v.head(ROOT).unwrap(), "{gap:?}");
    }
}

/// 2.2: a segment is unlinked only once the next segment starts at or
/// before B + 1. A call appended to the newest segment while the compactor
/// is past its coverage check survives the compaction and a kill.
#[test]
fn an_append_during_the_coverage_gap_survives_a_kill() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut v, parked) = gated(tmp.path(), |gate| Upkeep {
        gate: Some(gate),
        ..Upkeep::default()
    });
    put(&mut v, ROOT, "a", b"one");
    parked.until(Gap::Covered);
    // The newest segment starts at or before B: the call lands in it.
    assert_eq!(v.journal.segment_firsts().len(), 1);
    let generation = v.history().checkpointed().generation;
    let h = v.write_open(ROOT, "b", &writer("w"), Some(ATTRS)).unwrap();
    v.write(h, 0, b"returned", false).unwrap();
    v.durability().make_all_durable().unwrap();
    // One compaction, to its end.
    for _ in 0..4 {
        parked.go();
        parked.next();
    }
    parked.go();
    v.history().wait_checkpointed(generation).unwrap();
    v.settle().unwrap();
    let copy = killed(tmp.path());
    parked.end();
    let c = opened(copy.path());
    assert_eq!(c.read(ROOT, "b").unwrap(), b"returned");
    assert_eq!(c.read(ROOT, "a").unwrap(), b"one");
}

/// 2.3: the journal opens when nothing after B was durable: no segment
/// left, and a next segment whose first frame never reached the disk.
#[test]
fn a_checkpoint_with_no_segment_after_it_opens() {
    for torn in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = new_actor_with(tmp.path(), never_due());
        put(&mut v, ROOT, "a", b"one");
        compact_now(&v);
        let expected = reads(&v);
        let head = v.head(ROOT).unwrap();
        drop(v);
        let journal = tmp.path().join(JOURNAL);
        let built = checkpoint_in(&journal).built;
        for entry in fs::read_dir(&journal).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "seg") {
                fs::remove_file(&path).unwrap();
            }
        }
        if torn {
            // The next segment, its first frame never durable: a few bytes.
            fs::write(journal.join(segment::name_of(built.next())), b"AFJ3\0\0\0").unwrap();
        }
        let mut v = opened(tmp.path());
        assert_eq!(reads(&v), expected, "torn: {torn}");
        assert_eq!(v.head(ROOT).unwrap(), head);
        // The next record is past the checkpoint's point.
        assert!(put(&mut v, ROOT, "b", b"two") > built);
        drop(v);
        assert_eq!(opened(tmp.path()).read(ROOT, "b").unwrap(), b"two");
    }
}

/// Which pointer a test moves: the root's branch, by a save, or the
/// volume's row chain, by a rows step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Branch,
    Chain,
}

impl Kind {
    fn name(self) -> String {
        match self {
            Kind::Branch => ROOT_BRANCH.to_owned(),
            Kind::Chain => refs::row_chain(Chain::Volume),
        }
    }

    /// One step on the pointer.
    fn step(self, v: &mut TreeActor, text: &str) -> RecordNo {
        match self {
            Kind::Branch => put(v, ROOT, "a", text.as_bytes()),
            Kind::Chain => put_row(v, text),
        }
    }
}

/// 2.4: a record after B that names a step at or below B folds the same on
/// the old checkpoint and on the new one. The compactor is parked with its
/// checkpoint at B written and not yet in place, so a copy there reopens at
/// the old checkpoint, and a copy after it at the new one.
#[test]
fn a_step_named_after_the_checkpoint_folds_on_both() {
    for kind in [Kind::Branch, Kind::Chain] {
        for switch_first in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let mut v = new_actor_with(tmp.path(), never_due());
            kind.step(&mut v, "one");
            let built = v.settle().unwrap();
            let s = step_of(&v, &kind.name());
            // Compacted at B before the actor acts on that built point.
            let (gate, parked) = Parked::pair();
            let compacting = compaction(&v, gate);
            parked.until(Gap::Written);
            if switch_first {
                // The first record after B is the switch record.
                v.on_built();
                let (at, named) = switches(tmp.path()).pop().expect("a switch record");
                assert_eq!(at, built.next(), "{kind:?}");
                assert!(
                    named.iter().any(|p| p.name == kind.name() && p.step == s),
                    "{named:?}"
                );
                // This step names the oid.
                kind.step(&mut v, "two");
            } else {
                // This step names `Step(s)` as its `old`.
                kind.step(&mut v, "two");
                assert!(switches(tmp.path()).is_empty(), "{kind:?}");
            }
            v.settle().unwrap();
            let expected = reads(&v);
            let head = v.head(ROOT).unwrap();
            let old = killed(tmp.path());
            assert!(checkpoint_in(&old.path().join(JOURNAL)).built < s);
            parked.end();
            compacting.join().unwrap().unwrap().expect("compacts");
            let new = killed(tmp.path());
            let checkpoint = checkpoint_in(&new.path().join(JOURNAL));
            assert_eq!(checkpoint.built, built);
            assert_eq!(
                checkpoint.step_heads.get(&kind.name()),
                Some(&s),
                "{kind:?}"
            );
            for copy in [&old, &new] {
                let c = opened(copy.path());
                assert_eq!(reads(&c), expected, "{kind:?} {switch_first}");
                assert_eq!(c.head(ROOT).unwrap(), head);
            }
        }
    }
}

/// 2.4, a fork's branch: a checkpoint at a B whose only head is the fork's
/// branch, set by the fork step to its upstream's step, then the fork's
/// first save, which names that step as its `old`.
#[test]
fn a_forks_branch_is_the_only_head_and_its_first_save_folds() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor_with(tmp.path(), never_due());
    put(&mut v, ROOT, "a", b"one");
    v.settle().unwrap();
    let s = step_of(&v, ROOT_BRANCH);
    // The fork step switches the root alone: the fork is not built yet.
    let fork = rowless_fork(&mut v);
    let branch = refs::branch(fork);
    assert_eq!(step_of(&v, &branch), s);
    v.settle().unwrap();
    compact_now(&v);
    let checkpoint = checkpoint_in(&tmp.path().join(JOURNAL));
    assert_eq!(
        checkpoint.step_heads,
        BTreeMap::from([(branch.clone(), s)]),
        "the fork's branch alone"
    );
    put(&mut v, fork, "a", b"sandbox");
    let expected = reads(&v);
    let (root_head, fork_head) = (v.head(ROOT).unwrap(), v.head(fork).unwrap());
    let copy = killed(tmp.path());
    drop(v);
    for dir in [copy.path(), tmp.path()] {
        let c = opened(dir);
        assert_eq!(reads(&c), expected);
        assert_eq!(c.head(ROOT).unwrap(), root_head);
        assert_eq!(c.head(fork).unwrap(), fork_head);
    }
}

/// 2.5: no lock the actor takes is held across I/O or a wait. Parked at
/// each gap, another thread appends a call, makes it durable and catches up.
#[test]
fn calls_run_while_the_compactor_is_parked() {
    for gap in Gap::ALL {
        let tmp = tempfile::tempdir().unwrap();
        let (mut v, parked) = gated(tmp.path(), |gate| Upkeep::gated(gate));
        put(&mut v, ROOT, "a", b"one");
        parked.until(gap);
        let (done, finished) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let h = v.write_open(ROOT, "b", &writer("w"), Some(ATTRS)).unwrap();
            v.write(h, 0, b"a call", false).unwrap();
            v.durability().make_all_durable().unwrap();
            v.close(h).unwrap();
            v.settle().unwrap();
            v.on_built();
            let _ = done.send(());
            v
        });
        let ran = finished.recv_timeout(BOUND);
        parked.end();
        let v = worker.join().unwrap();
        assert!(ran.is_ok(), "a call waited for the compactor at {gap:?}");
        assert_eq!(v.read(ROOT, "b").unwrap(), b"a call");
    }
}

/// 2.8: new work is admitted while history trails past twice the due
/// length. The overdue stop is gone: only a tree's own lag holds it.
#[cfg(feature = "failpoints")]
#[test]
fn creates_go_on_while_history_trails() {
    let tmp = tempfile::tempdir().unwrap();
    let from = 4096;
    let upkeep = Upkeep {
        segment_bytes: 1024,
        compact_from: from,
        gate: None,
    };
    let mut v = new_actor_with(tmp.path(), upkeep);
    v.settle().unwrap();
    let history = v.history().clone();
    let held = hold_builder(&history);
    let mut n = 0;
    while v.durability().len() <= 2 * compact_at(from, 0) {
        put(&mut v, ROOT, &format!("f{n}"), &[b'x'; 2048]);
        n += 1;
    }
    held.parked();
    assert!(history.built_through() < v.durability().appended());
    assert!(history.admits(ROOT), "held by the journal's length");
    put(&mut v, ROOT, "more", b"admitted");
    assert_eq!(v.read(ROOT, "more").unwrap(), b"admitted");
    held.release(&history);
    v.settle().unwrap();
}

/// 2.11: a head goes at the first checkpoint at or past its switch record,
/// and the built log is trimmed past it.
#[test]
fn a_switched_head_is_let_go() {
    for kind in [Kind::Branch, Kind::Chain] {
        let tmp = tempfile::tempdir().unwrap();
        let journal = tmp.path().join(JOURNAL);
        let mut v = new_actor_with(tmp.path(), never_due());
        kind.step(&mut v, "one");
        v.settle().unwrap();
        let s = step_of(&v, &kind.name());
        compact_now(&v);
        assert_eq!(
            checkpoint_in(&journal).step_heads.get(&kind.name()),
            Some(&s)
        );
        assert!(v.history().checkpointed_through() < s, "the head is kept");
        // The switch record at r > B, built, and a compaction at B2 >= r,
        // with no step on the pointer since.
        v.on_built();
        let (r, _) = switches(tmp.path()).pop().expect("a switch record");
        let oid = oid_of(&v, &kind.name());
        compact_now(&v);
        let checkpoint = checkpoint_in(&journal);
        assert!(checkpoint.built >= r);
        assert_eq!(checkpoint.step_heads, BTreeMap::new(), "{kind:?}");
        assert_eq!(checkpoint.pointers.get(&kind.name()), Some(&oid));
        assert_eq!(v.history().checkpointed_through(), checkpoint.built);
        // The next batch trims the built log.
        put(&mut v, ROOT, "other", b"a batch");
        v.settle().unwrap();
        let (_, entries) = BuiltLog::open(&tmp.path().join(BUILT_LOG)).unwrap();
        assert!(
            entries.iter().all(|entry| entry.through > checkpoint.built),
            "{kind:?}: the built log keeps {:?}",
            entries.iter().map(|e| e.through).collect::<Vec<_>>()
        );
        assert!(
            !built_log::values(&entries)
                .unwrap()
                .contains_key(&(s, kind.name()))
        );
        let expected = reads(&v);
        drop(v);
        // A reopen reads the pointer's oid from the checkpoint.
        let c = opened(tmp.path());
        assert_eq!(reads(&c), expected);
        if kind == Kind::Chain {
            assert_eq!(oid_of(&c, &kind.name()), oid);
        }
    }
}

/// 2.11, a queue entry: queued at step q, switched once q is built, then
/// deleted by the landing that takes it, whose record names its oid.
#[test]
fn a_switched_queue_entry_is_deleted_by_its_landing() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor_with(tmp.path(), never_due());
    put(&mut v, ROOT, "a", b"base");
    let fork = rowless_fork(&mut v);
    put(&mut v, fork, "a", b"sandbox");
    let queue = refs::queue(ROOT);
    let (entry, q) = match v.pointers.under(&queue).pop() {
        Some((entry, Target::Step(q))) => (entry, q),
        other => panic!("the queue holds {other:?}"),
    };
    v.settle().unwrap();
    v.on_built();
    let oid = oid_of(&v, &entry);
    assert!(
        switches(tmp.path())
            .iter()
            .any(|(_, named)| named.iter().any(|p| p.name == entry && p.step == q)),
        "the queue entry is switched"
    );
    while v.progress().unwrap() {}
    assert!(v.pointers.under(&queue).is_empty(), "the entry is taken");
    let landed = frames_of(tmp.path()).into_iter().any(|(record, _)| {
        matches!(&record.entry, Entry::Step { pointers, .. }
            if pointers.iter().any(|p| p.name == entry
                && p.new.is_none()
                && p.old == Some(Target::Oid(oid.clone()))))
    });
    assert!(landed, "the landing names the entry's oid");
    assert_eq!(v.read(ROOT, "a").unwrap(), b"sandbox");
    let expected = reads(&v);
    let copy = killed(tmp.path());
    drop(v);
    for dir in [copy.path(), tmp.path()] {
        assert_eq!(reads(&opened(dir)), expected);
    }
    // Built past the landing, no head is left.
    let v = opened(tmp.path());
    v.settle().unwrap();
    compact_now(&v);
    let mut v = v;
    v.on_built();
    compact_now(&v);
    assert_eq!(
        checkpoint_in(&tmp.path().join(JOURNAL)).step_heads,
        BTreeMap::new()
    );
}

/// Each named entry's number and times, directories and files.
fn nodes(v: &TreeActor, paths: &[&str]) -> Vec<(String, Option<u64>, Times)> {
    paths
        .iter()
        .map(|path| {
            let node = v.node(ROOT, path).unwrap();
            ((*path).to_owned(), node.number, node.times)
        })
        .collect()
}

/// 2.12: no checkpoint is published without History's trees at B. A
/// compaction after a reopen keeps every directory's number and time and
/// every file's time.
#[test]
fn a_compaction_after_reopen_keeps_dir_and_file_times() {
    const PATHS: [&str; 6] = ["", "d1", "d1/f", "d2", "d2/g", "top"];
    let tmp = tempfile::tempdir().unwrap();
    let w = writer("w");
    let mut v = new_actor_with(tmp.path(), never_due());
    v.mkdir(ROOT, "d1", ATTRS, &w).unwrap();
    put(&mut v, ROOT, "d1/f", b"f");
    v.mkdir(ROOT, "d2", ATTRS, &w).unwrap();
    put(&mut v, ROOT, "d2/g", b"g");
    put(&mut v, ROOT, "top", b"top");
    for (n, path) in ["d1", "d1/f", "d2", "top"].iter().enumerate() {
        let at = Some(SetTime::At(1_000_000 * (n as u64 + 1)));
        v.set_times(ROOT, path, at, at, &w).unwrap();
    }
    let before = nodes(&v, &PATHS);
    let distinct: std::collections::BTreeSet<u64> =
        before.iter().map(|(_, _, times)| times.mtime_ns).collect();
    assert_eq!(distinct.len(), PATHS.len(), "each entry has its own time");
    drop(v);
    // Open does not compact: History has no trees before its first built event.
    let mut v = reopen_with(tmp.path(), never_due());
    assert!(v.history().at_built().is_none());
    assert_eq!(nodes(&v, &PATHS), before);
    put(&mut v, ROOT, "d2/g", b"one step");
    let uninterrupted = nodes(&v, &PATHS);
    compact_now(&v);
    assert!(checkpoint_in(&tmp.path().join(JOURNAL)).built > RecordNo::ZERO);
    drop(v);
    let v = opened(tmp.path());
    assert_eq!(nodes(&v, &PATHS), uninterrupted);
}

/// A volume whose segments hold several frames, never due on its own: a
/// held open's few calls beside another file's dead ones.
fn carrying() -> Upkeep {
    Upkeep {
        segment_bytes: 64 << 10,
        compact_from: u64::MAX,
        gate: None,
    }
}

/// An open with three calls held while another file's saved calls outweigh
/// them in its segment, and the journal rolled past it: its handle.
fn held_open(v: &mut TreeActor) -> u64 {
    let held = v
        .write_open(ROOT, "held", &writer("w"), Some(ATTRS))
        .unwrap();
    for _ in 0..3 {
        v.write(held, 0, b"abc", true).unwrap();
    }
    put(v, ROOT, "dead", &[b'd'; 40 << 10]);
    put(v, ROOT, "rolls", &[b'r'; 80 << 10]);
    put(v, ROOT, "rolls", b"again");
    assert!(v.journal.segment_firsts().len() > 1, "the journal rolled");
    held
}

/// 2.14: a carried open survives every gap, and the actor moves to the
/// carry. Each of the open's bytes is read once, with the unlink failing too.
#[test]
fn a_carried_open_survives_each_gap() {
    let once = |dir: &Path| {
        let c = opened(dir);
        assert_eq!(c.read(ROOT, "held").unwrap(), b"abcabcabc");
        assert_eq!(c.read(ROOT, "rolls").unwrap(), b"again");
    };
    for gap in Gap::ALL {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = new_actor_with(tmp.path(), carrying());
        let held = held_open(&mut v);
        v.settle().unwrap();
        let (gate, parked) = Parked::pair();
        let compacting = compaction(&v, gate);
        parked.until(gap);
        let copy = killed(tmp.path());
        parked.end();
        let stats = compacting.join().unwrap().unwrap().expect("compacts");
        assert!(stats.carried_bytes > 0, "{gap:?}: nothing was carried");
        assert!(stats.segments > 0, "{gap:?}: no segment was let go");
        once(copy.path());
        // The actor moves to the carry at its next built event, and the
        // builder at its next batch.
        let generation = v.durability().relocations().generation();
        assert!(generation > v.relocated, "{gap:?}");
        v.on_built();
        assert_eq!(v.relocated, generation, "{gap:?}");
        v.write(held, 0, b"!", true).unwrap();
        v.close(held).unwrap();
        v.settle().unwrap();
        assert_eq!(v.read(ROOT, "held").unwrap(), b"abcabcabc!");
        drop(v);
        assert_eq!(
            opened(tmp.path()).read(ROOT, "held").unwrap(),
            b"abcabcabc!"
        );
    }
    // The unlink fails: the checkpoint stands, the covered segment stays,
    // and a frame and its carried copy are both on disk.
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor_with(tmp.path(), carrying());
    let held = held_open(&mut v);
    v.settle().unwrap();
    let segments = v.journal.segment_firsts().len();
    let (gate, parked) = Parked::pair();
    let compacting = compaction(&v, gate);
    parked.until(Gap::BeforeUnlink);
    parked.fail();
    assert!(matches!(
        compacting.join().unwrap(),
        Err(CompactionError::NotDropped(_))
    ));
    assert_eq!(v.journal.segment_firsts().len(), segments);
    once(killed(tmp.path()).path());
    // The next compaction lets the segment go, and once the actor and the
    // builder have moved, nothing holds it.
    v.on_built();
    put(&mut v, ROOT, "next", b"a built event");
    compact_now_carrying(&v);
    v.on_built();
    put(&mut v, ROOT, "next", b"the builder moves");
    v.settle().unwrap();
    assert!(v.journal.segment_firsts().len() < segments);
    v.close(held).unwrap();
    assert_eq!(v.read(ROOT, "held").unwrap(), b"abcabcabc");
    drop(v);
    once(tmp.path());
}

/// [`compact_now`] on a volume of any upkeep.
fn compact_now_carrying(v: &TreeActor) {
    v.settle().unwrap();
    let journal =
        JournalCompaction::new(v.journal.dir().to_owned(), Arc::clone(v.durability()), None);
    compact_once(v.history(), &journal).unwrap();
}

/// What appends the switch record once the first save is built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum By {
    /// The built event.
    OnBuilt,
    /// A rows-only step, whose record the switch record follows.
    Step,
    /// Nothing: the next save finds the branch at its own unbuilt step.
    Nothing,
}

/// Where 2.15 stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    AfterSwitch,
    AfterSecondSave,
}

/// One run of 2.15: `kind`'s pointer set at step s and built, the switch
/// appended by `by`, then a second step on the pointer. At `stop`: a kill
/// with the last record unbuilt, a kill at rest, and a clean shutdown.
fn reopens_after_a_switch(kind: Kind, by: By, stop: Stop) {
    let what = format!("{kind:?} {by:?} {stop:?}");
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor_with(tmp.path(), never_due());
    let history = v.history().clone();
    kind.step(&mut v, "one");
    v.settle().unwrap();
    let s = step_of(&v, &kind.name());
    let built = history.snapshot().pointers().get(&kind.name()).unwrap();
    // The builder is held from here: the last record of each copy below is
    // not built.
    let held = hold_builder(&history);
    let mut last_is_unbuilt = true;
    match by {
        By::OnBuilt => v.on_built(),
        By::Step => {
            put_row(&mut v, "a step");
        }
        By::Nothing => last_is_unbuilt = false,
    }
    let named: Vec<(RecordNo, Vec<Switched>)> = switches(tmp.path());
    if by == By::Nothing {
        assert!(named.is_empty(), "{what}: {named:?}");
    } else {
        let (at, pointers) = named.last().expect("a switch record");
        assert_eq!(*at, v.durability().appended(), "{what}: the last frame");
        let switched = Switched {
            name: kind.name(),
            step: s,
            oid: built.to_string(),
        };
        assert!(pointers.contains(&switched), "{what}: {pointers:?}");
        assert_eq!(oid_of(&v, &kind.name()), built.to_string());
    }
    if stop == Stop::AfterSecondSave {
        if last_is_unbuilt {
            // The switch record's batch, then the save's.
            let through = v.durability().appended();
            held.parked();
            held.release.send(()).unwrap();
            history.wait_built(through).unwrap();
        }
        let before = v.pointers.get(&kind.name());
        kind.step(&mut v, "two");
        last_is_unbuilt = true;
        // The record that names the pointer's value as its `old`.
        let names_old = frames_of(tmp.path()).into_iter().any(|(record, _)| {
            matches!(&record.entry, Entry::Step { pointers, .. }
                if pointers.iter().any(|p| p.name == kind.name() && p.old == before))
        });
        assert!(names_old, "{what}");
        if by == By::Nothing {
            // The save saw the new built point with its pointer already at
            // its own unbuilt step: nothing is switched for it.
            assert!(
                switches(tmp.path())
                    .iter()
                    .all(|(_, named)| named.iter().all(|p| p.name != kind.name())),
                "{what}"
            );
            assert!(matches!(v.pointers.get(&kind.name()), Some(Target::Step(step)) if step > s));
        }
    }
    // A kill with the builder held at its batch start.
    let unbuilt = if last_is_unbuilt {
        held.parked();
        assert!(history.built_through() < v.durability().appended());
        Some(killed(tmp.path()))
    } else {
        None
    };
    held.release(&history);
    // Every switch record is asked of the builder: with no further call the
    // built point reaches the last record.
    at_rest(&history, &v);
    let expected = reads(&v);
    let head = v.head(ROOT).unwrap();
    let rested = killed(tmp.path());
    // The built event switches what the last batch built, once: a second
    // one with no build between appends nothing.
    let appended = v.durability().appended();
    v.on_built();
    if by == By::OnBuilt && stop == Stop::AfterSwitch {
        assert_eq!(v.durability().appended(), appended, "{what}");
    }
    at_rest(&history, &v);
    let appended = v.durability().appended();
    v.on_built();
    assert_eq!(v.durability().appended(), appended, "{what}");
    // The lease goes with the last holder of History.
    drop((v, history));
    for dir in unbuilt
        .iter()
        .map(tempfile::TempDir::path)
        .chain([rested.path(), tmp.path()])
    {
        let c = opened(dir);
        // The reopen case: a copy whose last record is a switch record the
        // builder never built reaches it with no call.
        at_rest(c.history(), &c);
        assert_eq!(reads(&c), expected, "{what}");
        assert_eq!(c.head(ROOT).unwrap(), head, "{what}");
    }
}

/// 2.15: a volume reopens after a switch and before the next checkpoint,
/// after a clean shutdown and after a kill. Its only checkpoint is the one
/// it was created with.
#[test]
fn a_volume_reopens_after_a_switch_before_the_next_checkpoint() {
    for by in [By::OnBuilt, By::Step, By::Nothing] {
        for stop in [Stop::AfterSwitch, Stop::AfterSecondSave] {
            reopens_after_a_switch(Kind::Branch, by, stop);
        }
    }
}

/// 2.16: the same for a row chain, and once built past the switch record a
/// checkpoint holds no row chain.
#[test]
fn a_row_chain_reopens_after_its_switch() {
    for stop in [Stop::AfterSwitch, Stop::AfterSecondSave] {
        reopens_after_a_switch(Kind::Chain, By::OnBuilt, stop);
    }
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor_with(tmp.path(), never_due());
    put_row(&mut v, "one");
    v.settle().unwrap();
    v.on_built();
    put_row(&mut v, "two");
    v.settle().unwrap();
    v.on_built();
    compact_now(&v);
    let checkpoint = checkpoint_in(&tmp.path().join(JOURNAL));
    assert_eq!(checkpoint.step_heads, BTreeMap::new());
    drop(v);
    let c = opened(tmp.path());
    assert_eq!(
        c.rows(crate::tree_actor::rows::RowDomain::Volume),
        [(Chain::Volume, "two".to_owned())]
    );
}

/// Every entry's times in every tree.
fn times(v: &TreeActor) -> BTreeMap<(String, String), Times> {
    let mut all = BTreeMap::new();
    for tree in v.trees() {
        let mut paths = vec![String::new()];
        paths.extend(v.files(tree).unwrap().into_iter().map(|(path, _, _)| path));
        for path in paths {
            let node = v.node(tree, &path).unwrap();
            all.insert((tree.to_string(), path), node.times);
        }
    }
    all
}

/// The live volume's times are the reopened copy's, and its own once its
/// saved trees rest on the builder's.
fn times_hold(v: &mut TreeActor, dir: &Path, what: &str) {
    let live = times(v);
    let copy = killed(dir);
    let c = opened(copy.path());
    assert_eq!(times(&c), live, "{what}: a reopened copy's times");
    v.settle().unwrap();
    v.on_built();
    assert_eq!(times(v), live, "{what}: the times once built");
}

/// 2.15, a step keeps its own time: a step whose record the switch record
/// follows is applied with the step's time, on the live volume as on a
/// reopen and by the builder.
#[test]
fn a_step_with_a_switch_behind_it_keeps_its_own_time() {
    // A time-setting step.
    let tmp = tempfile::tempdir().unwrap();
    let w = writer("w");
    let mut v = new_actor_with(tmp.path(), never_due());
    put(&mut v, ROOT, "a", b"one");
    v.settle().unwrap();
    let before = switches(tmp.path()).len();
    v.set_times(ROOT, "a", Some(SetTime::Now), Some(SetTime::Now), &w)
        .unwrap();
    assert_eq!(switches(tmp.path()).len(), before + 1, "the step switched");
    times_hold(&mut v, tmp.path(), "set_times");
    // A landing take.
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor_with(tmp.path(), never_due());
    put(&mut v, ROOT, "a", b"base");
    let fork = rowless_fork(&mut v);
    put(&mut v, fork, "a", b"sandbox");
    put(&mut v, fork, "new", b"a new file");
    let before = switches(tmp.path()).len();
    while v.progress().unwrap() {}
    assert!(switches(tmp.path()).len() > before, "a take switched");
    let frames = frames_of(tmp.path());
    let behind_a_take = frames.windows(2).any(|pair| {
        matches!(
            &pair[0].0.entry,
            Entry::Step {
                change: Change::Take { .. },
                ..
            }
        ) && matches!(pair[1].0.entry, Entry::Switch { .. })
    });
    assert!(
        behind_a_take,
        "a switch record right behind a take's record"
    );
    assert_eq!(v.read(ROOT, "new").unwrap(), b"a new file");
    times_hold(&mut v, tmp.path(), "a landing take");
}

/// 2.15, the fork exception: a fork's branch is set to its upstream's step,
/// and is switched only once the fork step itself is built, by a second
/// switch record that names the upstream's step.
#[test]
fn a_forks_branch_is_switched_once_the_fork_is_built() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor_with(tmp.path(), never_due());
    let history = v.history().clone();
    put(&mut v, ROOT, "a", b"one");
    v.settle().unwrap();
    let s = step_of(&v, ROOT_BRANCH);
    // The fork is made with the builder held: f is not built.
    let held = hold_builder(&history);
    let fork = rowless_fork(&mut v);
    let branch = refs::branch(fork);
    v.on_built();
    held.parked();
    let first = switches(tmp.path());
    assert_eq!(first.len(), 1, "{first:?}");
    let names = |named: &[Switched]| -> Vec<(String, RecordNo)> {
        named.iter().map(|p| (p.name.clone(), p.step)).collect()
    };
    assert_eq!(
        names(&first[0].1),
        [(ROOT_BRANCH.to_owned(), s)],
        "the root alone"
    );
    assert_eq!(step_of(&v, &branch), s, "the fork's branch stays a step");
    let at_first = killed(tmp.path());
    held.release(&history);
    v.settle().unwrap();
    v.on_built();
    let second = switches(tmp.path());
    assert_eq!(second.len(), 2, "{second:?}");
    assert_eq!(names(&second[1].1), [(branch.clone(), s)]);
    assert_eq!(oid_of(&v, &branch), oid_of(&v, ROOT_BRANCH));
    at_rest(&history, &v);
    let expected = reads(&v);
    let heads = (v.head(ROOT).unwrap(), v.head(fork).unwrap());
    let at_second = killed(tmp.path());
    // The lease goes with the last holder of History.
    drop((v, history));
    for dir in [at_first.path(), at_second.path(), tmp.path()] {
        let c = opened(dir);
        assert_eq!(reads(&c), expected);
        assert_eq!((c.head(ROOT).unwrap(), c.head(fork).unwrap()), heads);
    }
}

/// A batch that read through a step before that step's switch record was
/// appended ends one record short of it: the step's want makes the next
/// batch follow, so the volume comes to rest built through its last record.
#[test]
fn a_batch_one_record_short_of_a_switch_record_is_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor_with(tmp.path(), never_due());
    let history = v.history().clone();
    put(&mut v, ROOT, "a", b"one");
    v.settle().unwrap();
    // The batch parks once it has read the record it builds through.
    let (read, was_read) = mpsc::channel::<RecordNo>();
    let (release, gate) = mpsc::channel::<()>();
    history.on_batch_read(move |through| {
        if let Some(through) = through {
            let _ = read.send(through);
            let _ = gate.recv();
        }
    });
    // Between the step's record and its switch record, another thread's
    // wait asks the builder through the step, and the batch reads it.
    let asking = history.clone();
    history.on_step_recorded(move |step| {
        asking.want_through(step);
        assert_eq!(was_read.recv_timeout(BOUND), Ok(step), "the batch's read");
    });
    let step = v
        .record_rows(|_| Ok(vec![row(Chain::Volume, "a step")]))
        .unwrap();
    history.on_step_recorded(|_| {});
    let (at, _) = switches(tmp.path()).pop().expect("a switch record");
    assert_eq!(at, step.next(), "the switch record follows the step");
    history.on_batch_read(|_| {});
    drop(release);
    at_rest(&history, &v);
    assert_eq!(history.built_through(), at);
}

/// The object a built pointer names.
#[allow(dead_code, reason = "a probe's aid")]
fn built_oid(history: &History, name: &str) -> Option<Oid> {
    history.snapshot().pointers().get(name)
}
