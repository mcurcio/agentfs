//! Where the actor's time goes (target git-model §18; ledger R2).
//!
//! Measurements only: nothing here is read by ordering, a conflict decision or
//! a refusal. Each stage counts its runs and adds their durations; a stage may
//! enclose another (`queue_plan` encloses `snapshot_read` and `sync_merge`;
//! `journal_append` encloses `journal_fsync`). `event_owners` is the one Git
//! child that rebuilds the map of each save's commit at open. Totals are
//! process-wide, like the counters of Git children in [`crate::git_cli`] and
//! of objects in [`crate::tree_git`]: one
//! actor runs per process, so a reader compares two snapshots.
//!
//! The volume lock is measured twice over: how long each caller waited to
//! take it (`lock_wait`), and, for each hold, how much of it went to each
//! kind of work (`lock_hold`, [`Hold`]). A kind's time is exclusive: work
//! of one kind run inside another's (a catch-up inside a close) counts to
//! the inner kind only, and what no kind claims is `other`
//! (docs/telemetry.md, "The volume lock").

use std::cell::RefCell;
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

/// Every stage, the Git children the actor's path has started, the objects it
/// handed to Git's store and the fsyncs of the store's `objects/` directory.
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
    /// Each hold of the actor's lock through the call path (`Recording::actor`):
    /// its longest is the longest any other view of the volume waited.
    pub actor_lock: Stage,
    /// Each `git repack` of the tree history's repack task.
    pub repack: Stage,
    /// Each wait to take the actor's lock through the call path, from the
    /// call to the lock taken: what a call felt of other calls' holds.
    pub lock_wait: Stage,
    /// The actor's lock held, by kind of work: per kind, the holds it ran
    /// in, its summed time and its longest time in one hold.
    pub lock_hold: LockHolds,
    /// Calls held at the lag gate, by why they waited: each wait from the
    /// call held to the event that admits it.
    pub gate_wait: GateWaits,
    /// Each FUSE request a tree view answered on its one thread, by op: the
    /// time from the request to the handler's return, which a later request
    /// of the view waits behind.
    pub requests: Requests,
    pub git_children: u64,
    pub object_writes: u64,
    pub object_dir_syncs: u64,
    /// File bytes the actor and the builder copied: a frame's tail into its
    /// buffer, a file map read into memory (docs/trees.md, "File maps").
    pub bytes_copied: u64,
    /// File bytes compared to tell a close that changes nothing.
    pub bytes_compared: u64,
}

/// Why a call that starts new work waited at the lag gate (docs/trees.md,
/// "History trails the files"), the first that holds of: the overdue stop
/// (compaction waits for every step to be built), the overlay limit, the
/// lag limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateCause {
    Overdue,
    Overlay,
    Lag,
}

impl GateCause {
    const ALL: [Self; 3] = [Self::Lag, Self::Overlay, Self::Overdue];

    fn as_str(self) -> &'static str {
        match self {
            Self::Lag => "lag",
            Self::Overlay => "overlay",
            Self::Overdue => "overdue",
        }
    }
}

/// The lag gate's waits by [`GateCause`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct GateWaits {
    pub lag: Stage,
    pub overlay: Stage,
    pub overdue: Stage,
}

impl GateWaits {
    /// Each cause with its name, as the metrics file and OpenTelemetry's
    /// `cause` attribute spell it.
    pub fn by_cause(&self) -> [(&'static str, Stage); 3] {
        GateCause::ALL.map(|cause| {
            let stage = match cause {
                GateCause::Lag => self.lag,
                GateCause::Overlay => self.overlay,
                GateCause::Overdue => self.overdue,
            };
            (cause.as_str(), stage)
        })
    }
}

/// A FUSE request a tree view answers, as [`Requests`] counts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Lookup,
    Getattr,
    Open,
    Create,
    Read,
    Write,
    Flush,
    Release,
    Readdir,
    /// Every other request.
    Other,
}

/// How many kinds [`Op`] has.
pub const OP_COUNT: usize = 10;

