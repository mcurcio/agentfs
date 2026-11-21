# Latency: the volume lock held only for the call

**Proposed, not built.** This page proposes moving the volume's upkeep (journal compaction, history catch-up, freeing large maps) out from under the volume lock, so a call waits only for its own work. The commit that lands the last of this slice deletes this page and moves the behaviour into the library and operator docs.


**Layer: System** (Domain · **System** · Service · Module · Interfaces · Implementation). The domain is unchanged: trees, the journal, history, takes and the model's rules stay as they are. Read from origin/main 59015ea. Nothing was built, run or profiled (NOT RUN).

**What you are asked:** to agree the shape and the order of the slices, and to answer three questions. The order is: measure first, then the one measured cause, then compaction as the tracer bullet. All other work waits for a figure that justifies it.

**Answers (owner, 2026-11-21; OWNER-GUIDANCE.md, Performance):**

| Question | Answer |
|---|---|
| Q1. Which wait did "the queue flush" mean? | "Interruption to the host program due to the held locks which prevent it from opening/reading/writing new files": the volume lock held for upkeep, which this design removes |
| Q2. Must a close or fsync wait until the change is power-cut safe? | "No. If we want to take a durability approach, then I would suggest an oolong system where the queue is persisted. But it's not a requirement, just a nice-to-have". Not a requirement. Follow-up, answered "A": a close returns once its change is in the journal; fsync and FSYNCDIR keep waiting for the disk (slice 6) |
| Q3. How much should be committed now? | "Agreed": slices 0–2 and 5 committed; 3 and 4 only if slice 0 shows they matter |

**What changed since the review.** Four reviewers found two crash-order holes in the compaction plan, and a close plan that changed the order closes save in. They also found that the slices were committed before the 4-second hold had a known cause. The revision makes three changes:
- Compaction moves to history, which writes the checkpoint at its own built point into a file of its own.
- Closes stay as they are until a figure says otherwise.
- Measuring comes first.

## 1. Today

Each volume has one lock. Every recorded call and every lookup in every view of the volume waits while that lock is held. A call's own work is short: `close_4k` takes 0.91 / 1.48 ms at p50 (ext4 / XFS). The long holds come from **upkeep**, meaning work that keeps the engine tidy but belongs to no call.

| Figure | Value | What we know about its cause |
|---|---|---|
| Longest hold in the CI run (`image` 44926954986) | 4,078 ms ext4, 699 ms XFS | **Unknown.** It is the maximum over every workload, `close_1g` included, not `close_frag`'s own hold. The engine already times compaction separately (`stages.compaction`), but the figures script copies only the lock's total. |
| `close_frag`, longest hold (local) | 68–89 ms | Dropping a 200,000-piece map when history catches up. Compaction held the lock for 1–9 ms (performance.md). |
| `write_1m` compaction | 834 ms | bead ln6.9. The figure is older than later compaction changes and may be stale. |
| Slowest `create`, by tree size | 0.9 ms at 518 files, 67 ms at 50.5k; XFS up to 498 ms | Unknown. The rebuild's walk of the overlay grows with how far history trails, so it is a suspect. |
| Compaction overdue | every tree's new work stops | When the journal reaches twice its due length while steps are unbuilt, the lag gate holds all new work until history has built every step (trees.md). |

```text
flowchart LR
  V1[view A] --> L{{volume lock}}
  V2[view B] --> L
  L --> S[the call: decide · append · apply]
  L -.same hold.-> C[compaction: checkpoint, fsyncs, scans, copies]:::bad
  L -.-> R[catching up with history: overlay walk, large map drops]:::bad
  L -.-> O[rare paths: close redo, client death, fork copy]:::bad
  H[history builder] -. overdue: blocks all new work .-> L
  classDef bad fill:#fdd,stroke:#c00
```

One FUSE thread serves each view, so a slow close also stalls the other requests in its own view. `close_1g`'s close takes 3.97 s on ext4.

## 2. Requirements and background

**R1 (requirement, 2026-11-20, asked as absolute, answer "A").** When history's rebuild of a saved file disagrees, what the workload saved wins.

