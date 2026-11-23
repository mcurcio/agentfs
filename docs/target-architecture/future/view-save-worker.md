# A large close must not stall the view's other files

**Proposed, not built.** A tree view answers its kernel requests on one thread, and today it works out each file's
save on that thread when the file is released. While a large save is worked out, every other request of that view
waits. This page moves that work to a second thread per view, the **save worker**, and says what the view answers
for a file whose save has not landed yet. The commit that lands the last slice deletes this page and moves the
behaviour into [trees](../../trees.md), the library pages and [telemetry](../../telemetry.md).

**Layer: System** (Domain · **System** · Service · Module · Interfaces · Implementation). The domain does not
change: the model's rules 1 and 9 stay as written, and the model's close is the save landing.

## Today

A view answers its kernel requests on one thread (`agentfs_fuse::tree_view`). A RELEASE, which the kernel sends
after `close(2)` has returned, is saved on that thread (`TreeSession::close`), so while one file's save is worked
out the view's other requests wait. In one local run each, a stat of another file in the same view waited 377 ms
(`close_frag`) and 860 ms (`write_1m`); in another view of the volume it waited 124 and 87 ms
([performance](../../performance.md#tree-views-release-build)).

## Background

These are the owner's guidance (OWNER-GUIDANCE.md, "Performance"); none was asked as an absolute requirement.

- 2026-11-22: "Multi threading sounds important".
- 2026-11-22: "what really matters is how agentfs changes the working tree, not what the client in the container does
  with the filesystem. The client should be free to read and write its files, blissfully unaware of other processes
  interacting with the filesystem. ... agentfs can only merge in changes when the merge doesn't touch any open files
  or files with changes pending in the pending."
- 2026-11-22, "Agreed" to: a file whose save is still being worked out counts as open for model rule 9, so a sync or
  landing that touches it waits and one that does not proceeds; rule 1 stands with no new rule for that window; and
  in "write a temporary file, close it, rename it over the real one", the rename goes first and the pending save
  follows the file to its new name.
- 2026-11-23, asked whether a delete of a file whose save is still being worked out should wait for that save or go
  first and drop it: "The "wait" option sounds like a safe and sensible default for a baseline."
- Also binding as background: close-to-open (rules ledger R1.1, "Go", 2026-11-03); "we absolutely must avoid
  deadlocking" (2026-11-05); "It's all event driven" (2026-10-24).

## The frame

A closed file whose save has not landed is a **pending save**. It is the newest content for its own client, and an
open file for everything else. The view then needs no order between files: it answers its client from the pending
save, and rule 9 keeps takes off the file until the save lands.

```
kernel --one connection per view, in arrival order--> request thread (one per view, as today)
  RELEASE f       start the close under the lock (short) -> answer the kernel -> f's save to the save worker
  OPEN/STAT f     answered now: f's pending content for its own client, its size as today
  RENAME f        answered now; f's pending save follows f to its new name
  UNLINK f        waits for f's pending save, answered from the thread that lands it
save worker (one per view)        decide f's save with the lock released -> land it under the lock
takes, syncs, landings (core)     f held while open or pending -> land once f's save is built
```

| A request on f while its save is pending | Waits? | Answered from |
|---|---|---|
| GETATTR, LOOKUP; READ through the page cache | no | the size the view already gives while f has a write-open (`inode_size`, kept until the save lands); the modification time moves only when the save lands; f's image, which folds every open's calls, a pending save's included (`pages.rs`) |
| OPEN, read-only or for writing, by f's own client | no | f as its pending saves leave it. A write-open's own save still replays its calls on the saved head (rule 8) |
| OPEN by another client of the same tree | no | the saved content, as for any reader while a save is not in the tree |
| RELEASE of a second open of f | no | begun and answered at once; decided after f's earlier save lands, so f's saves land in the order its client closed them |
| RENAME of f | no | f keeps its object; the pending save lands on it at its new name |
| UNLINK of f; RENAME onto f; a whole-file replace of f; a path `truncate(2)` of f | **yes**, until f's save lands or fails | the save worker's thread, once the save has landed, by the call retried |

### Design rules

These are design choices, not requirements.

1. **The request thread never waits for a save.** It begins each RELEASE and answers it; decisions run on the view's
   own save worker, so no view waits for another's and no fewer decisions run at once than today. One request thread
   keeps the kernel's order: a RELEASE is begun before any later OPEN of its file.
2. **A pending save is its file's content for its own client and an open file for everyone else.** The client reads
   and stats it; takes, syncs and landings wait for it; history and other trees see only landed saves. It is never
   folded into another open's save.
3. **A file's saves land in the order its closes began, and every wait ends on an event** (a save landing or failing,
   the view ending), never a timer. A save waits only on saves begun before it, so no two saves wait on each other.
4. **A call that would delete or replace a file with a pending save waits for that save**, and the rename of the file
   itself goes first. History then holds the version the program finished writing, then the delete.

## Rename and unlink

**A rename goes first, and the core already allows it.** A rename keeps the file's object, and a pending save is
keyed by its open's object. The save lands if the object is still named and its saved step and page writes are
unchanged (`TreeActor::close_finish`). Nothing is re-keyed, and rule 9's held names follow the object. So
`close(tmp)`, `rename(tmp, real)`, then the save landing records history as
`[real = tmp before its save] [real = new]`.

If the save fails, history keeps the first state until the retry lands, and the client still reads its new content.
The cost: an editor's save over an existing file then aligns against tmp's content before its save, so it merges as
a rewrite of the old file, not line by line (R11.2), as an open tmp renamed before its close does today.

**A delete waits for the file's pending save** (the owner, 2026-11-23). The journal is append-only and a delete is
recorded before it returns (principle 1), so a delete that went first would leave the save landing on a file no name
reaches, which the core drops (`OrphanDropped`, R11.3). Waiting keeps the content in history: the save, then the
delete.

How it waits, with no thread blocked:

- Under the lock, the session's delete sees the file's pending save and refuses with nothing done: a
  `PathStep::WaitFor(SavePending)`. Only the core can make a `SavePending`, and the type must be used, so a caller
  cannot drop the refusal by mistake.
- The view hands the `SavePending` and a retry of the whole request to `TreeSession::when_saved_then`. The check and
  the registration are under the actor's lock, so a landing between them is not missed.
- When the last pending save of the file lands or fails, the thread that landed it (the view's save worker) runs
  the retry with the lock released. The retry may be refused again for a save begun meanwhile, and then waits again.
- The kernel holds the directory's lock for the whole unlink, so the program's other calls in that directory wait in
  the kernel, as the owner's question said ("that delete and its directory wait up to about a second"). Requests on
  other files go on.

The same holds for a rename onto the file, a whole-file replace of it (a move from the untracked area onto it) and a
path `truncate(2)` of it, since each destroys or replaces the content the save would land. This follows from the
answer about deletes, and rule 9 already lists these calls beside the delete. A move of the file out of the tree, into
the untracked area, copies its pending content into the area and then waits, as a delete does, before the tree's
name goes.

## Merges (rule 9)

No rule changes. A take collects held files from every open of its tree (`takes.rs`). An open whose close is being
decided (`closing`) or whose save failed (`pending_closes`) stays an open until its save is recorded, and the land
checks again. The waiting take wakes on the save's built event; a take that touches no pending file lands meanwhile.

| When | The client | Told | Recovery |
|---|---|---|---|
| A decision fails, or the save worker panics in one | sees nothing (`close(2)` returned long before); keeps reading its content | `tree SaveFailed` on stderr, the `failed_saves` counter and the `pending_saves` gauge in the metrics file | the file stays held; saved again under the volume lock at the tree's next step and at restart |
| The save worker hangs | nothing waits but a delete of that file; the view's later saves queue, their files held | the `pending_saves` gauge, with `oldest_pending_save_ns`, for an alert | no timer invents a failure; a restart replays the pending saves |
| agentfs is killed with saves pending | its view is gone | the restart's log | recovery closes every open it finds in the journal, a pending save's included |

## No deadlock

Every close begins under the actor's lock, so all closes of a volume have one begin order. A save worker takes its
view's saves in begin order, and a save waits only for an earlier-begun save of the same file. A cycle of waits would
need a save to wait, through other workers, for a save begun after it, which the begin order rules out. A delete's
retry runs one short call under the lock and waits for no save. `Recording::settle`, which waits for every pending
save, is never called from a save worker.

## Design it twice

| | **Answer from the pending save (chosen)** | A queue per file in the view |
|---|---|---|
| Requests on the same file | none waits but a delete, a rename onto it, a replace and a path truncate | stat, open and rename wait for the save |
| RELEASE answered | at its start: nothing for the kernel to queue | at the save: a bound under `max_background`, the request thread stopping at it |
| New machinery | a worker per view; small core changes | queues per file, a key per request kind, a turn value |
| Gives up | the modification time until the save lands; a second live writer's unsaved calls are visible to an open of a pending file when the file's image is the source | the owner's "blissfully unaware" |

**Canonical shape.** *Domain:* nothing added: rules 1 and 9 as written. *Patterns:* write-back caching with
read-your-writes (Linux's page cache: dirty pages answer reads before writeback, and invalidation waits for writeback,
as a take waits for a pending save); one reader with replies sent from other threads (fuser, as FSYNC already does
here); a worker per resource; a continuation on an event (as `admit_then` is for the lag gate).

## Reconcile with existing code

| Existing | Becomes | Cost |
|---|---|---|
| `TreeSession::close` blocks; the view's `release` answers after it | `TreeSession::start_close` and `SaveToDecide::finish` (`close` and `Recording::close_with` are both, for the CLI, tests and recovery); RELEASE answered after `start_close`; a save worker per view, drained before `ViewEnded` | small |
| `close_begin`'s quick save; opens take the saved map | never ahead of an earlier save of the same file; opens by the file's client take its pending content | one condition; one core read |
| `unlink`, `rename`, `replace_file` on the session answer at once | refused with `PathStep::WaitFor` for a pending save, retried by `when_saved_then` | the session's three signatures and their callers in the view |
| takes, rename of the pending file | unchanged | none |
| trees.md: "the step is recorded before the release is answered"; git-model §3: "readers keep the old content" | the close begun before the answer; old content for every reader but the file's own client. git-model §3 is a baseline page: the change is reported to the owner, not made there | docs |
| the conformance replay's `released()` stats the root after a close, then asks the gate to `settle` | `released()` still orders the release's start before the gate's line; the gate's `settle` now answers once no save is pending and history is built through every record | one wait in `Recording::settle` |

## Frozen slice pages

Two slices, run in order in one lane. The tracer carries every change the code needs to stay correct, because
answering a RELEASE early is only safe with pending content for opens, the delete's wait and the replay's settle in
place; the second slice measures it and tries it under failure.

The tracer's interfaces are committed as compiling code with `todo!()` bodies: `Closing`, `SaveToDecide`,
`SavePending`, `PathStep`, `Recording::start_close_with`, `Recording::when_saved_then`, `TreeSession::start_close`,
`TreeSession::when_saved_then`, `SessionGuard::open_map`, `SessionGuard::truncate_path` and
`tree_view/save_worker.rs`. The signatures marked *changes* below are not in that commit, because changing them
changes their callers' behaviour; the slice makes them.

### view-save-1: the tracer, a pending save answered from the save worker

**Goal it delivers.** The view's other files are not held up by a large close (the owner, 2026-11-22), with
close-to-open for the file's own client, rule 9's holds, the rename first and the delete waiting.

**Exact signatures.**

```rust
// crates/agentfs-core/src/tree_actor/call_path.rs; re-exported from agentfs_core::tree_actor.
#[must_use]
pub enum Closing {
    Saved(Saved),
    Decide(SaveToDecide),
}

pub struct SaveToDecide { /* recording: Arc<Recording>, handle: u64, and its snapshot when taken at start */ }

impl SaveToDecide {
    pub fn handle(&self) -> u64;
    /// Same-file order: waits on the close epoch until no earlier-begun save of
    /// its file is in flight, then snapshots, decides with the lock released and
    /// lands under it. Runs its file's `when_saved_then` callbacks once none is
    /// pending, with the lock released. Hits failpoint `tree.close_decide` before
    /// it decides (error: a failed decision; panic; sleep(ms)).
    pub fn finish(self) -> Result<Saved, Error>;
}

impl Drop for SaveToDecide {
    /// Unfinished (a panic included): `TreeActor::close_failed`, counted in
    /// `failed_saves`; its file's `when_saved_then` callbacks run.
    fn drop(&mut self);
}

#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavePending { /* tree: TreeId, object: u64; made only in crate::tree_actor */ }

impl SavePending {
    pub fn tree(&self) -> TreeId;
    pub fn object(&self) -> u64;
}

#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathStep<T> {
    Done(T),
    WaitFor(SavePending),
}

impl Recording {
    pub fn start_close_with(
        self: &Arc<Self>,
        handle: u64,
        check: impl FnOnce(&TreeActor) -> Result<(), Error>,
    ) -> Result<Closing, Error>;
    /// changes: `self: &Arc<Self>`; is `start_close_with` then `finish`.
    pub fn close_with(
        self: &Arc<Self>,
        handle: u64,
        check: impl FnOnce(&TreeActor) -> Result<(), Error>,
    ) -> Result<Saved, Error>;
    pub fn when_saved_then(&self, pending: SavePending, then: Then);
    /// changes (body only): waits until no save is pending, then until history
    /// is built through every record appended after that.
    pub fn settle(&self) -> Result<RecordNo, Error>;
}

// crates/agentfs-core/src/tree_actor/recording/mod.rs
impl TreeActor {
    /// A file at `path` whose save is being decided: the session's refusal.
    pub(crate) fn pending_save(&self, tree: TreeId, path: &str) -> Option<SavePending>;
    /// The file as `by`'s pending saves leave it, else its saved map (see open_map).
    pub(crate) fn open_map(&mut self, tree: TreeId, path: &str, by: &WriterAt) -> Result<FileMap>;
}

// crates/agentfs-core/src/tree_actor/mod.rs: new fields of `Metrics`, in the metrics file.
pub struct Metrics {
    // ...existing fields...
    /// Saves begun and not landed: being decided, queued, or failed and awaiting a retry.
    pub pending_saves: u64,
    /// The age of the oldest of them, in nanoseconds; 0 with none.
    pub oldest_pending_save_ns: u64,
    /// Saves whose decision or land failed, or that were dropped unfinished, since open.
    pub failed_saves: u64,
}

// crates/agentfs-core/src/session.rs
impl TreeSession {
    pub fn start_close(&self, handle: u64) -> Result<Closing>;
    pub fn when_saved_then(&self, pending: SavePending, then: Then);
    pub fn close(&self, handle: u64) -> Result<Saved>; // unchanged: start and finish here
}

impl SessionGuard<'_> {
    pub fn open_map(&mut self, path: &str) -> Result<FileMap>;
    pub fn truncate_path(&mut self, path: &str, size: u64) -> Result<PathStep<()>>;
    /// changes: refused while the file at `path` has a pending save.
    pub fn unlink(&mut self, path: &str) -> Result<PathStep<()>>;
    /// changes: refused while `to` names a file with a pending save; `from`'s goes first.
    pub fn rename(&mut self, from: &str, to: &str, mode: RenameMode) -> Result<PathStep<()>>;
    /// changes: refused while `path` names a file with a pending save.
    pub fn replace_file(
        &mut self,
        path: &str,
        attrs: Attrs,
        src: &mut dyn std::io::Read,
        len: u64,
    ) -> Result<PathStep<Saved>>;
}

// crates/agentfs-fuse/src/tree_view/save_worker.rs
pub(super) type SaveJob = Box<dyn FnOnce() + Send + 'static>;
pub(super) struct SaveWorker { /* queue, thread */ }
impl SaveWorker {
    pub(super) fn start(tree: TreeId) -> io::Result<Self>; // thread `tree-saves`
    pub(super) fn submit(&self, job: SaveJob);            // FIFO; inline after drain
    pub(super) fn drain(&self);                           // joins; idempotent
}
// crates/agentfs-fuse/src/tree_view.rs: `View` gains `saves: Arc<SaveWorker>`.
```

**Invariants the implementer keeps.**

- `close_begin` checks for an earlier-begun save of the same file in flight before anything else, the tree's failed
  closes included; such a close is marked closing (it takes no calls and counts as open) and returns
  `Closing::Decide` with no snapshot. `finish` takes the snapshot at its turn; it may then save quickly under the
  lock as `close_begin` would.
- The actor keeps the order closes began (a counter under the lock) and each closing open's start time.
- `save_pending` never saves a failed save of a file ahead of an earlier-begun save of that file still in flight: it
  stays pending for a later step.
- The close epoch is a counter and condition variable in `Recording`, bumped after every landing or failure of a
  closing open; a waiter reads it under the actor's lock with the state it checks. Lock order: the actor, then the
  waiters of `when_saved_then`, then the epoch.
- `open_map` and `write_open` give f's pending content only to the writer whose pending saves they are: one pending
  save whose base did not move and with no page write gives its open's map, a clone; otherwise f's image, built under
  the lock as a page fill builds it. A write-open begun on pending content has no base step (`base_step: None`), so
  its close replays its own calls on the saved head and never saves its view.
- `TreeActor::close`, `truncate_path` and `close_writer` run in one hold. A session reaches the first two only after
  `pending_save` said no under the same hold. A client's end runs after its view drained its save worker; another
  client's save of the same file then in flight lands after it, replayed on the new head.
- The view: `release` calls `start_close`, answers the kernel, and submits a `Closing::Decide` as a job that calls
  `finish` and logs `tree SaveFailed` or `tree OrphanDropped` as `finish` did before. Each call that gets
  `PathStep::WaitFor` drops its guard and passes a retry of the whole request to `when_saved_then`, replying only from
  the retry; a setattr refused at its size retries only its size and times, so its metadata step is not made twice.
  A move out of the tree reads `open_map` and waits on the tree's unlink. Read-only opens use `open_map`.
- The view's thread drains the save worker after the session ends and before `ViewEnded`.
- `Recording::settle` is never called from a save worker or with a `SaveToDecide` held.

**Acceptance criteria, as tests written red first** (the `todo!()` bodies fail them), each with the mutation it must
catch.

New file `crates/agentfs-core/tests/tree_pending_saves.rs`, through `Recording` and `Sessions`, with two read-write
clients of the root where a test needs another client:

| Test | Asserts | Mutation it catches |
|---|---|---|
| `a_started_close_returns_before_its_decision` | a same-length overwrite's `start_close` is `Closing::Decide`; the saved content is the old one and `pending_saves` is 1 with `oldest_pending_save_ns` above 0 until `finish`, which returns `Saved::Commit`; then `pending_saves` is 0 | deciding and landing inside `start_close_with` |
| `an_open_by_its_client_reads_the_pending_content` | during the pending save, the closing client's `open_map` gives the new bytes and the other client's the old; after `finish`, both the new | `open_map` returning the saved map; returning pending content to every client |
| `a_write_open_of_a_pending_file_starts_from_its_content_and_saves_after_it` | a write-open during the pending save reads the new bytes (`read_open`); its append, closed after `finish`, leaves new bytes plus the append, in two saves in that order | the write-open's view taken from the saved map; its base step kept, so it saves its view |
| `an_unlink_of_a_pending_file_waits_for_its_save` | `unlink` answers `WaitFor`; the callback has not run when `when_saved_then` returns; after `finish` it has, its retried `unlink` is `Done`; after `settle`, the root's history has the save's commit, then the delete's | no `pending_save` check in `unlink` (the save is `OrphanDropped`); a callback run before the landing |
| `a_rename_of_a_pending_file_goes_first_and_onto_it_waits` | `rename(f, g)` is `Done` during the pending save and the save lands at `g`; `rename(x, g)` onto pending `g` answers `WaitFor` | checking `from` instead of `to`; no check |
| `a_replace_or_path_truncate_of_a_pending_file_waits` | `replace_file` and `truncate_path` of the pending file answer `WaitFor` with nothing recorded | either check missing |
| `two_closes_of_one_file_land_in_close_order` | two opens of f write different same-length bytes; A then B begin; B's `finish` on another thread, A's here; f holds B's bytes and the journal has A's save before B's | no same-file wait (B decides on the old head and lands first; A's redo leaves A's bytes) |
| `a_save_dropped_unfinished_stays_pending_and_holds_its_file` | dropping the `SaveToDecide` counts `failed_saves` 1, leaves `pending_saves` 1, runs a waiting callback; a `mkdir` in the tree saves it with the new bytes | no `Drop`; a drop that discards the save |
| `settle_waits_for_a_pending_save` | `settle` started on a thread while a save is pending returns a record at or past that save's step once `finish` runs | `settle` without the wait |
| `a_take_waits_for_a_pending_save_and_lands_at_its_built_event` | a fork with a pending save of f: a sync into f is refused as held while the save is pending and lands after the save is built; a sync of another file lands meanwhile | `closing` opens left out of the take's held files |

In `crates/agentfs-fuse` (no mount):

| Test | Asserts | Mutation it catches |
|---|---|---|
| `save_worker::tests::jobs_run_in_submission_order` | jobs record their numbers in submission order | a pool or a stack |
| `save_worker::tests::a_panicking_job_does_not_stop_the_worker` | a job after a panicking one runs | no catch per job |
| `save_worker::tests::drain_runs_every_queued_job_and_later_submits_run_inline` | every job queued before `drain` has run when it returns; one submitted after runs on the caller's thread | a drain that does not join |
| `tests::a_release_is_answered_before_its_save_lands` | with the view's worker held by a first job that waits on a channel, `View::finish` of a same-length overwrite returns `Ok`, `pending_saves` is 1, a read-only open of the file reads the new bytes and a stat of another file answers; released, `drain` lands the save | finishing on the request thread |

On real mounts (`ci/trees-close-open.py`, run by `ci/e2e-tree-pagecache.sh`, which then checks history after the
clean unmount with `ci/tree-git-test-lib.sh`): after a slow close (a 64 MiB file overwritten in place), a stat and a
read of the file from the same process and from another give the new size and bytes; after write tmp, close, rename
over the real file, reading the real file gives the new bytes; after a close and an unlink at once, the root's history
has the content's commit before the delete's. The mutations: the request thread answering opens from the saved map;
the delete not waiting.

**The conformance replay.** `crates/agentfs-conformance/src/mounted.rs`: `released()` keeps its stat, now only to
order the release's start before the gate's line, and its comment says so; `close_fd`'s `settle` then waits for the
save through `Recording::settle` over the existing gate channel (`run_take_gate`'s `settle` line). No new gate verb.

**The figure it moves.** `close_frag` and `write_1m` with `--probe`: the same view's longest call on another file
falls from 377 and 860 ms toward the other view's 124 and 87 ms (a prediction: those are the runs' next-longest
waits, for the volume lock). Measured on a release build:
`sudo python3 ci/trees-figures.py --view V --filesystem ext4 --metrics M --probe --probe-view F --only close_frag`,
then `--only write_1m`, reading `probe.same.*.max_ms` and `requests.release.longest_ms`. Reported, not gated.

**Owned paths.** `crates/agentfs-core/src/tree_actor/call_path.rs`, `crates/agentfs-core/src/tree_actor/recording/mod.rs`,
`crates/agentfs-core/src/tree_actor/recording/pages.rs`, `crates/agentfs-core/src/tree_actor/mod.rs`,
`crates/agentfs-core/src/session.rs`, `crates/agentfs-core/tests/tree_pending_saves.rs`,
`crates/agentfs-core/tests/tree_close.rs`, `crates/agentfs-fuse/src/tree_view.rs`,
`crates/agentfs-fuse/src/tree_view/save_worker.rs`, `crates/agentfs-conformance/src/mounted.rs`,
`ci/trees-close-open.py`, `ci/e2e-tree-pagecache.sh`, `docs/trees.md`, `docs/telemetry.md`, `docs/library/core.md`,
`docs/library/filesystem.md`, `docs/architecture.md`, `docs/guarantees.md`.

**Docs it carries** (AGENTS.md: docs travel with the code): trees.md's "Recording and durability" and "The view"
(the release begun before it is answered, the save worker, pending content, the delete's wait); the metrics keys in
telemetry.md; `TreeSession::start_close`, `when_saved_then`, `open_map` and `PathStep` in library/core.md's
"Sessions"; the view's threads in library/filesystem.md; the architecture rows of `session.rs`, `call_path.rs` and
`tree_view.rs`. The git-model §3 change is reported, not made.

**Gates.** `cargo fmt --all --check`; `cargo clippy -j 2 -p agentfs-core -p agentfs-fuse -p agentfs-conformance
--all-targets -- -D warnings`; `cargo test -j 2 -p agentfs-core -p agentfs-fuse -p agentfs-conformance -p
agentfs-model`, which runs the model-verification regression tier (no model rule changes, so no full sweep); locally
with root and `/dev/fuse`: `ci/e2e-tree-pagecache.sh`, `ci/e2e-trees.sh`, and `ci/e2e-tree-replay.sh` on the
regression traces with a failpoints build. CI: `ci.yml` (FUSE mounts, `ci/e2e-trees.sh`, `ci/e2e-trees-ordering.sh`),
`phase-1.yml` (`ci/e2e-tree-replay.sh`, `ci/e2e-tree-conformance.sh`, `ci/e2e-tree-pagecache.sh`,
`ci/e2e-tree-restart.sh`), `image.yml` (`ci/e2e-trees-tracer.sh`).

### view-save-2: measured, and tried under failure

**Goal it delivers.** Evidence that nothing waits on another file's save, that each file's saves land in order under
load, and that a failed, panicking or killed decision loses nothing; then this page's behaviour moved into the
library and human docs and this page deleted.

**Exact signatures.** No Rust signature. New `ci/trees-figures.py` probe legs, each run only with `--probe`, each
reporting `spread` figures (`count`, `p50_ms`, `p99_ms`, `max_ms`) under its name in the report:

| Leg | Workload | Figure |
|---|---|---|
| `creates_during_close` | `close_frag`'s close, with a 4 KiB file created, written and closed in its directory every 10 ms on another thread until the close's save lands | the longest create and close(2) |
| `rename_save` | a 64 MiB file written as `tmp`, closed, renamed over an existing 64 MiB file, with creates in the directory meanwhile | the longest create, the rename's own time, and the real file's bytes checked |
| `many_closes` | 64 files of 4 MiB, each overwritten in place and closed while `close_frag`'s save is decided | the longest close(2); each file's bytes checked after the save lands |
| `other_view_close` | with `--probe-view`, the same `close_frag` close in the other view at once | the other view's longest call, against 124 ms today |

New `ci/trees-pending-saves.py MOUNT HISTORY_GIT METRICS` with `ci/e2e-tree-pending-saves.sh`, on a failpoints build
with `AGENTFS_FAILPOINTS`:

- `tree.close_decide=error`: the client reads its new content; `failed_saves` 1 and `pending_saves` 1 in the metrics
  file; a sync into the file from the fork's view waits; the next step in the tree saves it and the sync lands.
- `tree.close_decide=panic`: the view goes on; a later save of another file lands; the panicked save is saved at the
  next step.
- `tree.close_decide=kill`: after the restart, history has the save.

**Acceptance criteria, red first.** Each leg and each case fails before its code is written (the script lacks it).
The mutations it must catch, tried locally on the tracer's code: finishing on the request thread (`creates_during_close`'s
longest create rises to the close's length); the same-file wait removed (`many_closes`' bytes check, run with two
closes of each file); no `Drop` for `SaveToDecide` (the panic case's next-step save never comes, the script's bounded
wait fails it); the take's held files ignoring `closing` opens (the error case's sync lands before the save).

**The figures it moves.** The tracer's, now with legs: `creates_during_close` and `many_closes` longest near a plain
create's and close's (a few ms), not the decision's 377 ms; `other_view_close` no worse than 124 ms. Measured as in the
tracer, `--only close_frag` then each leg; reported in docs/performance.md, never gated (the owner, 2026-11-20).

**Owned paths.** `ci/trees-figures.py`, `ci/trees-pending-saves.py`, `ci/e2e-tree-pending-saves.sh`,
`.github/workflows/phase-1.yml`, `docs/performance.md`, `docs/runbooks/recording-failures.md`,
`docs/target-architecture/future/view-save-worker.md` (deleted), `docs/target-architecture/README.md`,
`docs/SUMMARY.md`.

**Docs it carries.** The figures in performance.md; the pending-save failure in the recording-failures runbook (the
gauge, the counter, what an operator does); this page deleted with its rows in the target-architecture README and
SUMMARY.md.

**Gates.** `python3 ci/trees-figures.py --self-test --budget ci/trees-budget.json`; locally with root and
`/dev/fuse` on a failpoints build: `ci/e2e-tree-pending-saves.sh`; the legs once on a release build. CI:
`phase-1.yml`'s new step, and `image.yml`'s `ci/e2e-trees-tracer.sh` (the figures, read and never gated beyond
`create`'s bar).

## New terms

| New term | Meaning | Why needed | Existing term it could be |
|---|---|---|---|
| pending save | a closed file whose save has not landed: being decided, queued behind an earlier save of the file, or failed and awaiting its retry | one name for what the client reads and rule 9 holds | code: `closing` plus `pending_closes`; the owner's "changes pending" |
| save worker | a view's thread that decides and lands its pending saves | where a decision runs | like `tree-takes`, `tree-gate` |
| close epoch | a counter bumped at every landing or failure of a pending save | what same-file order and `settle` wait on | like history's work epoch (`wait_for_work`) |