impl Op {
    const ALL: [Self; OP_COUNT] = [
        Self::Lookup,
        Self::Getattr,
        Self::Open,
        Self::Create,
        Self::Read,
        Self::Write,
        Self::Flush,
        Self::Release,
        Self::Readdir,
        Self::Other,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::Lookup => "lookup",
            Self::Getattr => "getattr",
            Self::Open => "open",
            Self::Create => "create",
            Self::Read => "read",
            Self::Write => "write",
            Self::Flush => "flush",
            Self::Release => "release",
            Self::Readdir => "readdir",
            Self::Other => "other",
        }
    }
}

/// How many buckets a [`Latency`] has: bucket `i` counts runs under
/// 2^`i` µs (the last, every longer one).
pub const LATENCY_BUCKETS: usize = 25;

/// One kind of request's runs, as a [`Stage`] with a histogram: `buckets[i]`
/// counts the runs under 2^`i` µs and at or over the bucket before.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Latency {
    pub count: u64,
    pub nanos: u64,
    pub max_nanos: u64,
    pub buckets: [u64; LATENCY_BUCKETS],
}

/// The tree views' requests by [`Op`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Requests {
    pub lookup: Latency,
    pub getattr: Latency,
    pub open: Latency,
    pub create: Latency,
    pub read: Latency,
    pub write: Latency,
    pub flush: Latency,
    pub release: Latency,
    pub readdir: Latency,
    pub other: Latency,
}

impl Latency {
    /// Its runs without the histogram.
    pub fn stage(&self) -> Stage {
        Stage {
            count: self.count,
            nanos: self.nanos,
            max_nanos: self.max_nanos,
        }
    }
}

impl Requests {
    /// Each op with its name, as the metrics file and OpenTelemetry's `op`
    /// attribute spell it.
    pub fn by_op(&self) -> [(&'static str, Latency); OP_COUNT] {
        Op::ALL.map(|op| {
            let latency = match op {
                Op::Lookup => self.lookup,
                Op::Getattr => self.getattr,
                Op::Open => self.open,
                Op::Create => self.create,
                Op::Read => self.read,
                Op::Write => self.write,
                Op::Flush => self.flush,
                Op::Release => self.release,
                Op::Readdir => self.readdir,
                Op::Other => self.other,
            };
            (op.as_str(), latency)
        })
    }
}

/// The actor's lock held, by kind of work ([`Hold`]): each kind's runs
/// are the holds it ran in, its time is its exclusive share of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LockHolds {
    /// Journal compaction (`TreeActor::compact`).
    pub compaction: Stage,
    /// Catching up with history at a built point (`TreeActor::rebuild`),
    /// without its drops.
    pub catch_up: Stage,
    /// The last drop of the maps and shapes a catch-up let go.
    pub drops: Stage,
    /// A close's first half and its land, and pending closes saved again.
    pub close: Stage,
    /// A take's inputs and its land.
    pub take: Stage,
    /// A fork's step, its untracked area's copy included.
    pub fork_copy: Stage,
    /// Everything else under the lock: the call's own work.
    pub other: Stage,
}

impl LockHolds {
    /// Each kind with its name, as the metrics file and OpenTelemetry's
    /// `kind` attribute spell it.
    pub fn by_kind(&self) -> [(&'static str, Stage); HOLD_COUNT] {
        Hold::ALL.map(|kind| (kind.as_str(), self.of(kind)))
    }

    fn of(&self, kind: Hold) -> Stage {
        match kind {
            Hold::Compaction => self.compaction,
            Hold::CatchUp => self.catch_up,
            Hold::Drops => self.drops,
            Hold::Close => self.close,
            Hold::Take => self.take,
            Hold::ForkCopy => self.fork_copy,
            Hold::Other => self.other,
        }
    }
}

/// A kind of work under the actor's lock: a [`section`] of it claims the
/// hold's time until it ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) enum Hold {
    Compaction,
    CatchUp,
    Drops,
    Close,
    Take,
    ForkCopy,
    Other,
}

/// How many kinds [`Hold`] has.
pub const HOLD_COUNT: usize = 7;