**Background (guidance, revisable):**
- AGENTS.md principle 1, in the repo's words: a change is recorded before the call returns. The 2026-11-13 guidance adds that it becomes power-cut safe at close or flush: "not power safe until the process closes and flushes (or the queue catches up)".
- The model's rules: one step is one journal record; a failed close saves first at the next step in its tree; a later close is the final save (git-model §3, §6, §12). These are design choices you accepted. This design keeps all of them.
- "My biggest concern right now is latency." "We haven't really gotten to background/async processing if the queue flush is a synchronous requirement to unlock." (2026-11-21)
- "Im just looking to cut waste" (2026-11-20). New bars warn rather than fail: "No" (2026-11-20). "we absolutely must avoid deadlocking" (2026-11-05). "It's all event driven" (2026-10-24).

> **Q1, general guidance. Which wait did "the queue flush" mean?**
> Four waits stand between a call and its return today.
> | Reading | Where it bites | What this design does |
> |---|---|---|
> | Compaction's and the catch-up's work inside the lock | Every view stalls for up to seconds | Moves it out: slices 1 and 2 |
> | The overdue stop: all new work waits for history to finish building | Every tree, until history drains | Removes it: slice 2 |
> | The lag and overlay gates, which slow writes to history's pace | One tree, about 2 ms per create | Unchanged. You approved this pacing on 2026-11-20 ("yes"). |
> | The journal's flush at close or fsync | Each close: p99 2.7 / 10.1 ms | Unchanged unless Q2's answer is "no" |
>
> Your answer tells us whether the slices aim at the right wait.

> **Q2, absolute requirement. Must close, fsync and FSYNCDIR keep waiting until the change is power-cut safe?**
> Today a closing program waits for its changes to reach the disk. That wait is the slowest part of a small save. The sentence that would bind: "*A close, fsync or FSYNCDIR returns only once the change survives a power cut.*" It rules out returning after the change is recorded and letting the disk catch up in the background ("or the queue catches up").
> | Answer | Gives | Costs |
> |---|---|---|
> | **Yes (recommended)** | Programs that fsync get the promise they asked for. | Every close keeps its disk wait. |
> | No | Closes return as soon as the change is recorded, which is faster. | A power cut can lose changes a program believes are saved. A separate design follows. |
>
> A yes makes this a requirement. A no opens that design.

## 3. The proposed shape

```text
flowchart LR
  V[view thread] -->|the call only| L[(volume lock)]
  L -->|after release, same thread| D[deferred drops]
  B[history builder] -->|built event| K[compactor, owned by history]
  K -->|checkpoint file at the built point| J[(journal)]
  K -.never takes.-> L
  L -->|appended records| W[durability: group fsync, outside the lock]
```

**Design rules (design choices, not requirements):**
1. **Under the lock, work is proportional to the call.** Nothing under the lock fsyncs, reads the journal, unlinks a file, walks the volume, or does the last drop of a large value. Rare fallbacks that still do so are named and measured.
2. **Each fact has one owner.** A checkpoint describes built state, so history writes it from its own trees at its own built point. The call path never waits for history to finish building before it can compact.
3. **Nothing reorders what the journal promises.** Closes keep today's order within each tree. Recovery folds every record the same whichever checkpoint survives a crash.

## 4. Canonical shape

- **Domain:** no concept changes. The checkpoint moves from the actor's write path to history, which already owns built state. git-model §3's compaction paragraph and §12's journal row are amended through the model-verification procedure. No rule in the model changes, because the journal is modelled only as rule 12. So the regression tier is enough, not the full sweep (2026-11-13).
- **Patterns:**
  - A write-ahead log with group commit (exists).
  - A checkpoint kept in its own file and named by a pointer: PostgreSQL's checkpoint record with `pg_control`, or LevelDB's MANIFEST with CURRENT.
  - An ARIES fuzzy checkpoint: redo starts at the built point, and nothing waits for history to drain.
  - Deferred drops after the lock is released, the standard scoped-guard idiom.
  - LevelDB's single mutex, released around compaction I/O.

## 5. Public interfaces (TypeScript sketches)

