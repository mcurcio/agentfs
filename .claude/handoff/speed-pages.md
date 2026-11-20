# Frozen pages: tree views speed, B2 to B4 (code architect, 2026-11-14)

Inside the frame of `speed-design.md` v4 and the owner's answers in `speed-brief.md` (G1: queue the change at the
call, power-cut safe at close, flush or fsync or once the builder catches up, git built by a trailing process; Q1 = A,
hashed sub-folders). Every finding of v4's last adversarial review is resolved below (table in §1). Nothing here is
built or measured: every figure is a hypothesis with a kill condition.

Repo `/home/user/wt-step6`, branch `trees/module-cut-step-6` (PR #39). Interfaces committed as compiling code, bodies
`todo!("B2a"|"B2b")`: `a78ac03` and `3a35108` (§2). Rails for every implementer: the m1-impl-brief Rails section with
the owner override (regression tier only, never the full sweep), `CARGO_INCREMENTAL=0`, `df -h /` before each build,
one container (`ci/model-docker.sh`).

## 0. Slice order, and what may run at once

| # | Slice | Package(s) | Needs | Why here |
|---|---|---|---|---|
| 1 | **B2a** journal: no per-call fsync, group commit, deferred replies | agentfs-core, agentfs-fuse (+ CLI, ci scripts) | B1 | The journal is the package every later slice imports; it is green alone and halves B2's risk (B2 as one bead is over the per-bead ceiling) |
| 2 | **B2b** the builder (tracer): queue the step, build history behind it, takes off the actor, lag gate | agentfs-core, agentfs-fuse, agentfs-conformance (tracer exception) | B2a | The riskiest seam: the commit leaves the call path |
| 3 | **B4** builder costs its change (Q1-A) | agentfs-core (history format), agentfs-conformance (reader) | B2b | Makes the creates bar reachable; small and isolated |
| 4 | **B3** file maps and segments | agentfs-core (journal, recording, saved state), agentfs-fuse (reads) | B2b | The 1 GiB bar; touches the journal's layout, so after B4's churn in history |

**Concurrency: none.** B3 and B4 own disjoint source files (§5, §6), but both build `agentfs-core` into the one cargo
target (docker volume `agentfs-model-target`), and the rails allow one container and no second target (disk). Shared
build output means not disjoint: run them in sequence. B3 before B4 is equally valid; B4 first is cheaper to review.

## 1. Review findings, where each is resolved

| Finding (v4 review) | Resolution | Where |
|---|---|---|
| 1 gate contradiction; fsync under the actor lock | Lock discipline lives in one type, `Recording`: under the lock a call decides, appends (one `write`, no fsync) and applies. Durability waits run on the journal's waiter thread; lag-gate waits on the gate's thread; the FUSE thread is handed a callback (`durable_then`, `admit_then`) and never blocks. The actor has no fsync method. The view stays single-threaded, which keeps close-to-open ordering (a later request is answered after the RELEASE before it) | §2, B2a, B2b |
| 2 visibility held by global lag | The gate is **per tree** and holds only calls that start new work (create, write-open, `O_TMPFILE`, link, rename, unlink, path truncate). It never holds a write call, a FLUSH or a RELEASE, so a save is visible at RELEASE whatever the lag. Cost stated: past its limit a tree's next create or open waits for the builder; other trees run | B2b |
| 3 take starvation; no "held" refusal | `land` refuses `FilesChanged`, `Held`, `HeadMoved`, `EntryGone`. Progress: takes are one at a time, so between decision and land only direct steps run; a take lands at the first built event after which none of its inputs changes before it lands. It starves only while its files change faster than the lag plus its decision, which is G7's "keeping the fs busy with real work". A reservation fallback is named and not built | B2b |
| 4, 5 arithmetic | Every figure below is a hypothesis with a kill condition, measured with the builder running; the ext4 Intel runner's 3.5 ms miss is stated | each page |
| 6 two apply functions; no digest; read path | One apply per concern, used by actor and builder: `close_calls` (which calls a close applies, recorded by reference in the step) then `Call::apply` (B2b) / `FileMap::apply` (B3). The save record carries `len` and `crc32` of the result; the builder refuses a mismatch (`Disagrees`). Merged take bytes are in the take record. Built runs are read from the private checkout through an open `BaseFile` (B3) | B2b, B3 |
| 7 model equality of delayed takes; Q1-A hash | A decision records every key it read (touched keys plus the held keys that cut a sync or blocked an earlier entry) and lands only if none changed and none it writes is held: then it equals an atomic take at the land, by construction. A library test compares decide-early with decide-late. Q1-A's fanout is fixed (two levels of 256), hashed from the file's name; names are flat, so no directory rename exists to rehash | B2b, B4 |
| 8 determinism | Alignment runs once per step, in the builder, before the step's built-log entry; nothing reads an unbuilt step, so a rebuild after a crash under another `git` changes nothing anyone saw. After a power cut, overlapping opens' recovered close order may differ from what readers saw (G1 cost) | B2b |
| 9 durability details | One buffer, one `write_all_at` per frame; recovery reads every valid frame, the builder only to the mark; the builder fsyncs fast-import's pack files and `objects/pack` itself; no repack runs on a tree volume, scratch refs `refs/build/*` are deleted after install and at recovery; segment creation fsyncs the directory before the mark covers it (B3) | B2a, B2b, B3 |
| 10 records per step kind | `Change` covers create, link, save, rename, unlink, fork, take; path truncate is a save (write-open, truncate, close); fork's head is a record number the builder resolves, no wait; recovery and client-death saves stay Direct saves, as git-model §3 says; chmod, chown, utimes answer `EOPNOTSUPP` (tree_view `setattr`), unchanged | B2b |
| 11 wedged builder; estimate | Waiters wait unboundedly on a wedged builder (no timer stands in for an event); the oldest-unbuilt-age gauge is its own judge, and the runbook kills the child, which fails the batch and releases waiters. Lag is recomputed at every built event from two measured rates: ns per step and ns per byte | B2b |
| 12 torn-tail strawman | The mark exists to tell media corruption inside the durable region from a torn tail; prefix-only would silently truncate the former. Acknowledged records do form a prefix | B2a |
| 13 docs of record | AGENTS.md principle 1, guarantees.md, recording.md, trees.md, git-model §3, §6, §12, OWNER-GUIDANCE.md (the 2026-11-13 speed answers, not yet recorded) | B2a, B2b docs |
| suspicion: fsync of a read-only fd | `durable_all` | B2a |
| suspicion: two homes for the built point | The built log is the one home; a checkpoint's `built` is only the redo start, written after the built log held it durably | B2b |
| suspicion: restart cost | Replays from the checkpoint, at most the journal (64 MiB plus live opens' calls); measured in B2b's restart test | B2b |
| suspicion: space during a 1 GiB close | State area needs about 3x the file (journal, pack, checkout); figures' loop filesystem sized for it | B3 |

## 2. Shared primitives (committed, bodies `todo!`)

| Primitive | File | Responsibility | Consumers now / scheduled |
|---|---|---|---|
| `RecordNo` | `tree_actor/journal/durable.rs` | The one numbering: calls, steps, durable mark, built point | B2a journal; B2b builder, History, takes; B3 file maps; B4 none new |
| `Durability`, `DurableError` | same | Group commit: `mark`, `appended`, `make_durable`, `make_all_durable` | B2a (journal, Recording); B2b builder before each batch; B3 segments |
| `Recording`, `Then`, `Progress` | `tree_actor/call_path.rs` | The call path served: the actor behind one lock, durability and gate waits outside it; `create`, `open`, `actor`, `durable(_then)`, `durable_all(_then)`, `admit(_then)`, `history`, `progress`, `take_next`, `wait_for_work`, `settle` | B2a: view, state_area, CLI; B2b: gate, takes, conformance; B3 reads |
| `History`, `BuiltSnapshot`, `Lag`, `TreeLag`, `Health`, `BuildError`, `WaitError` | `tree_actor/history/built.rs` | The one owner of the Git store: `built_through`, `wait_built`, `snapshot`, `lag`; a snapshot exists only at or before the built point | B2b takes, CLI, tests; B3 base files; B4 snapshot format |

The exact text is in those files; it is the frozen surface. Crate-private additions are on each page.

## 3. B2a: the journal queues calls; durability by group commit

**Observable behaviour.** A write returns once its frame is in the kernel; FLUSH, FSYNC, FSYNCDIR, an `O_SYNC` write
and recovery make records durable; concurrent waits share one fsync; the view's FUSE thread never waits on an fsync.
Steps still build their commit in place (B1) until B2b; they write and fsync objects under the lock until then (a
stated interim cost), but their journal record is no longer fsynced.

### Frame format (`AFJ3`; `format` file `agentfs-trees-5`, older refused)

```text
frame  = header head tail
header = "AFJ3" | u32le salt | u64le record | u64le mark | u32le flags
         | u32le head_len | u32le tail_len | u32le crc32(head + tail) | u32le crc32(first 40 header bytes)   (44 bytes)
head   = JSON of Entry       (Record.seq leaves the JSON: the header is the record number's one home)
tail   = raw bytes of the record's call or runs
mark   = Durability::mark() when this frame was appended
flags  = 0 (B3 defines bit 0, "carried")
```

Open rule, at the first invalid frame at offset X after the last valid record `r`: if any later valid frame of this
salt has `mark > r` (a record in the hole was durable when it was written), `CorruptJournal(X)`, nothing changed;
else a torn tail: saved to `calls.wal.torn`, truncated, synced (as today). Recovery reads every valid frame, durable or
not (a killed agentfs leaves them in the kernel); `Recording::open` then calls `make_all_durable` before serving.

### Signatures (crate-private, frozen)

```rust
// tree_actor/journal/mod.rs
pub(super) struct Record { pub seq: RecordNo, pub at_ns: u64, pub entry: Entry }  // seq read from/written to the header
impl Journal {
    /// One buffer (header, head, tail), one `write_all_at`, no fsync; then `durability.appended_to(seq)`.
    /// A failed write is rolled back with `set_len` (no fsync); a failed rollback makes the file unknown.
    pub fn append(&mut self, record: &Record, tail: &[u8]) -> Result<()>;
    pub fn durability(&self) -> &Arc<Durability>;
    // `sync()` is deleted: nothing on the call path fsyncs the journal.
}
// tree_actor/journal/durable.rs
impl Durability {
    pub(in crate::tree_actor) fn new(file: Arc<File>, appended: RecordNo, mark: RecordNo) -> Arc<Self>;
    /// Under the actor lock, after the frame's write returned.
    pub(in crate::tree_actor) fn appended_to(&self, record: RecordNo);
    /// Compaction, under the actor lock, after the new file is fsynced and renamed: mark = max(mark, last).
    pub(in crate::tree_actor) fn replace_file(&self, file: Arc<File>, last: RecordNo);
    /// Queues `then` on the waiter thread (started by `new`): one fsync answers every queued wait it covers.
    pub(in crate::tree_actor) fn then(&self, through: RecordNo, then: Then);
    pub(in crate::tree_actor) fn has_failed(&self) -> bool;
}
// leader: lock; while mark < through { if failed: Err; if leading: wait; else break }; leading = true;
// target = appended; unlock; file.sync_data(); lock; leading = false; Ok => mark = max(mark, target),
// Err(e) => failed = errno; notify_all.   (sync_data: fdatasync covers the size a read needs.)

// tree_actor/mod.rs
pub enum Error { /* ... */ #[error(transparent)] Durable(#[from] DurableError) }   // errno: DurableError::errno
impl TreeActor {
    /// The last record a FLUSH of `open` must cover: its open record, its calls, its object's page writes.
    pub fn durable_through(&self, open: u64) -> Result<RecordNo>;
    /// stopped || journal unknown || durability failed.
    pub fn is_stopped(&self) -> bool;
    // `pub fn sync(&self)` is deleted.
}
```

`Recording` (B2a part): `{ actor: Mutex<TreeActor>, durability: Arc<Durability> }`. `durable(open)` locks, reads
`durable_through`, unlocks, then `make_durable`. `durable_then(open, then)` locks, reads it, unlocks, then
`Durability::then`. `create`/`open` wrap `TreeActor::{create, open}` (which stay pub until B2b).

### View (`crates/agentfs-fuse/src/tree_view.rs`)

- `serve`, `serve_with_metrics` take `Arc<Recording>`; `n_threads` stays 1 (in-order answers are close-to-open).
- FLUSH: `Open::Write` -> `recording.durable_then(handle, reply)`; `Open::Read` -> reply at once.
- FSYNC: `Open::Write` -> `durable_then`; `Open::Read` -> `durable_all_then`. New FSYNCDIR -> `durable_all_then`.
- `open`/`create` with `O_SYNC` or `O_DSYNC`: `Open::Write { sync: true, .. }`; each WRITE applies under the lock,
  drops the guard, then `durable_then`.
- Lock order: the actor guard before `handles`, neither held when a `then` runs or is queued.
- Check before relying on it: fuser's `ReplyEmpty`/`ReplyWrite` are `Send` and may be answered from another thread.

### Owned files

`crates/agentfs-core/src/tree_actor/{mod.rs, call_path.rs, stages.rs, journal/{mod.rs, durable.rs, commit_point.rs,
fold.rs}, recording/{mod.rs, pages.rs}}`, `crates/agentfs-core/src/state_area.rs`, `crates/agentfs-core/tests/{tree_actor.rs,
tree_stages.rs, crash_points.rs, support/crash_oracle.rs}`, `crates/agentfs-fuse/src/tree_view.rs`,
`crates/agentfs/src/trees_cmd.rs`, `ci/power-loss-trees.sh`, `ci/power-loss-trees-check.sh`, docs listed below. Build
output: the shared target.

### Acceptance

| Test | Shows |
|---|---|
| `a_call_is_not_fsynced_and_flush_fsyncs_once` (tree_stages) | N writes then `durable`: `journal_fsync.count` rises by 1; close_4k's engine path does 1 journal fsync where it did 4 |
| `concurrent_waits_share_one_fsync` (durable.rs unit) | 8 threads append then `make_durable`: fsyncs < 8; each returns a mark >= its record |
| `the_mark_never_passes_a_record_written_after_the_fsync_began` | failpoint `tree.durable_in_fsync` holds the leader; an append meanwhile; mark ends at the leader's target |
| `a_flush_waits_without_the_actor_lock` | with the leader held at that failpoint, another thread takes `recording.actor()` |
| `a_hole_a_later_mark_covers_is_corruption`, `a_hole_no_later_mark_covers_is_a_torn_tail` | the open rule, on synthetic files |
| `a_kill_after_a_write_returned_keeps_it` (crash_points) | kill after `append` returns, no fsync; reopen in the same kernel: the call saves |
| power-loss-trees (phase-1, CI) | re-pointed to G1: the `XW` mark after `close`, the `RN` mark after `fsync` of the view's directory; everything marked survives |
| regression tier | `cargo test -p agentfs-core -p agentfs-fuse -p agentfs-conformance`, `cargo test -p agentfs-model`, `ci/model-digest.sh check`: digests unchanged (the model is untouched) |

Release figures (image job, advisory). Hypotheses against B1 (12.7/15.1 ms close_4k p50 ext4/XFS): about three fewer
fsyncs per save, so close_4k 8 to 12 ms; creates by 300 s 5,500 to 7,000 (about 4,500 after B1); close_64m no worse.
**Kill condition:** close_4k p50 does not fall by at least 1.5 ms on either filesystem: the per-call fsync was not
where the time went; report the figures before B2b.

### Mutation probes (each fails a named test)

1. An fsync put back in `Journal::append` -> `a_call_is_not_fsynced_and_flush_fsyncs_once`.
2. The leader sets `mark = appended` at the end instead of the target taken before the fsync -> `the_mark_never_passes...`.
3. `durable` fsyncs while holding the actor guard -> `a_flush_waits_without_the_actor_lock`.
4. The open rule ignores `mark` (every hole is torn) -> `a_hole_a_later_mark_covers_is_corruption`.
5. FSYNCDIR answered without `durable_all` -> power-loss `RN` replay (CI).

### Docs (same commit)

OWNER-GUIDANCE.md: the four 2026-11-13 answers in his words (speed before S3; match the recording engine; the night
direction quoted in speed-brief.md; Q1 "Hashed sub-folders"). AGENTS.md principle 1, `docs/guarantees.md`,
`docs/recording.md`, `docs/trees.md`: tree views are power-cut safe at close, fsync or FSYNCDIR (G1), recorded in the
journal before the call returns. git-model §3 table (write and namespace rows), §12 journal row; the ledger row on call
durability, if any, cites G1. `docs/runbooks/recording-failures.md`: the corruption rule with the mark.
`docs/architecture.md` rows. CHANGELOG Unreleased.

## 4. B2b (tracer): the step is queued; the builder makes history behind it

**Observable behaviour.** The call path runs no git child and at most one fsync (a FLUSH's); a save is visible at
RELEASE; history (refs, checkouts, commits) follows behind, built by `git fast-import` from durable steps; takes decide
off the actor on built snapshots; per-tree lag gates new work; a clean shutdown builds everything.

### Records (journal head JSON, frozen)

```rust
pub(super) enum Entry {
    Checkpoint { next: RecordNo, built: RecordNo,        // built: the redo start, <= the built log's point
                 pointers: BTreeMap<String, String>,     // every value an oid, as of `built`
                 upstreams: BTreeMap<String, String>, opens: Vec<OpenRecord>,
                 page_saved: Vec<(String, u64, u64)> },
    Open { tree: String, path: String, by: WriterAt, object: u64 },
    AnonymousOpen { tree: String, by: WriterAt },
    Call { handle: u64, call: CallHead },
    PageWrite { tree: String, object: u64, runs: Vec<RunHead> },
    Step { by: WriterAt, change: Change, pointers: Vec<PointerChange> },
}
pub(super) struct PointerChange { pub name: String, pub new: Option<Target>, pub old: Option<Target> }
#[serde(rename_all = "lowercase")]
pub(super) enum Target { Oid(String), Step(RecordNo) }  // Step(n): the value step n's build gives this ref
pub(super) enum Change {
    /// A create; opens handle = this record. Object = this record's number.
    Create { tree: String, path: String },
    Link { tree: String, object: u64, path: String },
    Save { tree: String, handle: u64, pages: Vec<PagePiece>, outcome: SaveOutcome },
    Rename { tree: String, from: String, to: String },
    Unlink { tree: String, path: String },
    Fork { upstream: String },                            // fork id = this record's number; head = Step(upstream's head step)
    Take { tree: String, take: TakeRecord },
}
/// A page write's run applied by a close, by reference (the open's own calls are implied: all its Call records).
pub(super) struct PagePiece { pub record: RecordNo, pub offset: u64, pub len: u64 }
pub(super) enum SaveOutcome { Saved { len: u64, crc32: u32 }, Unchanged, OrphanDropped, AnonymousDropped }
pub(super) struct TakeRecord {
    pub entry: String, pub whole: bool, pub decided_at: RecordNo,   // decided_at: audit
    pub head: TakeHead,
    pub files: Vec<(MergeFileIdentity, Option<referee::File>)>,     // the decision's changed files: object and line map (bytes inside)
    pub names: Vec<(MergeFileIdentity, NameHistory)>,
    pub collisions: Vec<referee::Collision>,                        // derive Serialize, Deserialize
}
pub(super) enum TakeHead { FastForward { to: String }, Landing { incoming: String }, Sync { target: String }, Stay }
```

Built log `built.log` (state area; appended and fsynced only by the builder; frames as the journal's, magic `AFB1`):

```rust
pub(super) struct BuiltEntry { pub through: RecordNo, pub values: Vec<BuiltValue> }
pub(super) struct BuiltValue { pub step: RecordNo, pub name: String, pub oid: String }  // every Target::Step(step) of `name`, resolved
```

Rewritten (temp, fsync, rename, dir fsync, by the builder) to drop entries `<= checkpoint.built` once a checkpoint is
durable. The built point is the last durable entry's `through`.

### One apply function per concern (finding 6)

```rust
// tree_actor/recording/pages.rs
/// Which page-write pieces a close applies (today's `calls_for_close`, returning references).
pub(in crate::tree_actor) fn close_pieces(saved_len: u64, own: &[(RecordNo, Call)], writes: &[(RecordNo, Run)],
                                         saved_through: RecordNo) -> Vec<PagePiece>;
/// The calls a close applies, in record order: the open's own calls merged with the resolved pieces.
pub(in crate::tree_actor) fn close_calls(own: &[(RecordNo, Call)], pieces: &[(PagePiece, &[u8])]) -> Vec<Call>;
// then `Call::apply` (exists). The actor decides the pieces and the outcome; the builder resolves the pieces from
// frames, applies the same calls to the parent's built bytes and refuses `Disagrees(step)` when len or crc32 differ.
```

```rust
// tree_actor/tree.rs: the names-level part both projections run
#[derive(Clone, Default)]
pub(super) struct Shape { pub names: Names, pub anonymous: BTreeSet<u64>,
                          pub identities: BTreeMap<u64, MergeFileIdentity>, pub fresh_names: BTreeSet<u64> }
impl Shape {
    /// The one transition of names, objects and identities for a step; the keys it touched (for `land`).
    pub(super) fn apply(&mut self, change: &Change, step: EventNumber) -> Result<BTreeSet<queue::Key>>;
}
/// The actor's saved state: Shape and bytes (B3 replaces bytes with file maps). No line map, no name history.
pub(super) struct Saved { pub shape: Shape, pub bytes: BTreeMap<u64, Arc<FileBytes>> }
/// The builder's state: today's `Tree` (lines, namespace) with its `Shape` inside, so the transition is shared.
```

### Builder (`tree_actor/history/{builder.rs, feed.rs, built_log.rs, lag.rs, format.rs}`)

```rust
pub(in crate::tree_actor) struct JournalReader { /* own File, cursor: RecordNo */ }
impl JournalReader {
    /// Frames after the cursor through `durability.mark()`; after a compaction reopens and skips to the cursor.
    pub fn read(&mut self, through: RecordNo) -> Result<Vec<Frame>>;
}
pub(in crate::tree_actor) struct Builder { /* trees: BTreeMap<TreeId, Tree>, pointers, upstreams, owners, opens */ }
impl Builder {
    /// Its own thread. Waits for a step-appended event; through = durability.appended(); make_durable(through);
    /// reads to the mark; builds the batch on copies; installs; emits built or failed. Retries on the next event.
    pub fn run(self, shared: Arc<HistoryShared>) -> Result<(), BuildError>;
}
pub(in crate::tree_actor) struct Feed { /* child, stdin, next mark */ }
pub(in crate::tree_actor) enum Parent { Built(Oid), Mark(u64) }
pub(in crate::tree_actor) enum Edit<'a> { Blob { path: &'a str, mark: u64 }, Inline { path: &'a str, bytes: &'a [u8] },
                                          Delete { path: &'a str } }
pub(in crate::tree_actor) struct CommitSpec<'a> { pub tree: TreeId, pub parents: &'a [Parent], pub at_ns: u64,
                                                  pub message: &'a str, pub edits: &'a [Edit<'a>] }
impl Feed {
    /// `git fast-import --quiet --done --export-marks=<state>/build.marks` through `git_cli::command`, in history.git.
    pub fn start(git_dir: &Path, marks: &Path) -> Result<Self, BuildError>;
    pub fn blob(&mut self, bytes: &[u8]) -> Result<u64, BuildError>;
    pub fn blob_from(&mut self, len: u64, reader: &mut dyn Read) -> Result<u64, BuildError>;  // B3 streams
    /// `commit refs/build/<tree>`, committer `agentfs <agentfs@localhost> <at_ns/1e9> +0000` (author the same),
    /// `from`/`merge` from `parents`, the edits.
    pub fn commit(&mut self, spec: &CommitSpec<'_>) -> Result<u64, BuildError>;
    /// `done`, wait, read marks; fsync every new file in objects/pack (pack, rev, idx), then objects/pack.
    pub fn finish(self) -> Result<BTreeMap<u64, Oid>, BuildError>;
}
// format.rs: the commit layout's one home (B4 replaces the bodies)
pub(in crate::tree_actor) fn file_path(name: &str) -> String;                    // "files/<name>"
pub(in crate::tree_actor) fn checkout_path(dir: &Path, name: &str) -> PathBuf;   // dir/<name>
pub(in crate::tree_actor) fn edits<'a>(before: &'a Tree, after: &'a Tree, changed: &'a BTreeSet<u64>,
                                       blobs: &'a BTreeMap<u64, u64>) -> Vec<Edit<'a>>;  // B2b: changed files + whole metadata.json, namespace.json
pub(in crate::tree_actor) fn read_snapshot(repo: &TreeGit, commit: Oid) -> Result<referee::Snapshot>;
```

Install order per batch (rule 3): fast-import exits (pack renamed before idx by git) -> the builder fsyncs the new
pack files and `objects/pack` -> appends and fsyncs the built-log entry -> refreshes the odb -> copies (refs by one
ref transaction from the built pointers, `refs/build/*` deleted, `config` for forks, incremental checkouts) -> built
event (waiters, gate, take worker, actor's `touched` pruning). A copy failure is logged and redone at the next batch.
A failure before the built-log fsync: `Failed(e)`, the batch dropped, nothing reachable changed; the pack left behind
is unreferenced. Recovery deletes `objects/pack/tmp_*`, stale `.keep`, any `.idx` without its `.pack`, `refs/build/*`.

Commit identity: dates come from the record's `at_ns`, zone `+0000` (B1's local zone goes); a commit is the bytes
`git commit-tree` would write for them. Alignment is the builder's (`agentfs-align`, a `git diff` child), once per
step, before its built-log entry. No repack runs on a tree volume's repository; one added later must hold the builder.

### History, gate, takes

```rust
pub struct History { /* Mutex<TreeGit> for reads (refreshed at each built event), built point, built pointers,
                        Condvar, LagGate, health */ }
impl History {
    pub(in crate::tree_actor) fn open(git_dir: &Path, lease: Arc<FileLock>, built_log: &Path) -> Result<Arc<Self>>;
}
impl BuiltSnapshot {   // crate-private readers; constructor only in history::built
    pub(in crate::tree_actor) fn tree(&self, tree: TreeId) -> Result<referee::Snapshot>;
    pub(in crate::tree_actor) fn snapshot_at(&self, commit: Oid) -> Result<referee::Snapshot>;
    pub(in crate::tree_actor) fn read_commit(&self, commit: Oid) -> Result<Commit>;
    pub(in crate::tree_actor) fn is_ancestor(&self, a: Oid, b: Oid) -> Result<bool>;
    pub(in crate::tree_actor) fn queue(&self, tree: TreeId) -> Vec<(String, Oid)>;
}
// lag.rs
pub(in crate::tree_actor) const LAG_ALARM: Duration = Duration::from_secs(30);   // placeholders, one home
pub(in crate::tree_actor) const LAG_LIMIT: Duration = Duration::from_secs(120);
pub(in crate::tree_actor) struct LagGate { /* per tree: unbuilt (step, bytes); rates; health; Condvar; gate queue */ }
impl LagGate {
    pub fn appended(&self, tree: TreeId, step: RecordNo, bytes: u64);   // actor, under its lock: bytes the step adds
    pub fn built(&self, through: RecordNo, ns_per_step: u64, ns_per_byte: u64);  // measured on the batch
    pub fn failed(&self, error: BuildError);
    pub fn admit(&self, tree: TreeId) -> Result<(), WaitError>;
    pub fn admit_then(&self, tree: TreeId, then: Then);
}
// takes.rs
pub(in crate::tree_actor) struct TakeInputs { pub tree: TreeId, pub held: BTreeSet<queue::Key>, pub head_step: RecordNo }
pub(in crate::tree_actor) struct Decision {
    pub tree: TreeId, pub entry: String, pub decided_at: RecordNo, pub head_at: RecordNo,
    pub reads: BTreeSet<queue::Key>,    // touched keys, plus held keys that cut its sync or blocked an earlier entry
    pub writes: BTreeSet<queue::Key>,   // its plan's keys (a losing change's files included, R9.1)
    pub take: TakeRecord,
}
pub(in crate::tree_actor) enum Refused { FilesChanged, Held, HeadMoved, EntryGone }
/// Pure over the snapshot (its Git reads) and the inputs; never on the actor.
pub(in crate::tree_actor) fn decide(snapshot: &BuiltSnapshot, inputs: &TakeInputs) -> Result<Option<Decision>>;
impl TreeActor {
    pub(in crate::tree_actor) fn take_inputs(&self, tree: TreeId) -> Result<TakeInputs>;
    /// Appends the take if no step after `decided_at` touched a key of `reads`, no write-open holds a key of
    /// `writes`, and (FastForward) the head is still `head_at`; enqueues its moves under this record's number.
    pub(in crate::tree_actor) fn land(&mut self, decision: Decision) -> Result<std::result::Result<EventNumber, Refused>>;
}
```

The actor keeps `touched: BTreeMap<TreeId, BTreeMap<Key, RecordNo>>` (each key's last touching step, from
`Shape::apply`), pruned at built events of entries `<= built`. Takes are one at a time (the view's worker, or
`take_next`). Delayed-decision equality: a landed decision read nothing that changed and writes nothing held, so it
decides what an atomic take at the land would (P1's own rule covers an earlier entry it passed: they share no key or
source).

Gate: `appended` adds the step's bytes (files it changes, plus format's listings on its path); lag(tree) =
steps x ns_per_step + bytes x ns_per_byte, both measured on the last batch (initial 5 ms, 10 ns). `admit` passes while
lag(tree) <= LAG_LIMIT or health is `Failed`; else waits for a built or failed event. Health per the v4 state machine;
logs `tree HistoryLagging`, `tree HistoryStalled`, `tree HistoryBuildFailed: <e>` once per entry; Metrics gains
`history { built_through, appended, lag_seconds_max, oldest_unbuilt_ns, health }`. OpenTelemetry export of tree views
is not built (existing gap, not this slice).

View: `admit_then` wraps create, write-open, `O_TMPFILE`, link, rename, unlink, path truncate. The worker loop is
`epoch = rec.wait_for_work(epoch); while rec.progress()? == Landed { invalidate(drain_changed) }`.
Failpoint gate (conformance): a line `settle` runs `rec.settle()` and answers `settled`.

`TreeActor::{create, open}` become crate-private; tests and conformance use `Recording`. `TreeActor` holds no
`TreeGit` (only History does); clippy.toml disallows `agentfs_core::tree_git::TreeGit::{open, open_with_lease, init,
init_with_lease}` outside `tree_actor::history` (one `#[expect]` there; integration tests allow it).

### Owned files

New: `tree_actor/{takes.rs, history/{builder.rs, feed.rs, built_log.rs, lag.rs, format.rs}}`. Changed:
`tree_actor/{mod.rs, call_path.rs, tree.rs, pointers.rs, queue.rs, stages.rs, history/{mod.rs, built.rs, take.rs,
refs.rs, message.rs}, journal/{mod.rs, fold.rs, commit_point.rs}, recording/{mod.rs, pages.rs}}`, `tree_git.rs` (odb
refresh), `state_area.rs`, `clippy.toml`, `crates/agentfs-core/tests/tree_*.rs`, `tests/crash_points.rs`,
`tests/support/*`, `crates/agentfs-core/src/failpoints.rs` (settle line), `crates/agentfs-fuse/src/tree_view.rs`,
`crates/agentfs/src/trees_cmd.rs`, `crates/agentfs-conformance/src/{library.rs, mounted.rs}`, `ci/trees-figures.py`,
`ci/trees-budget.json`, docs. Build output: the shared target.

### Acceptance

| Test | Shows |
|---|---|
| tree_stages `the_call_path_runs_no_git` | create, write, close, rename, unlink: `git_children` 0, `object_writes` 0, `journal_fsync` 1 per FLUSH |
| `a_save_is_visible_before_it_is_built` | with the builder held (failpoint `tree.build_before_batch`), a read after close sees the bytes; `history().snapshot()` does not |
| crash: failpoints `tree.build_after_pack`, `tree.build_after_built_log`, `tree.build_after_ref_copy` | kill at each; reopen; refs name only built-log commits; settle; trees and messages equal an uninterrupted run's (same image) |
| `a_take_decided_before_disjoint_saves_lands_as_one_decided_after` (unit) | two volumes, same calls; decide, save another file, land vs save, then atomic take: equal trees and collisions read from git |
| `a_take_whose_read_key_changed_is_refused_then_redecided`, `..._held_is_refused`, `a_fast_forward_after_a_head_move_is_refused` | each refusal, then landing at the next event |
| `the_gate_holds_only_its_tree` | tree A over the limit (failpoint holding the builder, a test lag limit): A's create waits; B's create, A's write and A's RELEASE do not |
| `a_failed_builder_releases_the_gate_and_recording_goes_on` | failpoint fails the batch: waiters get `Failed`, writes and saves continue, a retry builds |
| `drop_builds_everything` | drop the last `Recording`; reopen; built point = last record |
| `restart_replays_from_the_checkpoint` | 10,000 files, compaction, kill; reopen time reported (hypothesis under 5 s debug) |
| conformance library replay (regression tier), mounted replay (phase-1 on the PR) | 0 mismatches; model digests unchanged |
| power-loss-trees (phase-1) | unchanged from B2a, now with the builder running |

Release figures (image job, builder running; the figures' guarantee becomes "saved before the view answers any
later request; history built later", and each workload reports `drain_s`, the time from its last save to the built
point reaching it, read from the metrics file).

| Workload | Hypothesis | Kill condition |
|---|---|---|
| close_4k p50 | 2 to 4.5 ms: one FLUSH fsync (1.3 to 2.3 ms in the engine) plus about five FUSE round trips; likely over 3.5 ms on the Intel ext4 runner, where the engine is too | p50 over 6 ms (the engine's through-FUSE budget) on either filesystem: report to the owner with figures, hold B4 |
| create by 300 s | 10,000 to 25,000: the call path is O(1), but the builder still writes whole-tree blobs (B4), so the gate holds creates to its pace | none (B4's bar); the call-path counters above are the gate |
| close_64m | no worse than B2a | over B2a's figure by 20% |
| close_4k drain_s | under 2 s (200 saves x 3 to 6 ms of builder) | over 10 s |

### Mutation probes

1. Built event before the built-log fsync -> crash test at `tree.build_after_pack` (refs name an unbuilt commit).
2. No fsync of fast-import's pack files -> `feed_fsyncs_what_it_installed` (unit: counts fsyncs of new pack files) and power-loss (CI).
3. A `BuiltSnapshot` built outside History -> does not compile (private field).
4. `land` skips the `reads` check -> `a_take_whose_read_key_changed...`; skips `held` -> `..._held_is_refused`.
5. The gate keyed globally -> `the_gate_holds_only_its_tree`.
6. The builder reads past the mark -> `the_builder_never_reads_past_the_mark` (unit: reader with an unsynced appended frame).
7. `close_calls` reimplemented in the builder without the pieces -> `a_mapped_write_after_a_close_survives_the_build` (tree_pages, via settle) and `Disagrees`.

### Docs

git-model §6 (takes decided on built snapshots, landed if their inputs are unchanged), §12 (the step's record is the
commit point of the change; the builder, built log, built point, install order; refs and checkouts copies as of the
built point; the object table row), §3 compaction behind the built point; `docs/trees.md` (history trails the files;
lag, gate, health, metrics; clean shutdown builds), `docs/guarantees.md`, `docs/architecture.md` (rows, a Technology
choices row for `git fast-import`), runbook `docs/runbooks/recording-failures.md` (HistoryBuildFailed, a wedged
builder: kill the `git fast-import` child), `docs/performance.md` figures, CHANGELOG. `speed-design.md`'s B2 part is
not in the repo (scratchpad), so nothing to delete; the M1 S3 page is re-cut after this slice.

## 5. B4: the builder costs its change (Q1-A)

**Observable behaviour.** A 1-file save stores the same objects at any tree size; 100,000 files in one directory keep
history within its bar.

### Commit layout (format.rs bodies replaced; frozen)

```text
<commit tree>
  files/<aa>/<bb>/<name>    the file's bytes
  lines/<aa>/<bb>/<name>    JSON [identity, lines]: the file's line map (metadata.json's entry)
  names/<cc>/<dd>/<id>      JSON NameHistory, one per file identity, ended ones included (namespace.json's entry)
aa bb  = hex digits 1-2 and 3-4 of `git hash-object --stdin` of <name>'s UTF-8 bytes (fixed two levels of 256: a
         file's path never changes when siblings come or go)
cc dd  = the same, of <id> in decimal
```

`metadata.json`, `namespace.json` and the object table refs (`refs/agentfs/objects/<tree>`) go. Object numbers live
in the journal: the checkpoint's new `objects: BTreeMap<String, BTreeMap<u64, u64>>` (tree, identity, object) as of
`built`, and steps after it. Names are flat (`name()` refuses `/`), so no directory rename rehashes many files; a
future directory slice decides its own hashing. Checkouts mirror `files/` (`checkout_path` = `dir/<aa>/<bb>/<name>`).

```rust
pub(in crate::tree_actor) fn fanout(name: &str) -> (String, String);   // the one hash; conformance re-implements it from this page
pub(in crate::tree_actor) fn file_path(name: &str) -> String;
pub(in crate::tree_actor) fn lines_path(name: &str) -> String;
pub(in crate::tree_actor) fn names_path(identity: MergeFileIdentity) -> String;
pub(in crate::tree_actor) fn edits<'a>(before: &'a Tree, after: &'a Tree, changed: &'a BTreeSet<u64>,
                                       blobs: &'a BTreeMap<u64, u64>) -> Vec<Edit<'a>>;  // only changed files' three paths
pub(in crate::tree_actor) fn read_snapshot(repo: &TreeGit, commit: Oid, objects: &BTreeMap<u64, u64>) -> Result<referee::Snapshot>;
// tree_reconcile.rs: LineMap::align with an empty old image makes every line new without a child (same hunks as git's)
```

### Owned files

`tree_actor/history/{format.rs, mod.rs, refs.rs}`, `tree_actor/journal/fold.rs` (checkpoint objects),
`tree_actor/history/builder.rs` (calls format only), `tree_reconcile.rs` (empty-old alignment),
`crates/agentfs-conformance/src/git_history.rs` (its own reader of this layout, sharing no code with format.rs),
`crates/agentfs-core/tests/{tree_stages.rs, tree_reconcile.rs}`, `ci/trees-budget.json`, docs (`docs/trees.md` layout,
git-model §4 and §12 object table row, performance.md). Not owned: journal/mod.rs, recording/*, tree_view.rs (B3's).

### Acceptance

| Test | Shows |
|---|---|
| tree_stages `a_save_stores_the_same_objects_at_100_and_10000_files` | builder objects per 1-file save equal (hypothesis 6: blob, line sidecar, three listings, commit) |
| `an_empty_files_first_alignment_runs_no_child` | `git_children` 0 for a first write; hunks equal git's on a table of inputs |
| `fanout_matches_git_hash_object` | against `git hash-object --stdin` |
| conformance library replay, mounted (phase-1) | 0 mismatches with the new reader |
| model regression tier | digests unchanged (the model's layout is its own) |

Release figures: create by 300 s at least 50,000 (the scaled bar), `drain_s` after it at most 360 s (12 min scaled);
close_4k still within B2b's. Hypothesis: builder 1.5 to 4.4 ms a create (about 20 KB of listings: root 256 entries,
level two up to 256, leaf a few, times files and lines), no align child, so creates run at the call path's 2.3 to 3.8
ms (200 s to 380 s for 100,000). **Kill condition:** fewer than 50,000 by 300 s, or drain over 360 s: report to the
owner with the per-create builder time before M1 S3. The unscaled 100,000 / 600 s / 12 min run is NOT RUN in CI
(the image job's 20-minute limit); a dispatch input can run it if the owner wants it.

### Mutation probes

1. A whole-tree blob per step put back (`metadata.json`) -> the objects-per-save test.
2. Fanout that grows with the tree -> `a_files_path_never_changes_when_siblings_are_added` (path of one file at 10 and 10,000 files).
3. Conformance importing `format.rs` -> does not compile (conformance does not depend on private items; reviewer checks the reader is separate).

## 6. B3: file maps and segments (the 1 GiB bar)

**Observable behaviour.** A write costs its own bytes at any file size; no file's bytes are held in the actor's
memory; a dirty 1 GiB close meets the engine's bar; a long-held open does not pin other files' saves.

### Journal segments

`journal/<first record:020>.seg`, 64 MiB each; a frame never spans two; a new segment's directory entry is fsynced by
the leader before the mark covers any record in it. `Durability` fsyncs every segment holding records past the mark,
in order. A segment is dropped (unlinked) once all its records are `<= built` and it holds no live open's call or
held page write; a live open's frames are copied forward (header flag bit 0 `carried`, original record number kept)
only when the dead bytes they pin exceed their own. Readers holding an `Arc<Segment>` keep reading an unlinked one;
its space returns when the last goes. Compaction becomes: write a checkpoint frame (as of `built`), drop segments.

```rust
pub(in crate::tree_actor) struct Segment { /* first: RecordNo, path, file: File */ }
pub(in crate::tree_actor) struct FramePos { pub segment: Arc<Segment>, pub tail_at: u64 }
impl Journal {
    pub fn append(&mut self, record: &Record, tail: &[u8]) -> Result<FramePos>;
    pub fn drop_behind(&mut self, built: RecordNo, live: &dyn Fn(&Record) -> bool) -> Result<DropStats>;
}
pub(in crate::tree_actor) struct DropStats { pub segments: u64, pub carried_bytes: u64 }
```

### File maps (`tree_actor/recording/file_map.rs`)

```rust
pub(in crate::tree_actor) struct BaseFile { /* file: File (the checkout's file as built), step: RecordNo, blob: Oid */ }
pub(in crate::tree_actor) enum Source { Zero, Base { file: Arc<BaseFile>, at: u64 }, Frame { pos: FramePos, at: u64 } }
pub(in crate::tree_actor) struct Piece { pub len: u64, pub source: Source }
#[derive(Clone)]
pub(in crate::tree_actor) struct FileMap { /* len: u64, pieces: Arc<BTreeMap<u64, Piece>> (copy on write) */ }
impl FileMap {
    pub fn empty() -> Self;
    pub fn of_base(base: Arc<BaseFile>, len: u64) -> Self;
    pub fn len(&self) -> u64;
    /// The one apply: the actor's saved state, an open's view, the builder's replay. Offsets never shift
    /// (POSIX has no insert), so a piece keeps its file offset for life.
    pub fn apply(&mut self, call: &Call, at: &FramePos) -> Result<()>;
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;   // pread per piece
    pub fn reader(&self) -> FileMapReader<'_>;                             // `impl Read`, for `Feed::blob_from`
    /// "unchanged": compares only `ranges` (the open's own runs), never the whole file.
    pub fn same_as(&self, other: &FileMap, ranges: &[Range<u64>]) -> io::Result<bool>;
    /// At a built event: pieces from steps `<= base.step` read from `base` at the same offset.
    pub fn rebase(&mut self, base: Arc<BaseFile>);
}
```

`Saved.bytes` becomes `BTreeMap<u64, FileMap>`; `Handle.bytes` becomes the open's `FileMap` (a clone of the saved one
at open, O(pieces), never a copy of bytes); `changed_bytes` becomes `same_as` over the open's runs; the save record's
`Saved { len, crc32 }` becomes `Saved { len, pieces: u64 }` (a hash of the piece list: the builder's `apply` must
reach the same pieces). Checkouts write each file to a temporary name and rename it, so a `BaseFile` keeps its inode.
The view reads through `FileMap::read` with the actor locked only to clone the map.

### Owned files

`tree_actor/journal/{mod.rs, durable.rs, commit_point.rs, segment.rs}`, `tree_actor/recording/{mod.rs, pages.rs,
file_map.rs}`, `tree_actor/tree.rs` (Saved), `tree_git.rs` (checkout by rename), `tree_actor/history/builder.rs`
(streams `FileMap::reader` into `Feed::blob_from`; its only change), `crates/agentfs-fuse/src/tree_view.rs` (reads),
`crates/agentfs-core/tests/{tree_actor.rs, tree_pages.rs, tree_stages.rs, crash_points.rs}`, `ci/trees-figures.py`
(`write_1g`, `close_1g`), `ci/trees-budget.json`, `ci/e2e-trees-tracer.sh` (loop filesystem 6 GiB), docs.

### Acceptance

| Test | Shows |
|---|---|
| tree_stages `bytes_copied_per_write_is_its_own` | a counter of bytes copied: a 4 KiB write into a 1 GiB file copies 4 KiB (in the engine, a sparse 1 GiB file) |
| `unchanged_compares_only_the_opens_runs` | a bytes-compared counter equals the open's run bytes |
| `a_held_open_pins_only_its_own_bytes` | after 200 MiB of other saves and a build, segments on disk under 2 x (live open bytes + 64 MiB) |
| `a_reader_of_a_rebased_file_reads_the_old_version` | a reader holding a map across a build reads the bytes it opened |
| crash_points at segment rollover (`tree.segment_after_create`) | reopen: records in a new segment whose directory entry was not fsynced are never marked durable |
| regression tier and phase-1 | 0 mismatches; digests unchanged |

Release figures: new `write_1g` (1 GiB in 1 MiB writes): p50 of the last 64 writes within 1.5 x the p50 of a 1 MiB
file's writes; `close_1g` at most 8 s on XFS and 12 s on ext4. Hypothesis: XFS 2.5 to 5.4 s, ext4 4.9 to 8.1 s (one
fsync of 1 GiB of journal, no copy). **Kill condition:** either bar missed: report figures to the owner. Space: the
state area needs about 3 GiB for a 1 GiB close while history builds (journal, pack, checkout).

### Mutation probes

1. A whole-file copy per write -> `bytes_copied_per_write_is_its_own`.
2. A full compare for "unchanged" -> `unchanged_compares_only_the_opens_runs`.
3. A live open's frames copied forward on the 1 GiB bar (threshold removed) -> `close_1g` bytes-written counter in the figures report, and `a_held_open_pins_only_its_own_bytes` inverted case.
4. Checkout writing in place -> `a_reader_of_a_rebased_file_reads_the_old_version`.

## 7. Test architecture

| Module | Isolation | Conformance boundary |
|---|---|---|
| `journal` + `Durability` | unit tests on temp files, failpoints for the leader's window; synthetic frames for the open rule | power-loss-trees (dm-log-writes, CI) |
| `call_path::Recording` | integration tests with real volumes; the lock-discipline test takes the actor while a leader is held | mounted replay |
| builder, feed, built log | unit: feed against a temp bare repo (fsck --strict); crash points at each install step | refs name only built-log commits |
| `takes` (`decide`, `land`) | unit with two volumes from one script (decide-early vs decide-late) | library replay against the model (digests, 0 mismatches) |
| `LagGate` | unit with injected rates and a test limit | none |
| `format` | unit; conformance's independent reader | conformance reads layout only from B4's page |
| `FileMap` | unit property-style: random calls applied to a map and to a `Vec<u8>` oracle agree | the model's byte oracle via replay |

## 8. Maintenance risks

- **B2b is large.** Three packages and most of `tree_actor`. If it overruns the per-bead ceiling, split on the compile
  seam "builder and History with takes still on the actor reading built snapshots after `settle`" first, then "takes off
  the actor and the gate". Each is green alone.
- **Two projections of one journal.** `Shape::apply`, `close_calls`, `Call::apply`/`FileMap::apply` are the only
  shared transitions; anything else the actor and builder both compute is a bug. The `Disagrees` check makes drift loud.
- **Test churn.** About 77 integration tests move from `&mut TreeActor` to `Recording` plus `settle`; mechanical.
- **fast-import as a dependency.** Its protocol is stable, but its durability is ours (we fsync) and its refs are
  scratch. A future repack must coordinate with the builder.
- **Lag constants** live in one place; making them git-config keys is deferred until real use asks.

## 9. Departures from the existing code, and their cost

| Code | Departure | Cost |
|---|---|---|
| journal fsync per call (`Journal::append`) | none; FLUSH or fsync makes durable (G1) | power cut loses unsynced calls, renames and unlinks included |
| `View` single-threaded, blocking fsync in FLUSH | replies deferred to the journal's waiter thread | a callback per FLUSH; ordering kept |
| `run_step` builds the commit (B1's in-process writer) | the builder builds it with fast-import; B1's writer is deleted | history trails; new failure mode `HistoryBuildFailed` |
| `TreeActor.repo`, `head()` | only History opens the store; heads only as built | tests settle before reading git |
| takes on the actor (`take_next`) | decided off the actor, landed by `land` | a take's latency includes the lag; refusals re-decide |
| metadata.json, namespace.json, object table refs | per-file sidecars, hashed folders, objects in the checkpoint (B4) | `git log -- files/<name>` needs the hashed path |
| `Handle.bytes`, `Tree.bytes` in memory | file maps over segments and checkout files (B3) | reads pread from two sources; checkouts write by rename |

## 10. Owner questions

None. Decided here (general guidance territory, by the principles): takes may starve while their files change faster
than the lag, under G7, with a reservation fallback named and not built; lag limits stay constants; the view stays
single-threaded with deferred replies; B2 is cut into B2a and B2b.
