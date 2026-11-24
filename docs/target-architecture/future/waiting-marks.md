# The mark: one way to wait, and every wait in the journal

**Draft, awaiting the owner's answers to its questions. Written before PR #43; line and commit references are to the code then.**

Layers: domain (agreed) · **system (this page)** · module · interfaces · implementation. Revision 2 (review
responses at the end). Prerequisites from PR #42, none on the branch at cb56648: the core built-event follower, the
close record written first, and a refusal carrying its `before`. Nothing here changes what is recorded or when a call
returns.

**The owner's words (2026-11-23, OWNER-GUIDANCE.md, all general guidance):** "Per-file". "I think #1 sounds like a clear
win, and #2 could build on top of that" (#1: one waiting primitive for durable, saved, built and checkpointed). "I agree
with the background worker thread. That and putting all the conditions and waits in the same oolong seem like it will
cover many edge cases" ("oolong" read as the oplog). What the last sentence asks for is Q1. Background: coordinate by
events (2026-10-24); principles 1 and 5.

**What changed since your last look.** The mark had its own lock and a copy of its owner's number; now number, waits
and demand share the owner's lock. A thread follows or executes, never both. `saved` moves to #2. The volume has a stop.

## The shape

```
calls -> ACTOR lock: decide, append n, apply; the stop field --release--> COMMIT POINT A: re-check every actor state wait
          | each append reports a step or a switch record to History (the builder's standing demand)
DURABILITY lock: durable, its waits, its failure   make_durable: reached | lead | failed, in one hold (as today)
          '-> tree-durable (follower): leads the fsyncs that registered waits ask for
HISTORY lock: built, its waits, asks, attempts; the gate's figures --release--> COMMIT POINT B: re-check the gate's waits
          '-> tree-builder owns built;  tree-compactor owns checkpointed and compacted
followers (may block):   builder, compactor, repack, tree-durable, metrics
executors (never wait):  view worker (one per view), core worker, take worker, invalidation worker
```

## The primitive

