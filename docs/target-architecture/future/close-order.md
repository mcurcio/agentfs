# Close order and the pending fold

**Proposed, not built.** Two changes to [pending saves](../../trees.md#pending-saves). The journal records when each close begins, so a restart lands a file's saves in the order their closes began, as a live volume does. And an open that must fold its client's pending saves folds them with the volume's lock released. The commit that lands the last slice deletes this page and moves the behaviour into [trees](../../trees.md), [performance](../../performance.md) and the [library pages](../../library/core.md).

**Layer: System.** The domain is unchanged: model rules 1 and 9 stand, with no new rule (crates/agentfs-model/MODEL.md).

## Requirements and background

- **Guidance, 2026-11-23** (OWNER-GUIDANCE.md, "Performance"), on recording each close's start in the journal and recovering in that order: "option A is ideal. that gets us closer to a true oplog style process (a proven strategy)".
- **Principle 1** (AGENTS.md): a change is recorded before the call returns. A close does not wait for the disk (the owner, 2026-11-13 and 2026-11-21).
- **Guidance, 2026-11-22**: "The client should be free to read and write its files, blissfully unaware of other processes interacting with the filesystem."
- **Guidance, 2026-11-23**: a delete of a file with a pending save waits for that save.

## Prior art

- **Write-ahead logs decide order by the log, never by memory.** ARIES's analysis pass rebuilds the table of live transactions from the log, and redo repeats history in log order. PostgreSQL's commit order is the order of its commit records in the WAL; two-phase commit writes a `PREPARE` record so a prepared transaction survives a restart. MongoDB's oplog is applied in its timestamp order. agentfs follows them: a close's start is a record, and its number is the close's order.
- **Readers pin a snapshot under the lock and do the work without it.** RocksDB's `Get` takes a reference to its `SuperVersion` (memtables and file version) under the database mutex, then reads and resolves merge operands without it. parking_lot's `MutexGuard::unlocked` releases a held lock around work and takes it again. A close's decision already works this way here (`CloseWork::decide`).

## The close record

| | |
|---|---|
| Entry | `CloseBegun { handle }`: no tail, about 100 bytes (a 44-byte header and the head) |
| Appended | Under the actor's lock, once per open: at a close's start (`close_begin`), before any save; at a client's end, for each open it closes, in open order; at restart, for an abandoned open that has none, before its save |
| Its number | The close's order: `Handle::close_order` is this record's number, and the in-memory counter (`closes_begun`) goes. A file's saves land in it, live and at restart |
| Durability | A call's: one `write(2)`, no fsync. It is in the kernel when the release is answered, so it survives a kill, and durable at the next fsync, FSYNCDIR, `O_SYNC` write or builder batch |
| Not a step | No writer, no commit, no lag, no pointer. The builder skips it and it ends no open |

**Why no fsync is enough.** A power cut loses a suffix of the journal, so the close records that survive are a prefix of those appended. Recovery orders by them, which is the order an uninterrupted run that stopped after the last surviving record would land them in. An fsync that returned covers every close record before it, so after it the order survives.

**A failed append** fails the close with nothing done, as a call's record does: the open stays open, not closing, and is closed at its client's end or at restart.

## Recovery

The fold sets each live open's `close_begun` from its record. A close record naming no live open, or a second one for an open, is `Error::Format`, as a call naming no open is. Recovery then closes abandoned opens in one order: those with a close record in record order, then those without in open order, which is the order a client's end gives them. Failed closes saved again at their tree's next step go in the same order, not in the order they failed. Two overlapping pending saves of one file then land as their client last read them, even after a kill or a power cut that followed an fsync.

The state format becomes `agentfs-trees-15`. A format 14 journal has no close records and does not open, as no earlier format does.

## Compaction

- **Folded.** The compactor's fold (`fold_through`) keeps the record on its open, and the checkpoint keeps it in the open's record (`OpenRecord::close_begun`) while the open lives. A fold from the checkpoint restores it.
- **Never carried.** The checkpoint names no close record as live, so none goes into a carry file. It is dead once folded and leaves with its segment, counted as an open's own record is.
- **No step heads, no switch records.** It moves no pointer.

## Settle

`Recording::settle` waits only for closes begun before it: those whose record number is below the journal's next record when it starts. A close begun after it, on any file, no longer holds it.

## The fold off the lock

**Rule: an open reads no journal under the actor's lock.** Under the lock, an open of a file by its own client takes what it reads (`TreeActor::open_content`): the saved map, or the one pending save's view whose base did not move with no page write, each a clone; else a pending fold (`PendingFold`): the saved map, the client's pending saves in close order with their call counts, the file's page writes and a reader of the journal, all snapshots. The caller folds with the lock released (`ActorGuard::unlocked`); `SessionGuard::open_map` and `SessionGuard::write_open` do so.

- **A read-only open** is its content as of the hold that took the snapshot: a pending save takes no more calls, so a landing meanwhile changes nothing it reads.
- **A write-open** takes two holds. The first records the open and takes the snapshot (`write_open_begin`); the handle is live but not yet given to the client. The second sets its view (`write_open_view`): the fold, with each page-write run of the file recorded after the open applied, as every open view of the file got it at its call. A client's end in between closes the open, and the write-open fails. A failed fold closes the open unchanged and fails the call.
- **The library's one-hold calls** (`TreeActor::write_open`, used by the CLI and tests) fold under the lock, measured as a `page_image` hold. `write_open_truncating` never folds: its truncate empties the view.

**Why not keep each fold.** A kept fold is content derived from the journal. Every landing by another writer, every page write and every take of the file would have to invalidate it, and a missed one serves stale bytes: a cache that can win over its source. It also holds a map per pending save. Each open folds again instead.

## What it costs

- One close record per close, and per open a client's end or a restart closes: about 100 bytes and one `write(2)` under the lock. The many-file `create` figure makes one more append per file.
- A restart reads the close records. Nothing else reads them.
- An open that folds holds the lock for its snapshot, O(segments + the file's page writes), and a write-open takes it twice. The fold's time moves to the opening view's thread, and the reader keeps the journal's segments linked until it ends.
- The opening view still waits for its own fold: a view has one request thread, so that view's other requests wait as long. Other views and the volume do not.

## What is left out

- **A refused call still waits for every save of its file.** An unlink, a rename onto the file, a path truncate, a time set or a move out of the tree waits until no save of the file is in flight, so a client that keeps closing the file can hold it. Bounding the wait to saves begun before the refusal does not help alone: the retry is refused again by the later save. Letting the retry go ahead drops that later save's version from history (the save lands on an unlinked file). The owner's answer of 2026-11-23 covers the save pending when the delete arrives, not later ones; this needs his call.
- **The conformance replay still settles after every close.** Its invariants read git after each call. Replaying calls over a pending save needs another checkpoint in the harness, and the close record does not make that cheaper. The restart tests below cover this defect.
- **The fold still runs on the opening view's request thread.**

## New terms

| Term | Meaning | Why it is needed | Existing term |
|---|---|---|---|
| Close record | The journal's `CloseBegun` record: a close's start, its number the close's order | The order must be on record to survive a restart | "a close begun" ([trees](../../trees.md#pending-saves)) |
| Pending fold | What an open of a file with pending saves folds, taken under the lock and folded without it | Names the snapshot the caller must fold off the lock | "the fold of its client's pending saves" ([trees](../../trees.md#pending-saves)) |

## Frozen slice pages

Two slices in one lane, in order; each lands green alone. The 120-line cap is the design's above; these pages go with it when the last slice lands. The interfaces are committed as compiling code with `todo!("close-order-N")` bodies and `#[expect(dead_code)]` (an unfulfilled expectation fails clippy, so each slice deletes the attributes it fulfils). Gates for each: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` (the model's regression tier and the conformance replay run in it), then CI: `ci.yml` (FUSE mounts, crash injection, phase 1 tree views), `phase-1.yml` (power loss on the replay), `image.yml` (tree figures), `ci/e2e-tree-pending-saves.sh` and `ci/e2e-tree-recording-faults.sh`.

### close-order-1: the close record, recovery in close order, settle bounded

**Owned paths:** `crates/agentfs-core/src/tree_actor/journal/{mod,fold,compaction}.rs`, `crates/agentfs-core/src/tree_actor/history/{builder,compactor}.rs`, `crates/agentfs-core/src/tree_actor/history/compactor/tests.rs`, `crates/agentfs-core/src/tree_actor/recording/mod.rs`, `crates/agentfs-core/src/tree_actor/mod.rs`, `crates/agentfs-core/src/tree_actor/call_path.rs`, `crates/agentfs-core/tests/tree_pending_saves.rs`, `ci/e2e-tree-pending-saves.sh`, `ci/trees-pending-saves.py`, `docs/trees.md`, `docs/runbooks/upgrading-the-sidecar.md`, `docs/library/core.md`, `docs/architecture.md`.

```rust
// journal/mod.rs (committed)
pub(super) enum Entry { /* ... */ CloseBegun { handle: u64 }, /* ... */ }
pub(super) struct OpenRecord { /* ... */ pub close_begun: Option<RecordNo> }
// journal/fold.rs (committed; the fold's arm is this slice's)
pub(in crate::tree_actor) struct LiveOpen { /* ... */ pub(in crate::tree_actor) close_begun: Option<RecordNo> }
// fold(): Entry::CloseBegun { handle } sets opens[handle].open.close_begun = Some(seq);
//         Error::Format for no live open `handle`, or one that has a close record.
// tree_actor/mod.rs
const FORMAT: &str = "agentfs-trees-15\n";
// TreeActor::closes_begun is deleted. Recovery closes abandoned opens sorted by close_rank,
// and save_pending saves its due closes sorted by close_rank, not in arrival order.
// recording/mod.rs
pub(in crate::tree_actor) struct Handle { /* ... */ pub(in crate::tree_actor) close_order: Option<RecordNo>, /* ... */ }
impl Handle { pub(in crate::tree_actor) fn recovered(folded: FoldedOpen) -> Self } // close_order = open.close_begun
/// The one order a file's saves land in, live and at restart: opens with a
/// close record in its order, then those without in open order.
pub(in crate::tree_actor) fn close_rank(handle: u64, h: &Handle) -> (bool, Option<RecordNo>, u64);
impl TreeActor {
    /// Appends `CloseBegun { handle }` unless the open has a close record,
    /// and sets `close_order`. No fsync. Called by `close_begin` before
    /// anything else it does, by `close_writer` for each open it closes (all
    /// appended before any save), and by recovery through `close`.
    fn record_close_begun(&mut self, handle: u64) -> Result<RecordNo>;
    pub(in crate::tree_actor) fn save_in_flight(&self, tree: TreeId, object: u64, before: Option<RecordNo>) -> bool;
    /// A close being decided or queued whose record is below `before`.
    pub(in crate::tree_actor) fn saves_in_flight(&self, before: RecordNo) -> bool;
}
// call_path.rs: Recording::settle(&self) -> Result<RecordNo, Error>, unchanged signature:
// its first hold reads `durability.appended().next()` as `before`, then waits on the
// close epoch while `saves_in_flight(before)`.
```

Invariants: at most one close record per open; every close that leaves its open closing or pending has one; `close_order` is `None` only for an open whose close has not begun. Error modes: a failed close-record append fails `close_begin` or `close_writer` with that open unchanged. The builder and `Replay::apply` skip the record; `Live::names` never names it.

| Acceptance test (red first) | Mutation probe it catches |
|---|---|
| `tests/tree_pending_saves.rs::a_restart_lands_two_pending_saves_in_close_order`: `f` saved as `SEED`; client `a` opens `h1` then `h2` on `f`, writes `AAAA` through `h1` and `BBBB` through `h2` at 0; `start_close(h2)` then `start_close(h1)`, neither finished; `opened(&a, "f") == AAAA`; `rec.durable_all()`; both saves dropped unfinished (`h2`'s first, as the save worker reaches them), the sessions and the volume dropped (a kill: nothing lands them); `Recording::open`: `f == AAAA`, and `f`'s last two root commits are `h2`'s save, then `h1`'s | recovery closes in handle order (sort removed): `BBBB`; the fold's arm leaves `close_begun` unset: `BBBB`; `close_begin` appends no record: `BBBB` |
| `tests/tree_pending_saves.rs::a_restart_keeps_a_client_ends_close_order`: client `c`'s save of `f` begun and held; client `a`'s open of `f` writes `AAAA` and `a` ends (its open waits behind `c`'s save); client `b`'s open of `f` (opened before `a`'s) writes `BBBB` and its close begins; drop all, reopen: `f == BBBB` | `close_writer` appends no record: `a`'s open lands last, `AAAA` |
| `history/compactor/tests.rs::a_checkpoint_keeps_each_live_opens_close_record`: two opens of one file closed in reverse order, then a `mkdir` built and compacted past both close records (segments rolled small); the published checkpoint's open records carry both, a reopen lands `AAAA` | `checkpoint_at` writes `close_begun: None`: `BBBB` |
| `tests/tree_pending_saves.rs::failed_saves_land_in_close_order`: as the first row, but the saves dropped unfinished `h1`'s first, then the tree's next step (a `mkdir`) saves both: `f == AAAA` | `save_pending` keeps arrival order: `BBBB` |
| `journal/fold.rs` test `a_close_record_naming_no_live_open_is_refused`: a record for an unknown handle, and a second one for an open, each fold to `Error::Format` | the arm ignores an unknown handle or overwrites a second record |
| `journal/mod.rs` test `a_close_record_is_appended_without_an_fsync`, beside `a_call_is_appended_without_an_fsync`: a close's start appends one `CloseBegun` frame and the durable mark does not move | an fsync at the close's start |
| `call_path.rs` test `settle_waits_only_for_closes_begun_before_it`: save `S1` held in its decision; `settle` on a thread, waiting (`on_wait_with`); `S2` of another file begun and held; `S1` released: `settle` returns while `S2` is held (a 60 s `recv_timeout` only fails the test, then `S2` is released) | `settle` waits on every save in flight |
| `ci/e2e-tree-pending-saves.sh`, case `order` then `order-restarted` (`ci/trees-pending-saves.py`): the mount with `tree.close_decide=sleep(600000)`; two fds on `order` (saved `SEED`), `AAAA` through the first, `BBBB` through the second, closed second then first; a fresh open reads `AAAA`; `fsync` of the mount's root; `kill -9`; restart with no failpoint: the file reads `AAAA` and its last two commits are the second fd's save, then the first's | as the first row, on a real mount and process |

**Figure:** one close record per close (about 100 bytes, one `write(2)` under the lock). The `create` release figure (`image.yml`) is reported before and after; a miss beyond its noise is a finding.

**Docs (this slice):** trees.md "Pending saves" (the restart limit becomes "a restart keeps close order"), "The journal" (the record, its durability, compaction, format 15), "Recording and durability"; the runbook's format number; core.md's settle.

### close-order-2: the fold off the lock

**Owned paths:** `crates/agentfs-core/src/tree_actor/recording/{mod,pages}.rs`, `crates/agentfs-core/src/tree_actor/call_path.rs`, `crates/agentfs-core/src/tree_actor/mod.rs`, `crates/agentfs-core/src/session.rs`, `crates/agentfs-core/tests/tree_pending_saves.rs`, `crates/agentfs-fuse/src/tree_view.rs`, `ci/trees-figures.py`, `docs/trees.md`, `docs/performance.md`, `docs/library/core.md`, `docs/library/filesystem.md`, `docs/architecture.md`; deletes this page and its rows in `docs/target-architecture/README.md` and `docs/SUMMARY.md`.

```rust
// call_path.rs (committed signature): the guard keeps the lock as an Option and
// a reference to its Recording; each side of `unlocked` reports overlays and
// is measured as a drop and a lock are.
impl ActorGuard<'_> { pub(crate) fn unlocked<R>(&mut self, work: impl FnOnce() -> R) -> Result<R, Error>; }
#[cfg(test)] impl Recording { pub(crate) fn on_unlocked_with(&self, hook: impl Fn() + Send + 'static); } // runs inside every `unlocked`, lock released
// tree_actor/mod.rs
pub(crate) use call_path::ActorGuard;
pub(crate) use recording::{OpenContent, PendingFold, WriteOpenStart}; // committed; drop the expect
// recording/mod.rs (committed)
pub(crate) struct PendingFold { saved: FileMap, closes: Vec<(u64, u64)>, pages: Option<(Vec<(u64, pages::Run)>, u64)>, reader: CallReader }
impl PendingFold { pub(crate) fn fold(self) -> Result<FileMap>; } // the body of pages_fold_saves, from snapshots
pub(crate) enum OpenContent { Ready(FileMap), Fold(PendingFold) }
pub(crate) enum WriteOpenStart { Ready(u64), Fold { handle: u64, fold: PendingFold } }
impl TreeActor {
    pub(crate) fn open_content(&self, tree: TreeId, path: &str, by: &WriterAt) -> Result<OpenContent>; // replaces open_map, deleted
    pub(crate) fn write_open_begin(&mut self, tree: TreeId, path: &str, by: &WriterAt, create: Option<Attrs>) -> Result<WriteOpenStart>;
    pub(crate) fn write_open_view(&mut self, handle: u64, folded: FileMap) -> Result<()>;
    pub fn write_open(&mut self, tree: TreeId, path: &str, by: &WriterAt, create: Option<Attrs>) -> Result<u64>; // unchanged: begin, fold in a Hold::PageImage section, view
    pub fn write_open_truncating(&mut self, tree: TreeId, path: &str, by: &WriterAt, create: Option<Attrs>) -> Result<u64>; // unchanged: begin, no fold, truncate
    fn pending_content(&self, tree: TreeId, object: u64, by: &WriterAt) -> Result<Option<OpenContent>>;
    fn pending_fold(&self, tree: TreeId, object: u64, closes: &[u64]) -> Result<PendingFold>; // replaces pages_fold_saves: snapshots only
}
// session.rs
pub struct SessionGuard<'a> { actor: ActorGuard<'a>, tree: TreeId, writes_as: Option<WriterAt> } // the Box<dyn DerefMut> goes
impl SessionGuard<'_> {
    pub fn open_map(&mut self, path: &str) -> Result<FileMap>; // unchanged signature; folds through ActorGuard::unlocked
    pub fn write_open(&mut self, path: &str, create: Option<Attrs>) -> Result<u64>; // unchanged signature; same
}
```

Invariants: no journal read under the lock on an open's path but `TreeActor::write_open`'s one hold; a handle from `write_open_begin` reaches no client before `write_open_view`; the view it sets is the fold plus the file's page-write runs whose record is after the handle's `Open` record, in record order. Error modes: a failed fold closes the open (`TreeActor::close`, unchanged, no calls) under the retaken lock and returns the fold's error; `write_open_view` answers `NotFound` when the client ended meanwhile; a lock that cannot be taken again is `Stopped`. `SessionGuard::open_map` and `write_open` say in their docs that the guard may be released inside them. The view's `create` replies with the attributes of the object it opened, never a path lookup after the open (a rename may come between).

| Acceptance test (red first) | Mutation probe it catches |
|---|---|
| `call_path.rs` test `an_opens_fold_runs_with_the_lock_released`: two pending saves of `f` by one client, held; `on_unlocked_with` records whether `try_lock` of the actor succeeds; the client's `open_map("f")` reads the fold; the hook ran once, with the lock free | `SessionGuard::open_map` folds under the guard (hook never runs); `unlocked` keeps the lock (`try_lock` fails) |
| `call_path.rs` test `a_write_opens_fold_keeps_page_writes_made_meanwhile`: as above with `write_open`; inside the hook another client's write-open of `f` makes a page write at 8; the new open's view (`read_open`) is the fold with that page write | `write_open_view` ignores runs after the open |
| the existing `an_open_reads_the_fold_of_its_clients_two_pending_saves`, `an_open_reads_its_pending_save_on_a_moved_base`, `a_write_open_of_a_pending_file_starts_from_its_content_and_saves_after_it` and `an_open_of_a_pending_mapped_save_reads_it_without_another_writers_calls` stay green | a fold that drops page-write pieces or close order |
| `ci/trees-figures.py` probe leg `pending_open` (`--probe --probe-view FORK`, warns, no bar): 1,000,000 64-byte appends in one open and its close, one more append-open, append and close, then a read-only open of the file: reports `open_ms`, the other view's longest probe call and the longest `page_image` hold | the fold back under the lock: the other view's longest call returns to the open's length |

**Figure it moves:** an open of a file with two pending saves (the review's case). Before: the lock held for the whole fold, 928 and 1,098 ms, and a stat in the fork's view waited as long (44 to 45 ms before pending saves). After: the open's hold is its snapshot, under a millisecond, and the fork's view stays near its 45 ms; the open itself still takes about a second on its own view.

**Docs (this slice):** trees.md "Pending saves" (the open row: folded with the lock released; the opening view waits, no other does), performance.md (the bullet and the new leg's figures), core.md and filesystem.md (`open_map` may release the lock), architecture.md (session.rs and recording rows); this page and its index rows deleted, and every code comment naming it (`grep -rn close-order.md crates`) names docs/trees.md instead.