impl Hold {
    const ALL: [Self; HOLD_COUNT] = [
        Self::Compaction,
        Self::CatchUp,
        Self::Drops,
        Self::Close,
        Self::Take,
        Self::ForkCopy,
        Self::Other,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::Compaction => "compaction",
            Self::CatchUp => "catch_up",
            Self::Drops => "drops",
            Self::Close => "close",
            Self::Take => "take",
            Self::ForkCopy => "fork_copy",
            Self::Other => "other",
        }
    }
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
    /// The actor's lock held, from lock to unlock (agentfs-ln6.1).
    ActorLock,
    /// One repack run (agentfs-ln6.5).
    Repack,
}

const STAGE_COUNT: usize = 15;

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

    fn add(&self, nanos: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.nanos.fetch_add(nanos, Ordering::Relaxed);
        self.max_nanos.fetch_max(nanos, Ordering::Relaxed);
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
static HOLDS: [Total; HOLD_COUNT] = [const { Total::new() }; HOLD_COUNT];
static WAITS: Total = Total::new();
static GATE: [Total; 3] = [const { Total::new() }; 3];

struct Histogram {
    total: Total,
    buckets: [AtomicU64; LATENCY_BUCKETS],
}

impl Histogram {
    const fn new() -> Self {
        Self {
            total: Total::new(),
            buckets: [const { AtomicU64::new(0) }; LATENCY_BUCKETS],
        }
    }

    fn add(&self, nanos: u64) {
        self.total.add(nanos);
        let micros = nanos / 1_000;
        // Under 2^i µs: i is the bit length of the µs.
        let bucket = (u64::BITS - micros.leading_zeros()) as usize;
        self.buckets[bucket.min(LATENCY_BUCKETS - 1)].fetch_add(1, Ordering::Relaxed);
    }

    fn read(&self) -> Latency {
        let stage = self.total.read();
        Latency {
            count: stage.count,
            nanos: stage.nanos,
            max_nanos: stage.max_nanos,
            buckets: std::array::from_fn(|i| self.buckets[i].load(Ordering::Relaxed)),
        }
    }
}

static REQUESTS: [Histogram; OP_COUNT] = [const { Histogram::new() }; OP_COUNT];

/// Records a wait at the lag gate for `cause` that began at `since`.
pub(in crate::tree_actor) fn gate_waited(cause: GateCause, since: Instant) {
    GATE[cause as usize].add(nanos_since(since));
}

/// One FUSE request of a tree view being answered: adds one run of `op`
/// when dropped. Measurement only.
#[must_use = "a request is measured until it is dropped"]
pub struct Request {
    op: Op,
    started: Instant,
}

/// Starts measuring a request of `op`, as the view's handler begins.
pub fn request(op: Op) -> Request {
    Request {
        op,
        started: Instant::now(),
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        REQUESTS[self.op as usize].add(nanos_since(self.started));
    }
}
static COPIED: AtomicU64 = AtomicU64::new(0);
static COMPARED: AtomicU64 = AtomicU64::new(0);

/// Counts `n` file bytes copied.
pub(super) fn copied(n: u64) {
    COPIED.fetch_add(n, Ordering::Relaxed);
}

/// Counts `n` file bytes compared.
pub(super) fn compared(n: u64) {
    COMPARED.fetch_add(n, Ordering::Relaxed);
}

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
            actor_lock: at(Which::ActorLock),
            repack: at(Which::Repack),
            lock_wait: WAITS.read(),
            lock_hold: {
                let held = |kind: Hold| HOLDS[kind as usize].read();
                LockHolds {
                    compaction: held(Hold::Compaction),
                    catch_up: held(Hold::CatchUp),
                    drops: held(Hold::Drops),
                    close: held(Hold::Close),
                    take: held(Hold::Take),
                    fork_copy: held(Hold::ForkCopy),
                    other: held(Hold::Other),
                }
            },
            gate_wait: GateWaits {
                lag: GATE[GateCause::Lag as usize].read(),
                overlay: GATE[GateCause::Overlay as usize].read(),
                overdue: GATE[GateCause::Overdue as usize].read(),
            },
            requests: {
                let op = |op: Op| REQUESTS[op as usize].read();
                Requests {
                    lookup: op(Op::Lookup),
                    getattr: op(Op::Getattr),
                    open: op(Op::Open),
                    create: op(Op::Create),
                    read: op(Op::Read),
                    write: op(Op::Write),
                    flush: op(Op::Flush),
                    release: op(Op::Release),
                    readdir: op(Op::Readdir),
                    other: op(Op::Other),
                }
            },
            git_children: crate::git_cli::children(),
            object_writes: crate::tree_git::object_writes() + super::history::feed::fed(),
            object_dir_syncs: crate::tree_git::object_dir_syncs()
                + super::history::feed::pack_syncs(),
            bytes_copied: COPIED.load(Ordering::Relaxed),
            bytes_compared: COMPARED.load(Ordering::Relaxed),
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
        TOTALS[self.which as usize].add(nanos_since(self.started));
    }
}