A **mark** is a rising record number in its owner's state. `Waiters`, the one shared piece, sits in that state with no
lock, thread or number of its own (etcd's `wait.WaitTime` beside its applied index).

```rust
/// The waits on one mark, in the owner's state, under the owner's lock. Invariants: n <= at answers
/// Reached, also after a stop. Each registration is answered once. Ready jobs are posted after release.
pub struct Waiters { /* parked */ }

impl Waiters {
    /// One hold: Reached, or the stop, or registered. Registering is the ask: it raises `asked()`
    /// to min(n, end), `end` being the journal's end now, and tags the wait with the current attempt.
    pub fn wait(&mut self, at: RecordNo, end: RecordNo, n: RecordNo, k: Resume) -> Option<(Resume, Answer)>;
    /// A follower's subscription: answered at the first advance past `seen`. Never an ask.
    pub fn next(&mut self, at: RecordNo, seen: RecordNo, k: Resume) -> Option<(Resume, Answer)>;

    pub fn asked(&self) -> RecordNo;                     // the owner's demand from waits
    pub fn asked_since(&self, a: Attempt) -> bool;       // a wait registered after attempt `a` began

    // The owner's moves. Each returns the ready jobs.
    pub fn begin(&mut self) -> Attempt;                  // an attempt starts (a build batch)
    pub fn advance(&mut self, to: RecordNo) -> Ready;    // never unwinds
    pub fn fail(&mut self, a: Attempt, e: AttemptError) -> Ready; // waits registered at or before `a` began
    pub fn stop(&mut self, why: StopReason) -> Ready;    // final for waits above the mark
}

pub type Answer = Result<RecordNo, WaitError>;

pub enum WaitError {
    Stopped(StopReason),   // final
    Failed(AttemptError),  // this attempt only; a later wait is the retry's ask
    Ended,                 // its executor was closed before it registered: the view or host ended
}

pub enum StopReason {
    Shutdown,              // the explicit volume stop
    Io(i32),               // a failed fsync, sticky, wherever it is heard
    OwnerLost,             // the owner's thread unwound: its guard dropped unfinished
}

/// On the stack of the thread that moves a mark. Not Clone; Send. Dropped without `finish`,
/// a panic's unwinding included: the mark stops with OwnerLost, in one hold of the owner's lock.
pub struct OwnerGuard<'a> { /* parked */ }

pub struct Resume {                // work resumed on a named executor
    on: Arc<dyn Executor>,
    job: Box<dyn FnOnce(Answer) + Send>,
}
```

| Mark | Owner (state and lock) | Moves when | Its demand | Its stop |
|---|---|---|---|---|
| durable | `Durability` | an fsync that began after the record's write ends | `make_durable` leads in its own hold; tree-durable leads for `asked()` | an fsync's failure, by a leader or by the compactor through `Durability::fsync_failed`; a leader's guard (`leading` becomes a guard) |
| built | `History` | each built event | the append's report of each step and switch record, plus `asked()`; after a failure, only `asked_since` | the builder's guard: Shutdown at its end, OwnerLost on unwinding |
| checkpointed | `History` | a checkpoint is published: its built point | none: waits are not asks; compaction is the compactor's policy | the compactor's guard |
| compacted (new) | `History` | the compactor is done with a built point: not due, or its compaction ended, unlink included | none | the compactor's guard |

The number stays the owner's (`Durability`'s mark, History's built point). A waiter woken at n reads its owner's state
at or past n: number, state and wake are one hold. The builder sleeps on its demand, woken by the hold that asks.

## Executors

**Rule:** a thread follows (blocks only on its own demand or on upstream marks) or executes posted jobs. An executor
never waits for another thread's progress: no mark wait, condvar, queue or join. CPU work, actor-lock holds, its own
file I/O and fsync, and the untracked area's lock are allowed.

```rust
/// agentfs-core's port; agentfs-fuse provides the view and take workers.
/// Invariants: `post` never runs a job on the posting thread. Each job is caught on its own.
/// An executor's death stops what depends on it (the core worker's: the actor, OwnerLost).
pub trait Executor: Send + Sync {
    /// Err(Closed) after `close`: the caller answers Ended itself.
    fn post(&self, job: Box<dyn FnOnce() + Send>) -> Result<(), Closed>;
    /// Phase 1: no new registration may name this executor. Those already made are delivered normally.
    fn close(&self);
    /// Phase 2, on a host thread: returns once its queue and its outstanding registrations are empty.
    fn drained(&self);
}
```

| Worker | Runs | Waits on |
|---|---|---|
| view worker (fuse, one per view; today's `tree-saves`) | save decisions and landings (until #2), a queued close's turn, its retries, gated calls, folds, the rest of a move after `durable`, its fsync replies | the actor lock; a move out of the tree also the untracked area's read lock (a fork's copy holds the write side throughout) and `place`'s fsync |
| core worker (core, `tree-built`) | `on_built` at each `built.next`; held-back closes (5b: today's replay under the lock, about 1 s per million calls) | the actor lock |
| take worker (fuse, `tree-takes`) | takes at each `built.next`; held takes | the actor lock |
| invalidation worker (fuse, one per host) | page drops after a landing, then `Landed` | the kernel's page fill, served by a request thread |

`tree-gate` is deleted, and tree-durable runs no replies: every continuation is posted. Tests assert, as
`assert_unlocked` does, that no executor enters a mark wait, condvar wait or join.

## State waits, and what calls change

A state wait is a named condition on one lock's state, checked in the hold that registers it and at every release.
Named, not closures, so a debug build can re-evaluate all at each release and panic on one ready but unanswered.

```rust
/// Commit point A: checked at every release of the actor lock. All registrations, every release.
pub enum ActorUntil {
    TurnOf { tree: TreeId, object: u64, record: RecordNo },      // 2a, 5b: no close of it below `record` in flight
    SavesBefore { tree: TreeId, object: u64, before: RecordNo }, // 3: none below the refusal's `before`
    Settled { before: RecordNo },                                // settle: no close below `before` in flight or held back
    Unheld { tree: TreeId, keys: HeldKeys },                     // 7b: no open or pending handle holds a key
}
/// Commit point B: checked at every release of History's lock.
pub enum HistoryUntil {
    Admits { tree: TreeId },                                     // 6: lag and overlay under their limits (after built stops: Q2)
}
// Answers: Ok when true; Stopped(reason) in the hold that sets the stop; Ended only at registration. A release
// while unwinding evaluates nothing: it sets the stop (OwnerLost). No registration outlives the volume stop.

// Session and Recording calls that change (agentfs-core).
fn admit_then(&self, tree: TreeId, on: Arc<dyn Executor>, k: Job);            // was: ran on tree-gate
fn when_saved_then(&self, refused: SavePending, on: Arc<dyn Executor>, k: Job); // SavePending carries `before`
fn durable_then(&self, through: RecordNo, on: Arc<dyn Executor>, k: Job);     // was: replied on tree-durable
fn open_map(&self, ..) -> Result<Opened, Error>;  // Opened::Ready(map) | Opened::Fold(Fold); also write_open
fn settle(&self) -> Result<RecordNo, WaitError>;  // Settled at A, then built.wait(the last record then)
fn wait_checkpointed(&self, n: RecordNo) -> Result<Checkpointed, WaitError>; // a published checkpoint's built point >= n
fn wait_compacted(&self, n: RecordNo) -> Result<(), WaitError>;               // compacted >= n
fn stop(&self) -> Result<(), Error>; // the volume stop; drop is a fallback only
// Fold::run(self) runs on the view worker off the lock and re-checks under it.
// Gone: wait_compactor, wait_for_work, wait_for_event_or, wake_workers, want_through.
```

**Stop is one field per lock.** The actor's `stop: Option<StopReason>` is the one judge of "the volume is stopped", and
`is_stopped` reads only it.

| Flag today | Becomes |
|---|---|
| actor `stopped`, `journal.unknown` | the actor's stop field |
| `durability.has_failed` | durable's own stop; the thread that records it (a leader, the compactor) then sets the actor's in a hold. A call between may append, as today between its check and the failure |
| History `stopping` | the volume stop's request to the builder: an input, not a judge |
| History `stopped`, `builder_gone` | built's stop |
| `compaction_stopped` | checkpointed's and compacted's stop |

**The volume stop.** One call on a host thread: the actor's stop (Shutdown), the builder's last batch and stop, the
compactor's, the core executors closed, drained and joined. A host closes its own executors first, then calls it. Drop
runs it only if no one did, never joining its own thread. "The lease is free" then holds at the stop's return, and the
gate thread's special case goes with that thread.

## Every wait in the survey (15 entries)

| # | Wait | Becomes | Resumed on |
|---|---|---|---|
| 1 | Volume lock | stays; its release is commit point A | caller |
| 2a | Queued close of the same file | `TurnOf` at A | its view worker, unblocked |
| 2b | `settle` | `Settled` at A, then `built.wait` | caller |
| 3 | Refused unlink, rename onto, replace, truncate, time set, move out | `SavesBefore` at A. Not bounded: a later save can refuse the retry again | the refused view's worker |
| 4, 5a, 12 | Save worker's queue; failed saves; deferred drops | the view worker; unchanged; unchanged | n/a |
| 5b | Closes a client's end held back | `TurnOf` at A | core worker |
| 6 | Lag and overlay gate, blocking `admit` | `Admits` at B (`tree-gate` goes) | the gated call's view worker; caller |
| 7a, 7c | Take worker's events; `on_built` | `built.next(seen)` | take worker; core worker |
| 7b | Takes held on files | `Unheld` at A | take worker |
| 8a | `wait_built` | `built.wait`: ask, registration and failure test in one hold | caller |
| 8b | Builder's `next_work` | the builder's own demand. Hand asks at mod.rs:947, :1765, :1827 (switch records) become the append's report; :1870 the core worker's ask | builder |
| 9a, 13 | Compactor's and repack's batch end | `built.next(seen)`: single-purpose followers, blocking | compactor; `tree-repack` |
| 9b | The three checkpoint waits | publish, then its unlink: `checkpointed` then `compacted`; "done with a built point": `compacted` | caller |
| 10 | Durability's group commit | `make_durable` unchanged; `durable_then` registers, and tree-durable leads for it | caller; the named executor |
| 11 | Pending fold | `Opened::Fold` | the opening view's worker |
| 14a | Host shutdown | the host closes its executors, then the volume stop | host thread |
| 14b, 15 | Metrics worker; the recording engine | a timer by design; out of scope (being retired) | `tree-metrics` |

## Design rules (design choices, not requirements)

1. A mark's number, its waits and its demand live in its owner's state under one lock. Registering, asking and the
   reached, failed or stopped test are one hold, and reached wins.
2. A thread follows or executes, never both. Every resumed job is posted to a named executor after the lock is
   released, and is caught on its own.
3. Every other wait is a named condition on the actor's or History's state, re-checked at every release of that lock.
   Stop is a field under that lock.

Deadlock freedom: no thread waits holding the actor lock (`assert_unlocked`, today). Locks go actor, then History or
Durability, then an executor's queue; `post` never runs inline, so no edge runs back. Followers wait on their own demand
or upstream marks, a graph with no loop. Executors wait on no thread's progress; a fork's copy waits on none of them.

## Alternatives

| | Chosen: `Waiters` in each owner's state | RAII guards and a release-time bump, no new type | `tokio::sync::watch`, a runtime per worker |
|---|---|---|---|
| What | one wait list; numbers stay home | guards on builder, compactor, leader; the epoch bump in `ActorGuard::release` | a `watch` per mark; async workers |
| Gives | one form of wait; demand and failure in its hold | the panic hangs go, least code | a mature runtime; value travels with the wake |
| Costs | about 200 lines with tests; each release re-checks its waits | eleven waits stay on one condvar, the hand asks stay, a new state change must still remember a wake | a second home for each number; demand and attempts still need a lock; tokio enters agentfs-core, every worker async |

Prior art: PostgreSQL's `XLogFlush` (lock or wait and re-check, from memory), which `make_durable` already is; etcd's
`wait.WaitTime`; Kafka's purgatory, whose watch keys are the list this design avoids by re-checking all; Mesa monitors.
The choice goes under Technology choices in docs/architecture.md when built.

## Costs and deferrals

- Every actor release re-checks every registration, and History's every release the gate's. A held take's check
  walks its tree's handles. Measure with `lock_hold`; a keyed index is the module gate's, behind the debug check.
- A refused call can be refused again by a later save of its file; a steady closer starves it. Q1's B or #2 gives it a
  place in the file's order.
- A view worker behind a fork's copy, a fold or a decide delays its view, and other views' closes of that file, until #2.
- The checkpoint waits become record-based, a public API change (mod.rs:1946-1955, two test files, docs/library/core.md).
- `saved` (in the owner's #1) waits for #2, as the save follower's own number. Now it would be a low watermark kept at
  every release for one wait, `settle`.

## What happens next

**Tracer (slice 1).** `built`'s `Waiters` with the demand and attempt rules, the builder's guard, the core and take
workers as executors on `built.next`, the two-phase close, the volume stop: wait, next, advance, fail, stop and the
guard. Proof: a builder panic mid-batch answers `wait_built`, `settle` and the stop with `Stopped(OwnerLost)`; a batch
that fails at once is retried by an ask made after it began; PR #42's overlay test needs no hand ask. Not run: today's
code hangs on the first two, by reading. Non-goals: durable, the state waits, the workers' new jobs.

Slices 2 to 5: `durable` (leader's guard, the compactor's failure, one stop field); `checkpointed` and `compacted`; the
workers (posted jobs; `tree-gate` goes; a same-view `stat` during a 1 s fold stays at baseline); commit points A and B
(both epochs go; a take held on a file lands when it closes unchanged; docs). No landing rule changes: regression tier.

## Questions

**Q1 (general guidance). What did you mean by "putting all the conditions and waits in the same oolong"?**

| | A. One numbering (recommended) | B. Also written as records |
|---|---|---|
| What | every wait answers from the journal's numbering at one of two commit points. 2a, 2b, 3 and 5b compare close records. Two do not: the gate (estimated build time and overlay bytes against limits) and a held take (which open files hold its keys) | a refused call and a held take are also journal records |
| Gives | no format change | a refused call gets its place: later closes of the file queue behind it, so row 3 cannot starve |
| Costs | row 3 can starve until #2 | new record kinds, a format change, the model's rules (full sweep) |

The answer decides whether slice 5 adds record kinds.

**Q2 (general guidance). When the history builder dies, does the workload keep writing?** The builder turns journal
records into git history. A panic is final until the process restarts. Today the gate lets every call through once the
builder failed, but `admit` answers Stopped once it is gone, so new creates fail.

| | A. Keep writing (recommended) | B. New creates fail |
|---|---|---|
| Workload sees | nothing; an alarm is raised | errors until a restart |
| Costs | no compaction: the journal grows until restart | a history fault reaches the workload (principle 5) |

The answer sets what `Admits` answers after `built` stops, and whether a history stop stops the volume.

**Q3 (general guidance). One PR or two?** Five slices, against "about 4". Recommended: two, the marks (1-3), then the
workers and commit points (4-5). One PR is one review, a larger diff.

## New terms

| Term | Meaning | Why needed | Existing term |
|---|---|---|---|
| mark | a rising record number in its owner's state, with its waits | the owner's #1 | "durable mark" |
| ask | a wait registered above the mark: demand | demand in the wait's hold | `wanted` |
| attempt | one try at moving a mark; its failure answers waits asked before it | retries | `requests` |
| owner guard | on the owner thread's stack; dropped unfinished, stops the mark | panic becomes stop | n/a |
| follower / executor | blocks on demand or upstream marks / runs posted jobs, waits on no thread | deadlock freedom | builder / save worker |
| commit point | the release of the actor's or History's lock, where its state waits are re-checked | one wake | `ActorGuard` release |
| state wait | a named condition on one lock's state | the six left | n/a |
| compacted | the built point the compactor is done with | a wait for a compaction never due | `compactor_through` |

## For the code architects (parked)

`Waiters`' crate; the debug re-check and any keyed index; `Fold`; counting outstanding registrations; drain order at a
view's end; `Landed`; the metrics timer; History's lock cost at B.

## Reconcile with existing code

`make_durable` stays (it is already acquire-or-wait); `leading` becomes a guard and `Then`'s queue a `Waiters`.
`wanted`, `requests` and `failed_at` fold into History's `Waiters`; its condvar keeps the builder's demand and blocking
`admit`. `ActorGuard::release` becomes commit point A; the close epoch and the `waiters` map go. `SaveWorker` becomes the
view worker, minus its inline run after a drain. Cost: about fifteen wait sites and their hooks; no format change.

## Review responses

1: applied; 8b goes. 2: `claim`, `park` deleted; `make_durable` is the one call. 3: applied with `next(seen)`:
`then(seen + 1)` is an ask and would drive a batch per open record. 4: re-check all; one stop field. 9: `saved` to #2.
10: cost stated. 13: Rust, per docs/target-architecture/README.md. 15: proof not run. Others applied.
