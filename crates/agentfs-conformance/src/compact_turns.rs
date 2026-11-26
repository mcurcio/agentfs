//! Compaction under a replay: the volume compacts at every built event, and
//! its compactor is parked at one gap at a time, each gap in turn, while one
//! whole replayed call runs (`crates/agentfs-model/VERIFYING.md`, "Phase 1").
//!
//! The library replay gives its volume [`Turns`] as its compaction gate. The
//! mounted replay answers the mount's FIFO gate (`AGENTFS_COMPACT_GATE`) from
//! a thread that asks the same [`Turns`]. Event-driven: the compactor moves
//! on at a replayed call's end, never at a time.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use agentfs_engine::tree_actor::{CompactGate, Gap};

/// The line that ends [`answer_fifos`]' thread, written to `at` by the
/// harness itself: no gap has this name.
const END: &str = "end";

#[derive(Default)]
struct Turn {
    /// The gap the compactor parks at next: an index into [`Gap::ALL`].
    gap: usize,
    /// Replayed calls ended so far.
    calls: u64,
    /// The replay is over, or its volume is about to go: nothing parks.
    ended: bool,
}

/// Whose turn it is to hold the compactor, shared by the replay and the gate.
#[derive(Default)]
pub struct Turns {
    turn: Mutex<Turn>,
    moved: Condvar,
}

impl Turns {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn turn(&self) -> MutexGuard<'_, Turn> {
        self.turn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One replayed call ended.
    pub fn call_ended(&self) {
        self.turn().calls += 1;
        self.moved.notify_all();
    }

    /// Ends the turns: a parked compactor goes on, and no gap parks again.
    /// Called before the volume is dropped or unmounted, so its shutdown's
    /// join of the compactor never waits on the gate.
    pub fn end(&self) {
        self.turn().ended = true;
        self.moved.notify_all();
    }

    /// The compactor reached `gap`. Returns at once unless it is this turn's
    /// gap; then once two replayed calls have ended, so one whole call ran
    /// while the compactor was parked, and the turn moves to the next gap.
    pub fn reached(&self, gap: Gap) {
        let mut turn = self.turn();
        if turn.ended || Gap::ALL[turn.gap] != gap {
            return;
        }
        let parked_at = turn.calls;
        while !turn.ended && turn.calls < parked_at + 2 {
            turn = self
                .moved
                .wait(turn)
                .unwrap_or_else(PoisonError::into_inner);
        }
        turn.gap = (turn.gap + 1) % Gap::ALL.len();
    }
}

impl CompactGate for Turns {
    fn at(&self, gap: Gap) -> io::Result<()> {
        self.reached(gap);
        Ok(())
    }
}

/// Answers a mount's FIFO gate from `turns`: each gap's line read from `at`
/// is answered `go` on `go` once [`Turns::reached`] returns. Ends at the
/// line [`end_fifos`] writes.
///
/// # Errors
///
/// The thread could not be spawned.
pub fn answer_fifos(
    turns: Arc<Turns>,
    at: File,
    mut go: File,
) -> io::Result<JoinHandle<io::Result<()>>> {
    std::thread::Builder::new()
        .name("compact-turns".into())
        .spawn(move || {
            for line in BufReader::new(at).lines() {
                let line = line?;
                if line == END {
                    break;
                }
                if let Some(gap) = Gap::ALL.iter().find(|gap| gap.name() == line) {
                    turns.reached(*gap);
                    go.write_all(b"go\n")?;
                }
            }
            Ok(())
        })
}

/// Ends the mount's gate and [`answer_fifos`]' thread: the turns end, an
/// empty line on `go` ends the mount's gate (`agentfs_failpoints::FifoGate`), and
/// the end line on `at` ends the thread.
///
/// # Errors
///
/// A FIFO could not be written.
pub fn end_fifos(turns: &Turns, mut at: &File, mut go: &File) -> io::Result<()> {
    turns.end();
    go.write_all(b"\n")?;
    writeln!(at, "{END}")
}
