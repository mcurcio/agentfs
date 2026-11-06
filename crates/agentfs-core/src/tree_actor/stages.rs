//! Where the actor's time goes (target git-model §18; ledger R2).
//!
//! Measurements only: nothing here is read by ordering, a conflict decision or
//! a refusal. Each stage counts its runs and adds their durations; a stage may
//! enclose another (`queue_plan` encloses `snapshot_read`, `event_owners` and
//! `sync_merge`; `journal_append` encloses `journal_fsync`). Totals are
//! process-wide, like the counter of Git children in [`crate::git_cli`]: one
//! actor runs per process, so a reader compares two snapshots.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::Serialize;

/// One stage's runs: how many, their summed duration and the longest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Stage {
    pub count: u64,
    pub nanos: u64,
    pub max_nanos: u64,
}

/// Every stage, and the Git children the actor's path has started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Stages {
    pub journal_append: Stage,
    pub journal_fsync: Stage,
    pub compaction: Stage,
    pub align: Stage,
    pub objects: Stage,
    pub ref_copy: Stage,
    pub checkout: Stage,
    pub queue_plan: Stage,
    pub snapshot_read: Stage,
    pub judge: Stage,
    pub sync_merge: Stage,
    pub prior_decisions: Stage,
    pub event_owners: Stage,
    pub git_children: u64,
}

/// The stage a [`Timer`] adds to.
#[derive(Clone, Copy)]
pub(super) enum Which {
    JournalAppend,
    JournalFsync,
    Compaction,
    Align,
    Objects,
    RefCopy,
    Checkout,
    QueuePlan,
    SnapshotRead,
    Judge,
    SyncMerge,
    PriorDecisions,
    EventOwners,
}

const STAGE_COUNT: usize = 13;

struct Total {
    count: AtomicU64,
    nanos: AtomicU64,
    max_nanos: AtomicU64,
}

impl Total {
    const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
            max_nanos: AtomicU64::new(0),
        }
    }

    fn read(&self) -> Stage {
        Stage {
            count: self.count.load(Ordering::Relaxed),
            nanos: self.nanos.load(Ordering::Relaxed),
            max_nanos: self.max_nanos.load(Ordering::Relaxed),
        }
    }
}

static TOTALS: [Total; STAGE_COUNT] = [const { Total::new() }; STAGE_COUNT];

impl Stages {
    /// The totals so far in this process.
    pub fn current() -> Self {
        let at = |which: Which| TOTALS[which as usize].read();
        Self {
            journal_append: at(Which::JournalAppend),
            journal_fsync: at(Which::JournalFsync),
            compaction: at(Which::Compaction),
            align: at(Which::Align),
            objects: at(Which::Objects),
            ref_copy: at(Which::RefCopy),
            checkout: at(Which::Checkout),
            queue_plan: at(Which::QueuePlan),
            snapshot_read: at(Which::SnapshotRead),
            judge: at(Which::Judge),
            sync_merge: at(Which::SyncMerge),
            prior_decisions: at(Which::PriorDecisions),
            event_owners: at(Which::EventOwners),
            git_children: crate::git_cli::children(),
        }
    }
}

/// Adds one run of a stage when dropped, whichever way the scope ends.
#[must_use = "a timer measures until it is dropped"]
pub(super) struct Timer {
    which: Which,
    started: Instant,
}

impl Timer {
    pub(super) fn start(which: Which) -> Self {
        Self {
            which,
            started: Instant::now(),
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        let nanos = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let total = &TOTALS[self.which as usize];
        total.count.fetch_add(1, Ordering::Relaxed);
        total.nanos.fetch_add(nanos, Ordering::Relaxed);
        total.max_nanos.fetch_max(nanos, Ordering::Relaxed);
    }
}