```ts
type RecordNo = bigint;

/** Written by history's compactor from its own state at its built point B.
 *  The actor contributes nothing, and the volume lock is never taken. */
interface Checkpoint {
  built: RecordNo;                 // B: redo starts at B + 1
  pointers: RefTargets;            // the built ones
  objects: ObjectTables;           // per tree
  locals: TreeLocals;              // directory numbers and times, file times, from history's trees at B
  stepHeads: Map<RecordNo, Oid>;   // steps at or below B that later records may still name
  opens: OpenRecord[];             // write-opens with no close at or below B
  heldPageWrites: RecordNo[];
  carried: FrameCopy[];            // live frames from segments this checkpoint lets go
}

/** History's port onto the journal. Never imported by the call path. */
interface JournalCompaction {
  waitDurable(through: RecordNo): Durable | Stopped;
  /** Writes, fsyncs, renames into place, syncs the directory. */
  publish(cp: Checkpoint): Published | CompactError;
  /** Only segments wholly covered by a published checkpoint. */
  unlinkCovered(by: Published): Dropped | CompactError;
}

type CompactError =
  | { kind: "notPublished" }   // nothing changed; alarm, retried at the next built event
  | { kind: "stopped" };       // a journal segment's fsync failed: the actor stops, as today

/** The lock guard collects large values and drops them after release, on the same thread. */
interface DeferredDrops {
  defer(value: FileMap | BaseFile | OverlayEntries): void;
}

/** Added to Recording, --metrics-output, trees-figures and OpenTelemetry. */
interface LockFigures {
  lockWait: Histogram;                   // what a call feels
  lockHold: Record<HoldKind, Histogram>; // call, catchUp, compaction, closeRedo, clientDeath, forkCopy
}
```

**The crash contract (invariants on `JournalCompaction`):**
- **C1.** A checkpoint is published only after every record at or below B is durable.
- **C2.** A segment is unlinked only after a published checkpoint covers it, and only once every frame a later replay needs has a durable copy.
- **C3.** History forgets the built-log entries and step heads a checkpoint covers only after that checkpoint is published.
- **C4.** A record folds the same on the old checkpoint and on the new one.
- **C5.** A torn journal tail can never remove a checkpoint, because a checkpoint is not in a segment.
- **C6.** Space held by unlinked segments that readers still use counts toward the journal's length and the tree's space limit. A holder moves to the carried copy without a walk under the lock.

**Who is given what:**

| Part | Given |
|---|---|
| View host, `TreeSession` | unchanged |
| History's compactor | `JournalCompaction`, history's own trees. Never the actor lock. |
| Lock guard | `DeferredDrops` |
| `TreeActor` used directly, or `Recording` | the same history, and so the same compactor: there is no second mode |

**Lock order:** the actor, then history, then durability. The compactor takes only history and durability.

## 6. Design it twice: who writes the checkpoint

| | **Chosen: history, at its built point, in its own file** | Actor: a snapshot at record N under the lock, written outside it |
|---|---|---|
| Under the lock | nothing | a snapshot. The directory times need a persistent copy of the shapes, or the snapshot walks the volume. |
| The overdue stop | gone: compaction never needs every step built | stays |
| Naming across the gap | one rule (C4) | records written between the snapshot and the checkpoint fold on neither side. That needs the same rule, plus a durability order for them. |
| Restart | replays from B, so longer when history trails | replays from N |
| Unknown | whether history can know the opens and held page writes at B | none new |
| Gives up | restart time while history lags | the overdue stop stays: work stops until history drains |

Two whole-system alternatives were set aside:
- **Upkeep cut into slices of about 5 ms under the lock.** Compaction's fsyncs cannot be cut into slices, and the total lock time stays the same.
- **One lock per tree or per file.** Steps still meet at the single journal append, and ordered multi-lock steps are a deadlock risk (2026-11-05). Revisit only if `lockWait` p99 stays over 1 ms with 8 busy views. Read snapshots for lookups (RCU) would be the next step after that.

## 7. What stays synchronous

| Under the lock | Why |
|---|---|
| Decide the step, check the pointers, append one frame, apply it | Principle 1's commit point. Record order is the model's order. |
| A close's land: compare, then record. Today's redo when the base moved. Pending and client-death closes. The fork copy. | Moved only if slice 0 shows them causing long holds (§8) |
| **Outside the lock, as today:** the disk wait at close, fsync and FSYNCDIR; the lag and overlay gates | Q2 |