fn nanos_since(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// One hold of the actor's lock on this thread, being attributed: the kind
/// whose section runs now, since when, each kind's time so far and which
/// kinds ran (a run counts even if the clock did not move).
struct Attribution {
    current: Hold,
    since: Instant,
    spent: [u64; HOLD_COUNT],
    ran: [bool; HOLD_COUNT],
}

impl Attribution {
    /// Charges the time since the last switch to the current kind, then
    /// makes `to` current; answers the kind that was.
    fn switch(&mut self, to: Hold) -> Hold {
        let now = Instant::now();
        let nanos = u64::try_from(now.duration_since(self.since).as_nanos()).unwrap_or(u64::MAX);
        self.spent[self.current as usize] = self.spent[self.current as usize].saturating_add(nanos);
        self.since = now;
        self.ran[to as usize] = true;
        std::mem::replace(&mut self.current, to)
    }
}

thread_local! {
    /// The hold of the actor's lock this thread is in, if any. The lock's
    /// guard is not `Send`, so a hold begins and ends on one thread.
    static HELD: RefCell<Option<Attribution>> = const { RefCell::new(None) };
}

/// Records a wait for the actor's lock that began at `started` and ended now.
pub(super) fn waited(started: Instant) {
    WAITS.add(nanos_since(started));
}

/// One hold of the actor's lock, from its start to its drop: one run of
/// `actor_lock`, and each kind's share of it added to `lock_hold`.
#[must_use = "a hold is measured until it is dropped"]
pub(super) struct Held {
    _lock: Timer,
}

impl Held {
    /// The lock was just taken: the hold's time is `other`'s until a
    /// [`section`] claims it.
    pub(super) fn start() -> Self {
        let lock = Timer::start(Which::ActorLock);
        let mut ran = [false; HOLD_COUNT];
        ran[Hold::Other as usize] = true;
        HELD.with(|held| {
            *held.borrow_mut() = Some(Attribution {
                current: Hold::Other,
                since: lock.started,
                spent: [0; HOLD_COUNT],
                ran,
            });
        });
        Self { _lock: lock }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let Some(mut attribution) = HELD.with(|held| held.borrow_mut().take()) else {
            return;
        };
        attribution.switch(Hold::Other);
        for (kind, total) in HOLDS.iter().enumerate() {
            if attribution.ran[kind] {
                total.add(attribution.spent[kind]);
            }
        }
    }
}

/// A run of one kind of work: while it lives, the hold this thread is in
/// counts its time to `kind`. Outside a hold (an actor used directly) it
/// measures nothing.
#[must_use = "a section claims the hold's time until it is dropped"]
pub(in crate::tree_actor) struct Section {
    previous: Option<Hold>,
}

/// Starts a [`Section`] of `kind`.
pub(in crate::tree_actor) fn section(kind: Hold) -> Section {
    let previous = HELD.with(|held| held.borrow_mut().as_mut().map(|a| a.switch(kind)));
    Section { previous }
}

impl Drop for Section {
    fn drop(&mut self) {
        if let Some(previous) = self.previous {
            HELD.with(|held| {
                if let Some(attribution) = held.borrow_mut().as_mut() {
                    attribution.switch(previous);
                }
            });
        }
    }
}
