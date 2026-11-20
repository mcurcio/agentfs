# Memory per file and stalls from a slow close

**Proposed, not built.** This page proposes bringing a tree view's memory per file to the 2 KiB bar and ending the stalls a slow close causes. The commit that lands the last of this slice deletes this page and moves the behaviour into the library and operator docs.

**Layer:** implementation, inside the existing tree-engine and CSI modules. Two parts reach higher layers and are marked as such. The overlay (owner decision below) changes the domain model: one shape plus the lag instead of two copies. **§4.6** changes the host protocol, which is a published integration point (docs/library/filesystem.md promises "one reply per request, in order"); its signatures go to the interfaces gate. The gains are arithmetic from the findings' massif snapshots and the reviewers' probes. Measurements: HEAD 9045165, local ext4, 4 CPUs.


## 1. The problem, measured

| Goal (user's note) | Measured | Root cause |
|---|---|---|
| A 100k-file tree fits under 512 MiB | Steady state: **4.72 KiB/file**. Restart: **VmHWM 517 MiB at 56k files** (one run). malloc_trim then released 165 MB | State is handed over by copying it. The builder clones `BuildState` every batch (builder.rs:1223). `built_owners` copies all owners every batch. The actor keeps a second shape. Paths are stored 5 times. Restart does a full copy and read before the mount |
| A slow close must not stall other requests | A held fd or cwd stalls every request of the volume for as long as it is held (**4.7 s, 29.7 s**) | Three layers each allow only one request in flight (host.rs:238; host_client.rs:103; supervisor.rs:716-723) |
| (same) | **99 s** after 200k random writes. **345–409 ms** and a **659 MB peak** after 1M sequential writes | Work under the volume's one actor lock has no bound. `FileMap::put` splices a Vec. `changed()` clones the map and replays every call of the open (recording/mod.rs:1161). `same_as` reads every touched byte (143 ms for 512 MiB) |
| Builder keeps up | Batch cost grows with the pack count (345 → 797 ms). Mount takes 31 s with 403 packs, 14 s with 1 pack | The tree engine never repacks |

## 2. Requirements

- **R1. Recording before return** (AGENTS.md principle 1).
  - How it holds: `journal_record` in `call` and `record_step` in a close keep their place.
  - A close that does its work off the lock still records its step before RELEASE is answered.
  - Proof: a property test against today's `changed()`, kept only as the oracle, gives the same `SaveOutcome` and digest. The conformance replay at the regression seeds checks it too.
  - The changes to equality and replay touch save semantics, so the model-verification skill applies.
- **R2. History invisible and untouchable** (principle 2). Repack runs in the state area's git directory. The protocol runs on the control channel. Neither is under the mount.
- **R3. History never rewritten** (principle 7). `repack --geometric` packs whole packs through `--stdin-packs`, keeping every object, reachable or not. Nothing prunes, and no ref is touched.
- **Background, not a requirement:** the 2 KiB bar and its "512 Mi" text (ci/trees-budget.json:42-46) have no owner quote.

## 3. Design rules (design choices)

1. **Bounded work under the actor lock.** A call costs O(log pieces). A close or a built event costs O(1) plus its batch. Work proportional to a file's bytes, pieces or calls runs off the lock, on shared snapshots, and lands only if the saved step it read is still current.
2. **State crosses as a shared snapshot, never a copy, and each piece has one home.** An open's calls live in the journal. built.log lives on disk.
3. **A request waits only for a pending request that holds a resource it names.**

## 4. The fixes

### 4.1 Close: O(1) when nothing moved, off the lock when something did

1. Under the actor lock: when the base step is unchanged and only the length changed, take `h.bytes` (O(1)) and `record_step`.
2. Otherwise, under the lock, snapshot the `Arc`s (saved map, `h.bytes`, first seq).
3. Off the lock: replay the calls read back from the journal, `same_as`, digest.
4. Under the lock: if the saved step is still the one read, `record_step`; else redo once under the lock.

- The handle keeps no call list. Its length rule is O(1), and its calls are read back from the journal when needed.
- `Pieces` is an `Arc<[piece]>` of exact size while the map is small. Above a threshold it becomes a persistent ordered map keyed by absolute offset. An overwrite never shifts an offset, so a write is O(log n + pieces covered).

### 4.2 Builder

- `BuildState` maps become persistent (`imbl::OrdMap`). `next = state.clone()` stays and is O(1), so a failed batch is still dropped whole.
- History gets owners as a snapshot.
- `rebase` visits only the files of the batch.

### 4.3 One copy of each fact

- `ObjectState` merges the four per-object B-trees. Its `identity` is an explicit `None | Own | Merged(id)`, never "absent means the object".
- One `PathName` is shared by every index.
- built.log stays on disk only. `BuiltValue.oid` becomes an `Oid`.
- `BaseFile.link` is derived from the base's id.
- Line maps get exact capacity.

### 4.4 Built-event wake

The builder's built event triggers `rebase_built` as soon as a batch is built, not on the actor's next step. The lock order is actor → history, never the reverse.

### 4.5 Repack

- Each built batch emits an event. A repack task of its own runs `git repack --geometric=2 -d`, one at a time.
- It runs alongside fast-import, as git servers run it alongside receive-pack, and is **not** under the builder's lease.
- No `--write-midx`: it maps an index as large as the ones it replaces.

### 4.6 Host and plugin (protocol change; interfaces gate)

```ts
type Envelope<T> = {
  id: number;      // the reply echoes it; replies may come in any order
  body: T;
};

/** The resources a request names. A request waits only while a pending close
 *  holds one of them. */
type Resources = {
  mount?: Path;    // open, close
  client?: Label;  // open
  fork?: ForkKey;  // open of a fork view, fork_create, fork_end
};

/** reconcile names nothing: a closing view is still live, so it never waits.
 *  A duplicate id in flight is malformed: the host stops, as it does today.
 *  An unknown reply id means the protocol is broken: every waiter gets HostGone.
 *  The plugin checks hello.v at start and refuses a mismatch. */
```

- **Plugin locks.** The lock is keyed by **claim handle**, so the one-pod check is atomic with storing the record.
- **When a host is idle.** One volume-scoped set decides it: records plus opens in flight, read and written only under a short volume lock. That lock is never held across a host reply.
- **Host start.** `start_host` makes the client visible only after it has replayed the recorded views.
- **Cancellation.** Publish and unpublish run to completion as spawned tasks. A cancelled kubelet call drops only its own wait.
- **Controller.** Its `serial` lock is unchanged. A fork's `DeleteVolume` is refused while the claim's record exists, so `fork_end` never waits on a held file.

### 4.7 Restart

- First, attribute the 517 MiB peak (massif at mount).
- Then load straight into the persistent structures, one tree at a time.
- Trust the checkout when a marker, fsynced last, names the built files tree.

## 5. Algorithm comparisons

| Problem | Chosen | Alternative A | Alternative B |
|---|---|---|---|
| Close | O(1) take, otherwise off-lock replay (optimistic concurrency, then one pessimistic retry) | Piece tree only: still O(calls·log n) and `same_as` under the lock, and it keeps the 659 MB peak | Move the replay off the lock only: the common case still costs O(calls) |
| Piece map | Small `Arc<[..]>`, persistent map above a threshold (as Java's HashMap turns a bin into a tree at 8) | Always persistent: one 64-slot node per file breaks the memory bar | Inline one piece: about 56 B more in every map slot, and loses the `Arc` sharing |
| Batch state | Persistent maps (Okasaki; Clojure). A batch allocates 6–34 KiB instead of 1.3–11 MiB (reviewer's `pm` probe) | Undo log: a missed inverse is silent corruption | — |
| Pack count | Geometric repack in its own task | Larger batches: fewer packs but more lag | `-a` each time: 57 s per run |
| B-F6 | Request ids plus resources (JSON-RPC/LSP ids; a lock manager's held resources) | Answer `closing` at once plus a `view_ended` event: one-writer safety moves into the plugin | — |

## 6. Examples

| Case | Today | After |
|---|---|---|
| Write, then close; no other save of the file | Clone and replay every call under the lock | Take `h.bytes`: O(1) |
| 512 MiB rewritten with the same bytes, then closed | 143 ms stall for every view | `same_as` off the lock; other views keep running |
| Another handle saved the file during this open | Replay under the lock | Replay off the lock from the journal; commit if the step is unchanged, else one retry under the lock |
| Length rule: write@0+10, append 5, truncate 3 | Fold over the calls | Length = `max(len+a, b)` composed per call (max-plus): O(1) |
| `a/f` held; `close a`, then `open c`, `close b`, `reconcile` | All four wait | Only `close a` waits |
| Re-publish of claim K at a new target while K's close is pending | The open waits | It still waits (same client and fork); it never gets `ForkTaken` |
| `fork_end K` while a view of K is closing | Waits | Waits (same fork) |

## 7. Memory budget (B/file, from 4.72 KiB)

| Item | Gain | Basis |
|---|---|---|
| 4.3 capacity, link and line maps | −336 | massif sizes |
| 4.2 no per-batch clone, owners snapshot | −1,030 | −1,070 measured free-but-resident heap, assuming the clone causes it; +100 imbl node cost (reviewer's `pm`) |
| 4.4 wake | −137 | measured, t20k_poke |
| 4.3 `ObjectState`, `PathName`, built.log on disk | −500 | estimate: 180 + 100 + 222 (massif) |
| **Result** | **≈ 2.7 KiB VmRSS, ≈ 2.0 anon, ≈ 1.7 anon with the overlay; the bar reads anon** | 0.6–0.8 of it is clean mapped git index |
| Overlay (approved) | −320 | massif: copy C less its bytes node |

## Costs and deferrals

- **Costs:**
  - Protocol v2 changes both ends in one commit.
  - Persistent maps make point lookups 1.5–2× slower. The `create` bar measures this.
  - Repack takes CPU that the builder could use, and every time history doubles it rewrites all of it (38.9 s for 403 packs). Lag rises during that, but the builder is never stopped.
  - The off-lock close adds a retry path.
- **Deferred:**
  - Untracked fsync on the view's FUSE thread: never measured.
  - Trimming `owners` and `heads` at a checkpoint: no checkpoint happens before about 70k files.
  - `malloc_trim`: it treats the symptom 4.2 removes.
  - Merging sequential write pieces.

## Slices (about 14 beads)

1. **Tracer: 4.1 plus the figures.** `close_frag` (200k random writes) and a 1M-write open with bars on the longest lock hold and on VmHWM; the oracle property test. **3 beads.**
2. **4.6, protocol and CSI.** launcher_host: `open c` is answered while `close a` is pending. csi_trees: a NodePublish of another claim completes while one close is pending. **3 beads.**
3. **4.2 and 4.4.** A counting-allocator test: a batch allocates O(batch), not O(N). **2 beads.**
4. **4.3.** `memory_create` bar read at VmHWM, on ext4 and XFS. **2 beads.**
5. **4.5.** A long-history figure from 400 packs: ms per create within 1.2× of a fresh volume. **2 beads.**
6. **4.7.** Restart VmHWM bar on a 50k-file state; time to mount. **2 beads.**

## Owner decisions (2026-11-20)

- **One shape plus an overlay**: approved. The actor holds only names and objects changed since the last built point, over the shared built shape.
- **The 2 KiB bar counts only memory the kernel cannot reclaim**: clean, file-backed git index pages are not counted. `memory_create` reads anonymous resident memory.
- **The new bars warn, they do not fail CI** (peak memory, restart peak, longest lock hold, long-history ms per create), like every release figure but `create`'s.

## New terms

| Term | Meaning | Existing term it could be |
|---|---|---|
| Pieces | A file map's pieces: small or persistent | `FileMap.pieces` |
| `ObjectState` | One object's meta, times, freshness and identity | the four `Shape` maps |
| `PathName` | One shared path string | `String` in `Names` |
| Overlay | The actor's changes not yet built | `SavedTree` |
| Resources | What a host request names | — |
| `close_frag` | Release figure: a fragmented file's close | — |

## Reconcile with existing code

- **recording/file_map.rs, mod.rs:** `changed()` becomes the test oracle. `h.calls` and `h.replayed` go. Medium cost; needs model verification.
- **builder.rs, built.rs:** persistent maps; the snapshot handover replaces `built_owners`. Medium cost.
- **tree.rs `Shape`:** merged into `ObjectState`. About 15 identity read sites change. Medium cost.
- **host.rs `deferred`/`closing`:** replaced by per-resource waits. host_client becomes a multiplexer. supervisor keys its lock by claim. contracts host.rs gets v2. High cost; touches both ends.

**For the code architects:** the piece threshold; libgit2 behaviour once repack deletes a pack (refresh and retry, tested mid-run); the frame keys the digest uses without a call list; how the journal read-back is indexed; the lock order of the wake.

**NOT RUN by me:** any re-measurement. All numbers are the findings' and the reviewers'. Also not run: XFS, CI, CSI end to end, 100k files, and any change with an A/B comparison.