## 8. What happens next

| # | Slice | Proven by |
|---|---|---|
| 0 | **Measure.** Hold by kind and `lockWait` in the figures and OpenTelemetry. `close_1g`, `close_frag`, `write_1m` and `create` each run alone. | CI read before slice 2 is cut. It names the cause of the 4,078 ms. |
| 1 | **Deferred drops.** Catching up with history drops replaced maps and base files after the lock is released. | `close_frag` longest hold under 10 ms. A guard in tests that panics on an fsync, journal read, unlink or large drop under the lock. |
| 2 | **Tracer bullet: compaction off the lock.** History writes the checkpoint file at B. The overdue stop is deleted. | Power-loss runs (`power-loss-trees.sh`) with marks between write, rename, directory sync and unlink, while records after B are not yet durable. A conformance leg that parks the compactor at each gap across model steps. New work admitted while history trails past twice the due length. Phase-1 regression seeds. |
| 3 | *Gated on slice 0:* the catch-up's overlay walk, using layers frozen at a record and dropped whole once built, never before history's check of that save (R1) | `create`'s slowest call flat across tree sizes; reads identical before and after |
| 4 | *Gated on slice 0:* client-death closes, one handle at a time on the unbind thread; the fork copy, made durable before its record | the other view's `lockWait` p99 during a 1M-call client death |
| 5 | One copy of each open's calls (ln6.10, cutting waste) | `write_1m` extra peak memory (warns) |
| 6 | **A close returns once its change is in the journal; fsync and FSYNCDIR still wait for the disk** (owner, 2026-11-21, option A). The power-cut wait leaves close and stays on fsync, FSYNCDIR and O_SYNC writes. | `close_4k` p50 and p99 before and after (warns); a power-loss run that cuts power after a close with no fsync shows the volume opens and loses at most that unsynced save; one that cuts after fsync loses nothing |

- **Tracer non-goals:** relocation mechanics beyond C6, closes, layers, memory, views.
- **Per your 2026-11-20 answer, the new bars warn.** Each slice reports its figure.

> **Q3, general guidance. How much should be committed now?** Today two beads are open (ln6.9, ln6.10).
> | Option | Gives | Costs |
> |---|---|---|
> | **Slices 0–2 and 5 committed; 3 and 4 only if slice 0 shows they matter (recommended)** | Removes the measured causes and the overdue stop | About 3 beads plus measuring. A cause slice 0 finds may add more. |
> | The same, plus a close that runs off the view's single thread | A sidecar's single view stops pausing for about 4 s on its own large close | Every request type must respect a save ordering. Missing one breaks close-to-open, where the next open sees the last close's bytes. |
> | Only ln6.9 and ln6.10 | The two residuals as filed | The overdue stop stays, and the catch-up drops stay under the lock |
>
> Your answer chooses which bead outline is cut.

## 9. Costs and what is given up

- **One new thread per volume:** the compactor, which history owns and built events wake.
- **State format agentfs-trees-13 becomes 14.** Older volumes do not open and are not converted (trees.md policy).
- **Longer restarts while history trails,** because redo starts at the built point.
- **Errors arrive later:** a failed compaction raises an alarm at the next built event, not at a call.
- **Deferred drops still cost the dropping thread time.** For catch-up inside a call, that is the view's own thread. The lock is free.

## New terms

| Term | Meaning | Why needed | Existing term it could be |
|---|---|---|---|
| upkeep | work that keeps the engine tidy and belongs to no call | names what moves | — |
| hold kind, lock wait | the longest hold per cause; the time a call waits for the lock | the proof for each slice; the latency felt | `Stages::actor_lock` |
| checkpoint file | the checkpoint, kept outside the segments | a torn tail cannot delete it | the checkpoint record |
| compactor | history's thread that writes checkpoints and drops segments | compaction leaves the call path | compaction |
| overdue stop | the lag gate holding all new work until history is built | removed | `compaction_overdue` |
| deferred drop | a large value dropped after the lock is released | frees leave the lock | — |
| frozen layer | an overlay layer closed at a record, dropped whole once built and checked | slice 3 only | the overlay |

## Reconcile with existing code

| Existing | Becomes | Cost |
|---|---|---|
| `commit_point.rs::compact`, `compact_if_due`, `Journal::checkpoint`, `drop_behind`, `relocate_frames` | history's compactor and `JournalCompaction` | medium; power-loss proof redone |
| `Journal::open`: the checkpoint as a segment's first frame; the torn-tail removal | the newest published checkpoint file, then replay from B + 1 | small to medium |
| `lag.rs` `overdue` term | deleted | small |
| `rebuild`, `Files::rebuilt`, `BaseFile::drop` under the guard | values returned to the guard and dropped after release | small |
| `ci/trees-figures.py` copying only `actor_lock` | every stage plus hold kinds | small |

## For the code architects (parked)

- How C4 is kept: the checkpoint's step heads against the actor switching its pointers to commit ids.
- How C6 is kept: a relocated copy built outside the lock and swapped in, or record-number addressing through a segment table.
- How history learns the opens and held page writes at B. If it cannot, the alternative in §6 comes back to this gate.
- Whether the compactor gets its own thread or shares the builder's.
- Frozen layers: a bound on how many there are, and how `readdir` merges across them.
- Check in slice 0: the errata report that the notes lock is held across an fsync on compaction's call path. It was not found in the code.

*History: v1 2026-11-21, synthesis of two drafts; v2 2026-11-21, revised after four architects' reviews.*

---

## Binding corrections from the second review (2026-11-21)

A second panel (durability, concurrency, git model, simplicity) reviewed the revision and slice 6. Its findings, with evidence, are in `/tmp/claude-0/-home-user-agentfs/d9d7fd5b-08fa-53a9-9d39-8e3a2341a15d/scratchpad/latency-rereview.md` until this page is built. These corrections bind every slice and override any line above that disagrees.

**Compaction (slice 2):**
- **C2a. A segment is unlinked only after the actor has rolled past it.** The roll is an actor event that history receives. History never decides from a length check alone. A conformance leg parks the compactor between its coverage check and its unlink while the actor appends.
- **C7. The journal always opens.** After any step of compaction and any power cut, the salt, the redo start B+1 and "no record after B" come from the checkpoint file. A missing or wholly torn segment after B is a torn tail, not corruption. Slice 2's power-loss marks include "covered segments unlinked, next segment's first frame not durable" and "no segment left".
- **C3 and C4 rewritten.** The checkpoint keeps the latest step head of every pointer at B, which is bounded by the number of pointers. History keeps each head until its built point passes the record at which the actor switched that pointer to an oid. The actor emits that switch as an event under its lock.
- **C8. Every field of today's `Entry::Checkpoint` has a named source at B.** The recommended source is the recovery fold (`journal/fold.rs`), run by history over the records at or below B. The architect lists every field and its source before slice 2 is cut.
- **C9. No lock the actor takes is held across I/O or a wait.** That covers History's state, Durability's state, the segment table and relocations. The segment table and the journal length have one owner, with a place in the lock order. The compactor holds History's lock only to clone persistent maps.

**Deferred drops (slice 1):**
- **Large drops leave request threads altogether.** Replaced values travel back through History's state, the transport `publish_maps`/`take_maps` already uses, to a thread that serves no requests. Slice 1 is proven by the latency of another file's request during `close_frag`, not by the lock hold alone.

**Close stops waiting for the disk (slice 6):**
- **Contract.** FLUSH (close) returns once the open's records are appended. It never waits for an fsync and never returns a journal fsync error. fsync, FSYNCDIR and O_SYNC/O_DSYNC writes still wait for the disk.
- **D1, the invariant that makes this safe.** A power cut loses a suffix of the journal: no record survives without every earlier record. After a power cut, the volume equals an uninterrupted run cut after some record r. That r is at or after the last record covered by a returned fsync, FSYNCDIR or O_SYNC write. The loss is every record after the last such call or builder batch, which can be many saves, not one.
- **D2. A change outside the journal that destroys data waits until the journal record carrying that data is durable.** For example, a move from the untracked area into the tree unlinks the area's file only after its record is durable. The wait is outside the actor's lock. This bug exists today, and slice 6 widens its window.
- **Proof.** The power-loss oracle checks four things: the recovered journal is a prefix of the appended records; it holds every record up to the last returned fsync, FSYNCDIR or O_SYNC write; the volume opens; and recovery saves what survived. One leg runs close A, rename A over B, close C, create D with no fsync. Another runs the editor pattern: an untracked temp file, fsync or no fsync, then a rename into the tree. The existing XW marks in `power-loss-trees.sh` become may-lose.
- **Docs and the model.** Slice 6 changes git-model §3 rows 142 and 179 and §18 row 1058, through the model-verification procedure. It also changes guarantees.md, trees.md and the flush in tree_view.rs.
- **Cost (§9).** A power cut can lose every change since the last fsync, FSYNCDIR or O_SYNC write, or since the last builder batch.

**Measurement (slice 0):**
- **Measure the interruption at the FUSE edge.** Request latency by op, a probe on another file in the same view and in another view during the workloads, and gate waits by cause. If gate waits or the view's single thread outweigh lock holds, the mapping of the owner's Q1 answer goes back to the owner before slice 2.

## Review responses

| Finding (reviewers) | Response |
|---|---|
| Records written between the snapshot and the install fold on neither checkpoint, because a step and a commit id never match (all four) | **Right.** Checked in `commit_point.rs`: the pointer swap happens in the same hold as the checkpoint. Fixed by having history write at its built point, so no records sit in a gap, and by C4. |
| A torn tail removes the later segments, the new checkpoint with them, leaving no checkpoint at all (three) | **Right.** Checked in `journal/mod.rs`: at a hole it removes every later segment, then refuses to open if no checkpoint remains. Fixed by C1, C2 and C5: the checkpoint is its own file. |
| `checkpointed` called too early corrupts the built log on restart (1) | **Right.** Covered by C3. |
| Writing chunks in short holds breaks the rule that a step's chunks come just before it (three) | **Right.** That slice is dropped. |
| The per-object close fence hangs instead of failing, breaks close-to-open, misses opens, and changes the per-tree order without a model check (all four) | **Right.** The close worker is dropped. Closes keep today's per-tree order and inline retry, and any move waits for a figure. |
| The fork copy reverses the lock order between the actor and the untracked area, or splits the copy (two) | **Right.** It is gated (slice 4), and the copy must be durable before its record. |
| Moving holders lazily pins space without bound and outside the accounting (two) | **Right.** C6 makes the space counted, and moving a holder is a contract. The mechanism is parked. |
| The snapshot misses the directory times, which cost a walk, and "superset" was wrong (three) | **Right.** They come from history's trees at B. The wording is removed. |
| A worker mode for `TreeActor` used directly hides the gap from conformance and is a policy leak (three) | **Right.** The compactor belongs to history, so there is one path, and a conformance leg parks it. |
| The tracer was committed before attribution. close_frag's 136 ms was a misattribution. (two) | **Right.** Slice 0 measures first, and the table is corrected from performance.md. |
| Q1 misstated the 2026-11-20 "yes", and the overdue stop is literally a flush that must finish first (one) | **Right.** That "yes" was about pacing writes to a memory budget. Slice 2 removes the overdue stop, and Q1 lists all four waits. |
| R1 and R2 were not requirements (four) | **Right.** They are now background. The power-cut half is asked as Q2. |
| Reading 13 alongside 14 is backward compatibility nobody asked for (one) | **Right.** Removed. |
| A reclaimer thread is not needed (two) | **Right.** It is replaced by drops after the lock is released, on the same thread. |
| "Install" collides with `Tree::install`, and telemetry should go through OpenTelemetry (one) | **Right.** The word is dropped, and OpenTelemetry is added. |
| The page went below its layer, into fsync sequences and retry counts (two) | **Right.** The contract stays as C1–C6, and the order of steps is parked. |
| Re-asking whether a latency figure should fail CI | **Withdrawn.** Your 2026-11-20 "No" stands, and the question slot goes to Q2. |
| The single-view stall goes unaddressed (one) | **Partly right.** It is offered in Q3 rather than recommended, because its ordering fence is the riskiest change on the page. |
| "One lock is wrong" | **Not raised.** All four reviewers tested the single lock and it held. |

**Not run:** no build, test, model run, power-loss replay or profile. Every check here is a reading of origin/main 59015ea.