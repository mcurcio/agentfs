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

**Answer (owner, 2026-11-22, general guidance; OWNER-GUIDANCE.md, Performance).** Slice 2 stopped a fourth time: the engine replaced a branch's placeholder (the commit that step N will build) with the built commit id, in memory only, and a volume shut down after that could fail to reopen. Asked whether to limit the replacement and teach recovery that the two names are one value (A), write the replacement into the journal as its own record (B), or park slices 2 and 6 (C), the owner answered "B". Slice 2's page below is cut for it.

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
- **C6.** Space held by unlinked segments that readers still use counts toward the journal's length. A holder moves to the carried copy without a walk under the lock.

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
---

## Frozen slice pages

Written by the code architect on 2026-11-21, after slice 0 (9eae003). These pages bind the implementers of slices 1, 2, 5 and 6. They override nothing above; where a page settles a point the page parked, it says so. An implementer who hits a wall appends to `.claude/errata.md` and stops: a signature here changes only through an erratum and a re-cut.

The interfaces are committed as compiling code with `todo!()` bodies and no behaviour change: 5a80783, e1eee36, e4beefd and a5ef18e on `claude/kind-fermi-vvxywi`. Each `#[expect(dead_code)]` names the slice that removes it.

### Lanes and order

| Lane | Slices, in order | Why this order |
|---|---|---|
| A | 2, then 6 | Both rewrite the journal's durability story, `journal/`, the power-loss oracle and git-model §3. Slice 6 reuses slice 2's power-loss harness. |
| B | 1 | One file. Independent of the others. |
| C | 5 | The close's call merge and the builder's replay. Independent of the others. |

**Slices 3 and 4 stay gated.** Slice 0 gives no figure that justifies either one. `create`'s slowest call was 116 to 153 ms against a catch-up hold of 120 ms, measured at one tree size, so nothing shows the growth with tree size that slice 3 targets. No workload exercised a client death or a fork copy, which slice 4 targets.

**Owned paths are disjoint.** Lanes B and C have landed (slice 1 in 331b8b2 and b60dcc4, slice 5 in 3d5512f), so lane A's list is slice 2's alone; every file slice 2 touches is in it, each with its reason under "Slice 2" below. Every lane builds into its own worktree's `target/`. No lane adds a dependency, so `Cargo.lock` is untouched. A lane that needs a file outside its list writes an erratum and stops.

| Lane | Owns (repo-relative) |
|---|---|
| A | Slice 2 (re-cut 2026-11-22): `crates/agentfs-core/src/tree_actor/journal/`; `crates/agentfs-core/src/tree_actor/history/{compactor.rs,compactor/,built.rs,lag.rs,built_log.rs}`; in `crates/agentfs-core/src/tree_actor/history/builder.rs`, only the switch record's arm in `BuildState::apply` and the comment at the heads trim (lane C has landed); `crates/agentfs-core/src/tree_actor/{mod.rs,call_path.rs,stages.rs}`; in `crates/agentfs-core/src/tree_actor/pointers.rs`, only the deletion of `to_targets`; `crates/agentfs-core/src/{session.rs,store.rs,state_area.rs,telemetry.rs,failpoints.rs}`; one line of `crates/agentfs-core/src/tree_actor/recording/pages.rs` (slice 2's guard); `crates/agentfs-core/tests/{tree_actor.rs,tree_stages.rs,tree_dirs.rs,crash_points.rs,support/trees.rs}`; `crates/agentfs/src/launcher/{mod.rs,serving.rs}`, `crates/agentfs/src/reference_tests.rs`; `crates/agentfs-fuse/src/tree_view.rs`, `crates/agentfs-fuse/src/tree_view/untracked.rs`; `crates/agentfs-conformance/`; `crates/agentfs-model/VERIFYING.md` (only the Phase 1 paragraph that names the mounted replay's gates); `ci/e2e-tree-recording-faults.sh`, `ci/power-loss-trees.sh`, `ci/power-loss-trees-check.sh`, `ci/trees-figures.py`, `ci/measure-trees.py`; `docs/target-architecture/git-model.md`, `docs/trees.md`, `docs/guarantees.md`, `docs/telemetry.md`, `docs/architecture.md`, `docs/configuration.md`, `docs/library/core.md`, `docs/runbooks/upgrading-the-sidecar.md`; `.claude/errata.md` |
| B | `crates/agentfs-core/src/tree_actor/history/retired.rs` |
| C | `crates/agentfs-core/src/tree_actor/recording/{pages.rs,layout.rs,mod.rs}`; `crates/agentfs-core/src/tree_actor/history/{builder.rs,verify.rs}` |

**Interfaces lane A must keep, because lane C's files call them:** `Entry::Checkpoint` keeps its name (`builder.rs` matches `Entry::Checkpoint { .. }`, which also matches a tuple variant); `Durability::{segments, relocations, mark, appended}`; `CallReader::of_segments`; `Journal::call_reader`; `CallReader::calls`; `History::{checkpointed_through, publish_maps, next_work, failed_event, batch_read, repo}`; `BuildState::at`'s parameters; `FramePos::relocate`; `pages::relocate`; `FileMap::relocate`. **Lane C keeps** `pages::ObjectPages`' fields `writes` and `saved_through` and their visibility, which `journal/commit_point.rs` reads until slice 2 deletes those readers.

**Left to the milestone's docs bead, after every lane lands:** `docs/performance.md` (each slice's figures, from its report), `docs/architecture.md`'s `retired.rs` row and `docs/telemetry.md`'s "drops" kind after slice 1, the `README.md` if anything user-visible changed, and deleting this page.

### Slice 2: compaction off the lock (tracer bullet)

**Bead:** agentfs-ln6.9. **Lane A, first.** **Delivers:** another view's call never waits for compaction. The overdue stop is gone. Slice 0 measured compaction as the only cause of the longest stall in another view: 1,244 ms in `close_1g`, 1,166 ms in `write_1m`, 233 ms in `close_frag`.

**Settled here (parked above; amended after the protocol review, see "Review responses (slice 2)"):**
- **C4: one step head per pointer, kept while the fold gives a step (re-cut 2026-11-22 for the owner's answer "B").** A record after B names `Step(s)` for a pointer set at step s ≤ B until the switch record for that pointer: as a target, as a change's `old`, or as the switch record's own `step`. Row chains are included because a row step names its chain's current value as `old` (`mod.rs:1543-1550`) and recovery's fold checks every `old` (`fold.rs:535`). Today's code leaves chains out only because its compaction switches every pointer in the same hold (`commit_point.rs:191-198`). A checkpoint at B sets `step_heads[p] = s` for every pointer the fold of the records at or below B gives as `Target::Step(s)`. There is no exception. No oid is stored: at B it is `Checkpoint::pointers[p]`, and on a restart it is the built log's value `(s, p)` (`mod.rs:505-523`), which `checkpointed_through` keeps. The fold seeds `Folded::pointers` with `Target::Step(s)` for each head of the checkpoint it starts from, and with `Target::Oid(pointers[p])` for every other pointer. So a fold starts at B with the pointers the fold of the whole journal has at B, and every record after B folds the same on either checkpoint. That equality is all of C4. **Heads are still needed with the switch recorded.** The compactor publishes at History's built point, and the actor appends the switch record for a step only when it next acts on that built point. So the switch record for step s always has a number past the built point that built s, and a checkpoint at that built point folds the pointer as `Step(s)`. After that checkpoint, a save of the same tree names `Step(s)` as its `old`, a fork names it as its branch's target, and the switch record names it as its `step`. A head goes at the first checkpoint whose built point is at or past its pointer's switch record, where the fold gives an oid, or past a later step on the pointer, which then has the head. That is one head per pointer at most, as before.
- **The switch is a journal record (owner, 2026-11-22, general guidance: "B"; re-cut 2026-11-22).** Until this re-cut the actor turned a pointer from `Step(s)` into its oid in memory only. Later records named the oid as the pointer's `old`. A fold that began at a checkpoint older than the switch still held `Step(s)`, so it failed with `PointerMoved`, and a cleanly shut-down volume did not reopen (`.claude/errata.md`, the fourth block). Now every change of the actor's pointers is a record's: a step's, or a switch's.
  - **The entry.** `Entry::Switch { pointers: Vec<Switched> }`, never empty, with `Switched { name, step, oid }`: pointer `name`, whose value was `Target::Step(step)`, is `Target::Oid(oid)` from this record on. `Switched::change` gives it as a `PointerChange` (`old` the step, `new` the oid), the one form the actor applies and the fold replays. It is not a step: it has no writer, builds no commit, adds to no tree's lag, and `History::appended_step` is never called for it.
  - **When it is appended.** By `TreeActor::switch_built`, under the actor's lock, in the two places the actor acts on a built point. The first is `record_with`, after a step's record is appended and applied and before `History::appended_step` tells the builder of that step. The second is `on_built`, the built event that `Recording::progress` delivers. `caught_up` (`rebuild`, `relocate_if_moved`) no longer calls it. `switch_built` reads History's snapshot. It takes every pointer whose value is `Target::Step(s)` with s at or below the snapshot's built point and which the snapshot holds, each with the snapshot's oid, row chains included. With none, it appends nothing and returns `None`. Otherwise it appends one record that names them all, with `append_record`, then applies them with `Pointers::apply_targets`, and returns the record's number. Append first, apply second, as a step does. A failed append changes no pointer, is logged as `tree CompactionFailed: the switch record: …` and is tried again at the next built point; a journal that can no longer be trusted stops the actor, as for any append. It does nothing once the actor has stopped. It costs O(1) when History's built point has not moved since its last full run, and O(pointers) plus one append when it has. It opens no section of its own, so its time counts to the hold it runs in. It fsyncs nothing, reads no journal and unlinks nothing, so the guard (2.10) has nothing to exempt.
  - **Why "which the snapshot holds".** A pointer's value names its own step, with one exception: a fork's branch, which the fork step f sets to its upstream's value `Step(s)`, s < f. That branch is a new name at f (`old: None`; fork numbers are record numbers and never repeat). So the snapshot holds it only once f is built, and its built value is then the value of s.
  - **Where it never lands.** Not between a step's chunk records and the step. Not between a step reading its own number (`fork_with`, `record_rows`, a drop, a save's `self.next + chunks`, a take) and its record: `before_record` never calls `switch_built`. It comes right after a step's record, or in `on_built`'s hold. The `recorded != seq` checks those steps already make stay as the guard.
  - **Why before the builder is told.** A batch reads through `Durability::appended()` at its start. The switch record is in the journal before `appended_step` wakes the builder, so the batch for a step also reads the switch record behind it. A test that settles between steps keeps one batch per step, and the tests that kill at the n-th batch (`tree.build_*=kill@N`) keep their batches. The implementer confirms this on `a_step_killed_after_its_record_is_not_saved_again` and `a_fast_forward_killed_at_any_step_lands_once`.
  - **The builder.** `BuildState::apply` gains one arm for the record, and the arm does nothing: the builder's pointers are built values already. After `on_built`'s switch no step follows, so `on_built` asks the builder to build through the record with `History::want_through`, as `rebuild` does for an overlay sealed past the last step. That is one batch with no step when a volume goes quiet: one journal fsync and one built-log entry. It keeps a volume at rest built through its last record. So what waits for the built point to reach the last record still ends (`drain` in `ci/trees-figures.py`, and the power-loss driver at `ci/power-loss-trees.sh:147-148`), and the next compaction keeps no head. It cannot loop: only a step makes a `Step` value, so a batch with no step leaves nothing to switch.
  - **Durability: with the journal's suffix, no fsync of its own.** The record is appended as a call's is, and is durable at the next group fsync: a caller's fsync, the builder's next batch, or shutdown. Nothing needs it sooner, for three reasons. Every record that names the switched oid comes after it, and a power cut loses a suffix of the journal (D1 above; recovery already cuts at the first invalid frame), so a surviving record always has its switch record before it. A checkpoint that keeps no head for the pointer has its built point at or past the switch record, and is published only once every record at or below that point is durable (C1). History lets the builder trim the built log and its heads only after that checkpoint is published (C3). If a power cut loses the switch record, the pointer folds as its step, whose value the built log still holds, and the reopened actor switches it again by a new record. A kill loses no appended record.
  - **Recovery.** The fold replays the record: `check_targets`, then `apply_targets`, over `Switched::change`. A switch whose pointer is not at that step is `PointerMoved`, as a step's stale `old` is. A fold from a checkpoint at or past the record never reads it: that checkpoint's `pointers` hold the oid, and it keeps no head. `Replay::apply` ignores the record. `Live::names` is false for it, so it is never carried. `TreeActor::open` takes its pointers from the fold, steps included, so a reopened actor switches by a record too.
  - **What goes.** `History::switched`, History's `switched` map and its keep-until-published rule, `AtBuilt::switched`, and the "unless switched" clause of C4. `History::published` drops no switch, and `compactor::checkpoint_at` reads the heads from the fold alone. The stubs' "reaches `at`" (`built.rs:545-549`) and "passes" (`compactor.rs:42-47`) go with them. The second review's "the actor emits that switch as an event under its lock" is now the record.
  - **What stays.** `step_heads`, for the reason under C4. `Published::oldest_head()` and the builder's trim: it drops heads and built-log entries at or below `checkpointed_through()`, which never passes `oldest_head() - 1`, and recovery gives it no row-chain heads (`mod.rs:800-805`). Both are still read: a restart resolves a head's step from the built log, and the builder resolves a fork's `Step(s)` after the checkpoint from its heads.
  - **Record numbers.** A switch record takes a record number. Object, open, fork and event numbers are record numbers, so they now depend on when the actor saw a built event. They stay unique and increasing, which is all the engine, the model's replay (`engine_names.rs` orders forks) and history rely on. On a mount they already depended on timing: page writes and takes are records too.
- **C6: holders move at the built event, as a named fallback.** The compactor copies live frames into `carry-<B>` files and records each copy in `Relocations`. A holder that still names an unlinked segment keeps reading it through its `Arc<Segment>`, and those bytes count in `JournalCompaction::len` until the last holder lets go. The builder moves its positions at its next batch, as today. The actor moves its positions in `TreeActor::relocate_if_moved`, which `on_built` runs only when `Relocations::generation()` moved. That walk is the one piece of compaction left under the lock, timed as `Hold::Compaction` (design rule 1: "named and measured"). It runs only when a live open's dead neighbours outweigh its own frames.
- **The compactor has its own thread**, started beside the builder, ended once History is stopping, and joined by `TreeActor`'s shutdown before the builder's join and before the lease is released. A publish in flight finishes or leaves only `checkpoint.tmp`.
- **When to compact has one home**, `history::compactor::compact_at(from, len_after_last)`, which is `max(from, 2 × len_after_last)` with `from` the volume's `Upkeep::compact_from` (64 MiB by default). Only the compactor thread reads it, at a built event. `commit_point::compact_at` and `TreeActor::compact_threshold` are deleted. The only other decider is the volume's compaction gate (`Upkeep::gate`, below): with a gate, the compactor compacts at every built event and ignores `compact_at`. `compact_once` returns `None` before History's first built event since open, and when B is not past the last published checkpoint's `built`. Otherwise it publishes a checkpoint at B and unlinks what is covered, which may be nothing. (Re-cut 2026-11-22: this replaces "or when nothing is covered past the last checkpoint", so a gated test of a small volume publishes at every built event.)
- **Open does not compact.** History's trees are empty until the builder's first built event (`built.rs:157-159`, `:309`), so a checkpoint made before then would carry empty `locals`. `State::trees` becomes `Option<imbl::OrdMap<TreeId, Tree>>`, `None` until then. `History::at_built` returns `None` while it is, and `built_trees` reads the same `Option` in place of its `is_empty` guard (`built.rs:869`). A reopened volume compacts at its first built event, once due. Every test and script that relied on open compacting, or on "the first call after the builder caught up compacts", is listed under "Tests and scripts that assumed a compaction at open or at a call" below, with what it becomes. This replaces "open still compacts". Conformance still compacts across reopens through the gate (2.7).
- **B may fall between a take's chunk records and its step.** The fold's `bytes` (chunks with no step yet) become the checkpoint's `chunks`, and `carry` copies them.
- **`Durability` is the one owner of the segment table and the journal's length.** It already holds `segments`, `add_segment` and `remove_segment`. **C2 and C2a are one rule there: a segment is covered when the next segment in the table starts at or before B + 1.** The actor adds each segment under its lock before appending to it (`durable.rs:289-295`; `journal/mod.rs:1038`, `:1157`). That call is the roll event the second review asked for. The newest segment, whose end is not yet known, is never covered. `History::rolled` and `AtBuilt::rolled` are deleted. Lock order: the actor, then History, then Durability, then Relocations. The `Journal` the actor holds keeps only its append position.
- **One order, enforced when each value is built:** `wait_durable`, `fold_through`, the checkpoint built with `carried` empty, `carry(&checkpoint)`, `Sealed::new(checkpoint, durable, carried)`, `publish(&sealed)`, `History::published`, then `unlink_covered`. `Sealed::new` is the only maker of the only value `publish` takes. It refuses a checkpoint whose `built` passes `durable.through` with `NotPublished`, which is logged and retried. It writes `Checkpoint::carried` from the `Carried` it consumes, the one source of that list. The check runs when the seal is made, not at compile time: the types force the order, and the record numbers are compared at run time.

**Settled in the re-cut of 2026-11-22 (after the second block; see `.claude/errata.md`).** The design above is unchanged. These settle how tests and tooling reach compaction, so no test depends on open or a call compacting.
- **A volume's upkeep is an option of its open, never process state.** `Upkeep` holds the segment size, `compact_from` and the gate. It is passed to `TreeActor::{create_with, open_with}`, `Recording::{create_with, open_with}` and `TreeStateArea::with_upkeep`. `create`, `open` and `TreeStateArea::at` use `Upkeep::default()`, which is today's behaviour: 64 MiB segments, a 64 MiB floor and no gate. The library reads no environment variable for it. The binary reads `AGENTFS_COMPACT_GATE` at its edge, in one function of `launcher/mod.rs` (`upkeep()`), and only in failpoints builds. Every open of a tree volume there calls it: `mount` and `host` through `serving.rs`, `init`, and `fork end`. So in-process tests that open volumes in parallel each carry their own gate, and none parks another's compactor.
- **The compaction gate is the one mechanism that parks, kills and fails a compaction.** `tree.compact_before_checkpoint`, `tree.compact_before_fsync` and `tree.compact_before_drop` are deleted from `failpoints::POINTS` and from `journal/mod.rs`. They have no rows in `docs/configuration.md`, so nothing is removed there. `every_hit_is_listed` keeps `POINTS` equal to the code. A kill is a process killed while its compactor is parked at a gap (the e2e script below), or the state area copied while parked (2.1, 2.14). Power loss is 2.6's marks, taken while parked. An error is a gate that returns `Err` at a gap: the compaction fails there, classed by `Gap::fault`:

  | Gap | Where it sits | An `Err` there |
  |---|---|---|
  | `Covered` | after the coverage check, before `wait_durable` | `NotPublished`: nothing written, retried at the next built event |
  | `Written` | `checkpoint.tmp` written and fsynced, before the rename (inside `publish`) | `NotPublished`: `checkpoint.tmp` stays and open ignores it |
  | `Renamed` | after the rename, before the directory's fsync (inside `publish`) | `Stopped`, as that fsync failing: `Durability::fsync_failed`, and the actor stops |
  | `DirSynced` | after the directory's fsync, before `History::published` | `NotDropped`: `History::published` still runs, and nothing is unlinked |
  | `BeforeUnlink` | after `History::published`, before `unlink_covered` | `NotDropped`: the checkpoint stands, the segments stay for the next compaction |

  The gate is called on the compactor thread with no lock held: not History's, Durability's or the relocations' (C9).
- **The published event.** `History::checkpointed()` and `History::wait_checkpointed(after)` report a generation and the `built` of the last checkpoint. The generation counts the checkpoints this open's compactor has published. It is 0 for the checkpoint `Journal::open` read, which `start_history` passes to `History::new` in place of `checkpointed(folded_built)`. Tests and tooling read the generation, act, then wait past it. Nothing waits on `journal_dropped`, a length or a timer. `History::published` is the compactor's alone and the one place the generation moves. In the same hold it moves History's epoch, so the `--metrics-output` worker (`tree_view.rs`, `metrics_worker`, which waits on the epoch) rewrites its file at each published checkpoint. It also adds one to `Stages::checkpoints`, exported to OpenTelemetry as `agentfs.journal.checkpoints` and in the metrics file as `stages.checkpoints`. `Stages::compaction` (existing) now times each compaction on the compactor thread, from the coverage check to the unlink, whether it published or not. When a compaction ends in `Stopped`, the compactor records the failure through `Durability::fsync_failed` first, so the actor's next call stops. It then calls `History::compaction_stopped`, which ends every `wait_checkpointed` with `WaitError::Stopped` and ends the compactor thread.
- **How a test makes a volume due without 64 MiB.** Its `Upkeep`. A gate compacts at every built event; a small `compact_from` makes the journal due early; a small `segment_bytes` makes segments roll, so they can be covered. `Upkeep::gated(gate)` is one frame per segment plus the gate, so every built event covers every segment but the newest. These are library options, not configuration keys. `docs/library/core.md` documents `Upkeep` and the published event. `docs/configuration.md` gains only `AGENTFS_COMPACT_GATE`, beside `AGENTFS_TAKE_GATE` under "Test builds only", and `reference_tests.rs`'s `every_environment_variable_is_documented` chains `failpoints::COMPACT_GATE_VAR` beside `failpoints::ENV_VAR`.
- **Carry files are compared by identity.** A carry file is a `Segment` outside the segment table. Its `first` is `RecordNo::ZERO`, so `JournalReader::next_segment` never picks it. Two segments are the same file when their paths are equal (`Segment::same_file`). `Relocations::dropped` and `FramePos::relocated` compare that way, never by `first`. `Segment`'s `Debug` prints its file name.
- **Where the journal's figures come from.** The compactor drops and carries, not the actor. So `Metrics::journal_dropped` reads `Durability::removed()`, the segments removed from the table since open. `Metrics::journal_carried_bytes` reads `Durability::carried()`, the bytes written to carry files since open. `Metrics::journal_bytes` reads `Durability::len()`, the figure `JournalCompaction::len` returns: segments, carry files and unlinked files still held. `Journal::{len, carried}` move to `Durability`; their callers in `mod.rs` (`metrics`, the test at `:2325`) and in `journal/mod.rs`'s tests read them there. `ci/measure-trees.py`'s `journal_size` and `tests/support/trees.rs`'s `journal_len` and `journal_blocks` count `*.seg` and `carry-*` files; `segments` stays the `*.seg` files. A directory listing cannot see unlinked files still held; `journal_blocks` reads those through `/proc/self/fd` as today. `measure-trees.py`'s `compactions` reads `stages.checkpoints`.

**Signatures (committed in 5a80783 to a5ef18e; this slice changes the ones marked `// changed`, and the bodies are its work).** `crates/agentfs-core/src/tree_actor/journal/compaction.rs`:

```rust
pub(in crate::tree_actor) const CHECKPOINT: &str = "checkpoint";
pub(in crate::tree_actor) const CHECKPOINT_TMP: &str = "checkpoint.tmp";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::tree_actor) struct Checkpoint {
    pub salt: u32,
    pub built: RecordNo,
    pub pointers: BTreeMap<String, String>,
    pub objects: BTreeMap<String, BTreeMap<u64, u64>>,
    pub locals: BTreeMap<String, TreeLocals>,
    pub upstreams: BTreeMap<String, String>,
    pub opens: Vec<OpenRecord>,
    pub page_saved: Vec<(String, u64, u64)>,
    pub page_writes: Vec<RecordNo>,
    pub chunks: Vec<RecordNo>,
    pub step_heads: BTreeMap<String, RecordNo>,  // changed: pointer -> step; its oid is `pointers[pointer]`
    pub carried: Vec<RecordNo>,                  // written only by `Sealed::new`
}
impl Checkpoint { pub(in crate::tree_actor) fn next(&self) -> RecordNo; }

pub(in crate::tree_actor) struct Durable { through: RecordNo }                 // changed: not Copy; only wait_durable makes one
pub(in crate::tree_actor) struct Carried { files: Vec<RecordNo>, bytes: u64 }  // changed: private; only carry makes one
impl Carried { pub(in crate::tree_actor) fn bytes(&self) -> u64; }
pub(in crate::tree_actor) struct Sealed { checkpoint: Checkpoint }             // new: only Sealed::new makes one
impl Sealed {
    pub(in crate::tree_actor) fn new(checkpoint: Checkpoint, durable: Durable, carried: Carried) -> Result<Self, CompactionError>;
    pub(in crate::tree_actor) fn checkpoint(&self) -> &Checkpoint;
}
pub(in crate::tree_actor) struct Published { built: RecordNo, carried: Vec<RecordNo>, oldest_head: Option<RecordNo> } // publish, or Journal::open
impl Published {
    pub(in crate::tree_actor) fn built(&self) -> RecordNo;
    pub(in crate::tree_actor) fn oldest_head(&self) -> Option<RecordNo>;
    pub(in crate::tree_actor::journal) fn of_open(checkpoint: &Checkpoint) -> Self; // new: Journal::open's, after its directory fsync
}

#[derive(Debug, thiserror::Error)]
pub(in crate::tree_actor) enum CompactionError {
    NotPublished(std::io::Error),   // logged `tree CompactionFailed`, retried at the next built event
    NotDropped(std::io::Error),     // checkpoint stands; the segment stays for the next compaction
    Stopped(#[from] DurableError),  // a directory or segment fsync failed: the actor stops
}

impl JournalCompaction {
    pub(in crate::tree_actor) fn new(dir: PathBuf, durability: Arc<Durability>, gate: Option<Arc<dyn CompactGate>>) -> Self; // changed (re-cut)
    pub(in crate::tree_actor) fn gate(&self, gap: Gap) -> Result<(), CompactionError>; // new (re-cut): Ok at once with no gate; an Err classed by Gap::fault
    pub(in crate::tree_actor) fn gated(&self) -> bool;                                  // new (re-cut): the compactor then compacts at every built event
    pub(in crate::tree_actor) fn wait_durable(&self, through: RecordNo) -> Result<Durable, DurableError>;
    pub(in crate::tree_actor) fn fold_through(&self, durable: &Durable) -> Result<Folded, CompactionError>;
    pub(in crate::tree_actor) fn carry(&self, checkpoint: &Checkpoint) -> Result<Carried, CompactionError>;
    pub(in crate::tree_actor) fn publish(&self, sealed: &Sealed) -> Result<Published, CompactionError>; // changed
    pub(in crate::tree_actor) fn unlink_covered(&self, published: &Published) -> Result<DropStats, CompactionError>;
    pub(in crate::tree_actor) fn len(&self) -> u64;                                     // Durability::len()
}

// Re-cut 2026-11-22: Gap moves here from compactor.rs, beside the I/O it sits between.
// Gap, CompactGate and ChannelGate are public, re-exported from `tree_actor`.

/// A gap in one compaction, where the volume's gate parks the compactor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Gap { Covered, Written, Renamed, DirSynced, BeforeUnlink }
impl Gap {
    /// Every gap, in the order one compaction passes them.
    pub const ALL: [Gap; 5] = [Gap::Covered, Gap::Written, Gap::Renamed, Gap::DirSynced, Gap::BeforeUnlink];
    /// Its line on the FIFO gate: `covered`, `written`, `renamed`, `dir_synced`, `before_unlink`.
    pub fn name(self) -> &'static str;
    /// What an `Err` returned at this gap fails: the table under "Settled in the re-cut".
    pub(in crate::tree_actor) fn fault(self, error: std::io::Error) -> CompactionError;
}

/// Parks a volume's compactor at each gap of every compaction, and may fail
/// the step after a gap. Called on the compactor thread with no lock held.
pub trait CompactGate: Send + Sync {
    /// Returns when the compactor may go on past `gap`; `Err(e)` fails the
    /// compaction there, as `gap.fault(e)` says.
    fn at(&self, gap: Gap) -> std::io::Result<()>;
}

/// The in-process gate: each gap reached is sent on a channel, and the
/// compactor waits for the holder's verdict. A gap nobody receives, or
/// reached once the verdict sender is dropped, passes at once, so a pair
/// with both ends dropped is a gate that never parks.
pub struct ChannelGate {
    reached: std::sync::Mutex<std::sync::mpsc::Sender<Gap>>,
    verdicts: std::sync::Mutex<std::sync::mpsc::Receiver<std::io::Result<()>>>,
}
impl ChannelGate {
    /// The gate, the receiver of each gap it reaches, and the sender of verdicts.
    pub fn pair() -> (Arc<Self>, std::sync::mpsc::Receiver<Gap>, std::sync::mpsc::Sender<std::io::Result<()>>);
}
impl CompactGate for ChannelGate { fn at(&self, gap: Gap) -> std::io::Result<()>; }
```

`crates/agentfs-core/src/tree_actor/history/compactor.rs`:

```rust
#[derive(Clone)]
pub(in crate::tree_actor) struct AtBuilt {
    pub built: RecordNo,
    pub pointers: Arc<Pointers>,
    pub objects: BuiltObjects,
    pub trees: imbl::OrdMap<TreeId, Tree>,
    // changed: `rolled` deleted; `switched` deleted in the re-cut for "B" (the heads come from the fold alone)
}
/// The default floor of the due length: `Upkeep::default().compact_from`.
pub(in crate::tree_actor) const COMPACT_FROM: u64 = 64 << 20;                       // new (re-cut)
pub(in crate::tree_actor) fn compact_at(from: u64, len_after_last: u64) -> u64;     // changed (re-cut): max(from, 2 × len), moved from commit_point.rs
// `Gap` moved to journal/compaction.rs (re-cut).
pub(in crate::tree_actor) struct Compactor { thread: Option<JoinHandle<()>> }
impl Compactor {
    /// At each built event: with a gate (`journal.gated()`), compacts; without, compacts
    /// once `journal.len() >= compact_at(compact_from, len after the last compaction)`.
    pub(in crate::tree_actor) fn start(history: History, journal: JournalCompaction, compact_from: u64) -> std::io::Result<Self>; // changed (re-cut)
    pub(in crate::tree_actor) fn join(self);
}
/// The thread's one sequence, and the in-crate tests' `compact_now`. `None` before
/// History's first built event since open, or when B is not past the last published
/// checkpoint's `built` (changed in the re-cut). Runs the gate at Covered, DirSynced
/// and BeforeUnlink; `publish` runs it at Written and Renamed.
pub(in crate::tree_actor) fn compact_once(history: &History, journal: &JournalCompaction) -> Result<Option<DropStats>, CompactionError>;
```

`History` (`history/built.rs`), each O(1) or O(log pointers) under History's lock, never across I/O:

```rust
pub(in crate::tree_actor) fn at_built(&self) -> Option<AtBuilt>;                          // changed: None until the first built event
// `History::switched` is deleted (re-cut for "B"): the switch is a journal record.
pub(in crate::tree_actor) fn published(&self, published: &Published);  // replaces `checkpointed`; the compactor's alone; sets
                                                                        // `checkpointed_through` from `built` and `oldest_head`, moves the
                                                                        // generation and the epoch, notifies, and adds one to Stages::checkpoints
```

Re-cut 2026-11-22, the published event (`history/built.rs`; `Checkpointed` re-exported from `tree_actor` beside `History`):

```rust
/// A checkpoint History has counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checkpointed {
    /// Checkpoints this open's compactor has published: 0 for the one `Journal::open` read.
    pub generation: u64,
    /// Its built point B.
    pub built: RecordNo,
}
impl History {
    /// `opened` is the checkpoint `Journal::open` read: generation 0, and the
    /// redo start `checkpointed_through` reports until the first publish.
    pub(in crate::tree_actor) fn new(repo: TreeGit, built: RecordNo, pointers: Pointers, owners: Owners, objects: BuiltObjects, opened: &Published) -> std::io::Result<Self>; // changed
    /// The last checkpoint counted. O(1).
    pub fn checkpointed(&self) -> Checkpointed;
    /// Returns once a checkpoint of a generation after `after` is published, with it.
    ///
    /// # Errors
    ///
    /// [`WaitError::Stopped`] at shutdown, or once a compaction stopped the actor
    /// (a failed fsync, which `Durability` reports first); [`WaitError::Failed`] at a
    /// failed built event after the call, as `wait_built`. A compaction that failed
    /// and will be retried (`NotPublished`, `NotDropped`) does not end the wait.
    pub fn wait_checkpointed(&self, after: u64) -> Result<Checkpointed, WaitError>;
    /// A compaction ended in `Stopped`: every `wait_checkpointed` ends, and the compactor thread with it.
    pub(in crate::tree_actor) fn compaction_stopped(&self);
}
```

Re-cut 2026-11-22, the volume's upkeep (`tree_actor/mod.rs`, `tree_actor/call_path.rs`, `state_area.rs`; `Upkeep` re-exported from `tree_actor`):

```rust
/// How one open volume keeps its journal. Fixed for the life of the open;
/// nothing of it is stored in the state area.
#[derive(Clone)]
pub struct Upkeep {
    /// The next frame starts a new segment once the current one has a record
    /// and the frame would take it past this. Default `SEGMENT_BYTES` (64 MiB).
    pub segment_bytes: u64,
    /// The floor of compaction's due length, `compact_at`'s `from`. Default
    /// `COMPACT_FROM` (64 MiB). Unread when `gate` is set.
    pub compact_from: u64,
    /// Parks the compactor at each gap; with one, the compactor compacts at
    /// every built event. Default none.
    pub gate: Option<Arc<dyn CompactGate>>,
}
impl Default for Upkeep { fn default() -> Self; }
impl std::fmt::Debug for Upkeep { /* the gate as `Some(..)` or `None` */ }
impl Upkeep {
    /// One frame per segment (`segment_bytes: 1`), `compact_from` as default, and
    /// `gate`: what `AGENTFS_COMPACT_GATE` sets, and the tests' `compacting()`.
    pub fn gated(gate: Arc<dyn CompactGate>) -> Self;
}
impl TreeActor {
    // `create` and `open` are these with `Upkeep::default()`; one body each.
    pub fn create_with(root: &Path, identity: &VolumeIdentity, root_owner: Option<FileOwner>, service: &PodContext, upkeep: Upkeep) -> Result<Self>;
    pub fn open_with(root: &Path, service: &PodContext, upkeep: Upkeep) -> Result<Self>;
}
impl Recording {
    pub fn create_with(root: &Path, identity: &VolumeIdentity, root_owner: Option<FileOwner>, service: &PodContext, upkeep: Upkeep) -> Result<Arc<Self>, Error>;
    pub fn open_with(root: &Path, service: &PodContext, upkeep: Upkeep) -> Result<Arc<Self>, Error>;
}
impl TreeStateArea {
    /// The upkeep `create` and `open` pass on; `at` starts with `Upkeep::default()`.
    pub fn with_upkeep(self, upkeep: Upkeep) -> Self;
}
// journal/mod.rs: `Journal::create` and `Journal::open` each gain `segment_bytes: u64` as
// their last parameter; `roll` compares against it in place of `SEGMENT_BYTES`.
```

Re-cut 2026-11-22, the journal's figures and segment identity (`journal/durable.rs`, `journal/segment.rs`):

```rust
impl Durability {
    pub(in crate::tree_actor) fn len(&self) -> u64;      // JournalCompaction::len's figure; Metrics::journal_bytes. O(1)
    pub(in crate::tree_actor) fn removed(&self) -> u64;  // segments removed from the table since open; Metrics::journal_dropped. O(1)
    pub(in crate::tree_actor) fn carried(&self) -> u64;  // bytes written to carry files since open; Metrics::journal_carried_bytes. O(1)
}
impl Segment {
    /// The same file: paths equal. Carry files all have `first == RecordNo::ZERO`.
    pub(in crate::tree_actor) fn same_file(&self, other: &Segment) -> bool;
}
```

Re-cut 2026-11-22, telemetry and the binary's edge (`stages.rs`, `telemetry.rs`, `failpoints.rs`, `crates/agentfs/src/launcher/mod.rs`):

```rust
pub struct Stages { /* ... */ pub checkpoints: u64 }                // checkpoints published, process-wide
pub(in crate::tree_actor) fn checkpointed();                         // stages.rs: adds one; History::published's alone
pub const JOURNAL_CHECKPOINTS: &str = "agentfs.journal.checkpoints"; // telemetry::names; a u64 observable counter,
                                                                     // unit `{checkpoint}`, registered by `observe_latency`

pub const COMPACT_GATE_VAR: &str = "AGENTFS_COMPACT_GATE";           // failpoints.rs, every build (as ENV_VAR); read only by the binary
/// `AGENTFS_COMPACT_GATE=<dir>`'s gate: FIFOs `at` and `go` in `dir`, both opened
/// read and write so neither end blocks or sees end of file. At each gap it writes
/// `gap.name()` and a newline to `at`, then reads one line from `go`: `go` goes on,
/// `fail` returns an EIO error (`Gap::fault`), and an empty line or end of file
/// ends the gate, after which every gap passes without a line.
#[cfg(feature = "failpoints")]
pub struct FifoGate { at: std::sync::Mutex<File>, go: std::sync::Mutex<BufReader<File>>, ended: AtomicBool }
#[cfg(feature = "failpoints")]
impl FifoGate { pub fn open(dir: &Path) -> std::io::Result<Self>; }
#[cfg(feature = "failpoints")]
impl CompactGate for FifoGate { fn at(&self, gap: Gap) -> std::io::Result<()>; }

/// launcher/mod.rs. A failpoints build with `AGENTFS_COMPACT_GATE` set:
/// `Upkeep::gated` over `FifoGate::open` of that directory (one that cannot open
/// aborts with its error, as the take gate's does). Otherwise `Upkeep::default()`.
pub(crate) fn upkeep() -> Upkeep;
```

The switch record (`journal/mod.rs`), new in the re-cut for "B". It is in neither the committed stubs nor the wip branch: the slice adds it.

```rust
/// One pointer the actor switched at a built point: from the step that set
/// its value to the oid that step built.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Switched {
    pub name: String,
    pub step: RecordNo,
    pub oid: String,
}
impl Switched {
    /// As a pointer change: `old` is `Target::Step(step)`, `new` is `Target::Oid(oid)`.
    /// The one form the actor applies and the fold replays.
    pub(super) fn change(&self) -> PointerChange;
}
pub(super) enum Entry {
    // Checkpoint, Open, AnonymousOpen, Call, PageWrite, Bytes and Step as they are, and:
    /// The pointers named are their oids from this record on. Never empty.
    /// Not a step: no writer, no commit, no lag.
    Switch { pointers: Vec<Switched> },
}
```

Every `match` on `Entry` gains its arm in this slice: `fold` (replays it), `Replay::apply` and `BuildState::apply` (nothing), and `Live::names` (false, through its `_` arm).

The actor (`journal/commit_point.rs`) and the segment table (`journal/segment.rs`):

```rust
impl TreeActor {
    /// Appends one `Entry::Switch` for every pointer whose step History has built and
    /// whose built value its snapshot holds, then applies it. Returns the record, or
    /// `None` when there was nothing to switch, the append failed or the actor has
    /// stopped. Called in `record_with` before `History::appended_step`, and in
    /// `on_built`, which passes the record returned to `History::want_through`.
    pub(in crate::tree_actor) fn switch_built(&mut self) -> Option<RecordNo>; // changed (re-cut for "B")
    pub(in crate::tree_actor) fn relocate_if_moved(&mut self); // in on_built, under the lock, Hold::Compaction
}
impl Relocations { pub(in crate::tree_actor) fn generation(&self) -> u64; }
```

The guard, new in `stages.rs`, landed with this slice as the review asked:

```rust
/// Work that never runs while this thread holds the actor's lock, except in
/// a hold of a named exemption, each measured as its own `lock_hold` kind:
/// `Hold::Close` (a close decided or redone under the lock, a client death's
/// closes), `Hold::ForkCopy`, and `Hold::PageImage` (a page fill or writeback
/// building a file's image from every live open's calls).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) enum Unlocked { Fsync, JournalRead, Unlink }

/// Debug builds: panics when this thread holds the actor's lock in a hold
/// that is not a named exemption. Release builds: nothing.
pub(in crate::tree_actor) fn assert_unlocked(work: Unlocked);
```

It is called at `Segment::sync`, at every unlink of a segment or carry file, and at `CallReader::calls`. **Design rule 1, as this slice builds it:** under the lock, nothing fsyncs, unlinks, or reads an open's calls back from the journal, except in the holds named above. Reading a call's own bytes from a map on frames, such as a page fill's `size` bytes, is proportional to the call and is not guarded. Calls are read under the lock today in three places:
- `own_calls`, in a close redo and in pending closes (`recording/mod.rs:1473`, `:1488`), as `Hold::Close`.
- `TreeActor::close` deciding in its hold (`recording/mod.rs:967-975`, into `:332`), as `Hold::Close`.
- The page image (`pages.rs:475-493`, from `page_fill` `:355` and `page_writeback` `:385`), as `Hold::Other`.

`Hold::PageImage` is new: slice 2 adds it in `stages.rs`, `telemetry.rs`, `ci/trees-figures.py` and `docs/telemetry.md`. It also writes **one line in lane C's `recording/pages.rs`**: `let _kind = section(Hold::PageImage);` at the top of `pages_build_image`. Slice 5 rewrites that function's loop, and whichever lands second rebases. Moving the image build off the lock belongs to neither slice. It is a residual for after slice 5, when the image reads one copy of the calls.

**Deleted:** `commit_point::{compact, compact_if_due, compact_at, relocate_frames, resolve_compacted}`, `TreeActor::{compacted, compact_threshold, pages_saved_through, pages_held}`, `commit_point::holds`, `CompactError`, `Journal::{checkpoint, drop_behind}`, the checkpoint as a segment's first frame, compaction in `TreeActor::open` (`mod.rs:1034-1043`), `History::{checkpointed, compaction_overdue, rolled}`, `History::steps_built` if nothing else reads it, `LagGate::compaction_overdue` and its term, and `GateCause::Overdue` with its `overdue` stage, OpenTelemetry label and figures column. `Entry::Checkpoint` becomes `Entry::Checkpoint(Checkpoint)`, the frame kept in the checkpoint file. A segment holding one is `Error::Format`, as today. **Deleted in the re-cut for "B":** `History::switched`, History's `switched` map, `AtBuilt::switched`, the `let_go` test in `compactor::checkpoint_at`, the direct `Pointers::set` in `switch_built` (after this slice nothing calls `set` on the actor's pointers: they move only by `apply_targets` of an appended record's changes), the call of `switch_built` from `caught_up`, and the two `EXPERIMENT` edits of the wip branch.

**Recovery (C5, C7).** `Journal::open` reads `journal/checkpoint` first. The salt, the redo start `built + 1` and "no record after `built`" come from it. It ignores `checkpoint.tmp`. It reads the `carry-*` files the checkpoint names as carried frames. It skips records at or below `built` in segments that survived, except the live frames the checkpoint names. It reads each frame once: when a carry and its original both survive (after `NotDropped`), the carried copy wins. **A gap in record numbers after `built` (a missing segment), or a wholly torn segment, counts as the first invalid frame, and today's rules judge it (trees.md, the durable mark):**
- A later valid frame whose mark passes the last valid record means corruption.
- So do valid records that end before the built log's point.
- Anything else is a torn tail, saved to `journal/torn` and cut.

So the journal opens in C7's two power-loss cases, where nothing after B was durable. It refuses when a lost segment held a record that a later frame or the built log proves durable. One case is not caught, as today: a durable record that is not yet built, whose segment is lost with no later frame. A volume with no checkpoint file does not open. Before it returns, `Journal::open` fsyncs the journal directory and returns `Published::of_open` of its checkpoint as well. `start_history` passes that to `History::published` in place of `checkpointed(folded_built)` (`mod.rs:819`). So a checkpoint renamed but not dir-synced before a crash is durable before the builder trims `built.log` by it. `Journal::create(dir, first: &Checkpoint)` writes the checkpoint file, then the first segment, named `first.next()`. State format `agentfs-trees-13` becomes `agentfs-trees-14`, and 13 does not open (trees.md policy, no conversion).

**C7 with the switch recorded.** Every pointer value a record names was set by a record before it: a step's, or a switch's. So any prefix of the journal that a kill or a power cut can leave folds from any checkpoint that survives with it. That is the property the unrecorded switch broke. The compactor's `fold_through` is the same fold from the last published checkpoint through B, so it replays switch records too, and no longer fails with `NotPublished` on a record that names a switched oid.

**Contract.** C1 to C9 as written in `compaction.rs`'s module doc and on this page. Where that doc's step-heads row, C2a and C7 differ from this page, this page wins, and the slice rewrites the doc. The module doc's C4 line, and the `step_heads` row of the table on `Checkpoint` (C8), lose their "unless the actor switched it" clause: that row's one source is the fold.

**Acceptance criteria and mutation probes.** Each probe is planted, run and reverted before the slice lands. The slice report names the test that caught it.

| # | Criterion | Test that proves it | Mutation probe |
|---|---|---|---|
| 2.1 | A kill at any gap of a compaction loses nothing returned, and the volume opens with the same reads (C1 to C5) | `compactor.rs::tests::a_kill_at_each_gap_keeps_every_returned_call`: for each `Gap`, park the compactor through its volume's `ChannelGate`. While it is parked, append a step that sets a pointer and wait for its built event, so a builder batch runs and trims `built.log` at its start (`builder.rs:1493-1500`). While it is parked at `Covered`, `Written` or `Renamed`, `History::checkpointed()` and `checkpointed_through()` are still the last published checkpoint's (C3). Then copy the state area, release, open the copy and compare every read with the uninterrupted volume | **P2.1** call `History::published` before `publish`: the check at `Written` fails. (Before the re-cut for "B" the copy also failed to open, in `resolve_pointers`, on a pointer switched in memory. With the switch recorded the old checkpoint needs no trimmed value in this scenario, so the test checks C3 itself.) |
| 2.2 | A segment is unlinked only once the next segment starts at or before B + 1 (C2, C2a) | `compactor.rs::tests::an_append_during_the_coverage_gap_survives_a_kill`: on a volume whose `segment_bytes` is larger than its frames (so the call lands in the segment that starts at or before B; not `Upkeep::gated`), park at `Covered` while the actor's newest segment starts at or before B, append one call and return it, release, kill, reopen, read the call | **P2.2** judge a segment covered by its own first record: the returned call is gone after the reopen |
| 2.3 | The journal opens when nothing after B was durable (C7) | power-loss marks "covered segments unlinked, next segment's first frame not durable" and "no segment left" (2.6), and `compactor.rs::tests::a_checkpoint_with_no_segment_after_it_opens` | **P2.3** treat a missing segment after `built` as corruption: the "no segment left" case refuses to open |
| 2.4 | A record after B that names a step at or below B folds the same on the old and the new checkpoint (C3, C4) | `compactor.rs::tests::a_step_named_after_the_checkpoint_folds_on_both`: a pointer set at step s, built through B ≥ s, and compacted at B before the actor acts on that built point. In one run, one more step on the pointer follows, which names `Step(s)` as its `old`. In the other, `on_built` follows, so the first record after B is the switch record with `step: s`, then a step that names the oid. Reopen at each of the two checkpoints. Each run once with a branch pointer and once with a row chain | **P2.4** leave the head out of `step_heads` when the fold gives `Target::Step`: the reopen fails with `PointerMoved` or `Format` |
| 2.5 | No lock the actor takes is held across I/O or a wait (C9) | `compactor.rs::tests::calls_run_while_the_compactor_is_parked`: parked at each `Gap`, another thread appends a call, runs `durable_all` and catches up, each finishing (a 10-second bound fails the test rather than hanging it) | **P2.5** hold Durability's state lock across `unlink_covered`'s unlink: the append blocks at `BeforeUnlink` |
| 2.6 | Power loss at each step of a compaction, with records after B not yet durable, keeps the journal a prefix of what was appended and every record up to the last returned fsync | `ci/power-loss-trees.sh`: new marks at `Written`, `Renamed`, `DirSynced`, `BeforeUnlink` and after the unlink, plus the two C7 marks, driven through the compaction gate in a mount of their own (`AGENTFS_COMPACT_GATE` set), after the existing legs' mount has unmounted so their marks keep today's segment size; the driver marks the log device while the compactor is parked, then answers `go`. Each mark is replayed from an empty device and checked by `power-loss-trees-check.sh` | **P2.6** skip the directory fsync in `publish`: the mark after `Renamed` replays with no checkpoint (run locally under root with dm-log-writes, or on CI) |
| 2.7 | Phase-1 coverage of compaction across model steps and reopens | the conformance leg: `phase1-replay library` with `TreeActor::{create_with, open_with}(.., Upkeep::gated(ChannelGate))` and the mounted replay with `AGENTFS_COMPACT_GATE` set (`mounted.rs`), at the regression seeds, compacting at every built event and parking for one replayed call at each gap in turn while the replay goes on. Each leg ends its gate (drops the verdict sender, or writes an empty line to `go`) before it drops the volume or unmounts, so the shutdown's join of the compactor never waits on it | **P2.7** the same plant as P2.4: the leg reports the seed and call where the library left the model |
| 2.8 | New work is admitted while history trails past twice the due length (the overdue stop is gone) | `lag.rs` unit test `a_long_journal_holds_no_tree`, and `compactor.rs::tests::creates_go_on_while_history_trails` with the builder held at its batch start (`History::batch_read` hook) on a volume whose `Upkeep` has a small `compact_from` and `segment_bytes` and no gate, with the journal past `2 × compact_at(compact_from, 0)`: no 128 MiB fill | **P2.8** restore the overdue term in `LagGate::admits`: both tests fail |
| 2.9 | A call never waits for compaction (the purpose), and the walk left under the lock is measured | figures (warn only), local release build on ext4. `close_1g` and `write_1m` with `--metrics --probe --probe-view`: the other view's longest probe call under 50 ms (slice 0: 1,244 and 1,166 ms). A new `held_open` workload, shaped like the power-loss HC leg, so the compactor carries frames and `relocate_if_moved` runs: report `lock_hold.compaction`'s longest hold, and file a residual bead if it is over 10 ms | none: figures. Read in the report. |
| 2.10 | The guard holds | the whole workspace test run in debug, `tests/tree_pages.rs:291` `a_fill_reads_every_opens_calls` included: `assert_unlocked` panics on any fsync, journal read or unlink under the lock outside the named holds | **P2.10a** add a `Segment::sync` call inside `relocate_if_moved`: 2.14's test panics in the guard. **P2.10b** remove the `Hold::PageImage` section: the page tests panic |
| 2.11 | A head goes at the first checkpoint at or past its switch record, and the built log is trimmed past it (C3, C4) | `compactor.rs::tests::a_switched_head_is_let_go`: 2.4's second run up to its switch record at r > B. Then a build through r and a compaction at B2 ≥ r, with no step on the pointer since. `step_heads` lacks it, `oldest_head()` is `None`, `checkpointed_through()` is B2, and the next batch trims `built.log`. A reopen from B2 reads the pointer's oid from the checkpoint. Run once with a branch pointer and once with a row chain | **P2.11** return from `switch_built` before it collects anything: the head stays and `checkpointed_through()` stays below s |
| 2.12 | No checkpoint is published without History's trees at B (C8) | `compactor.rs::tests::a_compaction_after_reopen_keeps_dir_and_file_times`: directories and files with distinct times; reopen; one step; compact at its built event; reopen again. Every directory's number and time and every file's time equal the uninterrupted volume's | **P2.12** have `at_built` return `Some` with empty trees before the first built event, and compact once at start: the second reopen fails with `Format` (`restore_locals`, `mod.rs:528-535`) |
| 2.13 | A lost segment after B that held a record proven durable refuses to open (C7) | `journal/mod.rs` test `a_lost_durable_segment_refuses_to_open`, two cases. First: durable records after B, a roll, one more durable record, then the middle segment unlinked by hand. Second: the built log past the lost records, with no later segment. Both refuse to open | **P2.13** treat a missing segment after `built` as a torn tail without the two checks: both open |
| 2.14 | A carried open survives every gap, and the actor moves to the carry (C6) | `compactor.rs::tests::a_carried_open_survives_each_gap`: an open with a few calls is held while another file's saved calls outweigh them in its segment and the journal rolls. Build, then compact so that its frames are carried. For each `Gap`, park, copy, release, open the copy and close the open: every byte is read once. Repeat with the unlink failing (`NotDropped`: the gate returns `Err` at `BeforeUnlink`). After the next built event, `relocate_if_moved` has run, and once the builder has moved, `len()` no longer counts the unlinked segment | **P2.14** read a carried frame and its surviving original both: the copy after `NotDropped` saves the bytes twice or fails with `Format` |
| 2.15 | A volume reopens after a switch and before the next checkpoint, after a clean shutdown and after a kill | `compactor.rs::tests::a_volume_reopens_after_a_switch_before_the_next_checkpoint`, on a `never_due()` volume, so its only checkpoint is the one it was created with. Save a file at step s, `settle`, then `on_built`: the journal's last frame is an `Entry::Switch` that names `refs/heads/root`, s and the built head. Save the file again: this record names that oid as its `old`, and is the one the fourth block could not fold. Stop at each of two points, after the switch record and after the second save. At each, reopen after a drop (clean shutdown), and from a copy of the state area taken before the drop (kill). The copy is taken twice: with the builder held at its batch start (the `History::batch_read` hook, as in 2.8), so the copy's last record is not built, and after `settle`. Each reopen opens, every read and `head(ROOT)` equal the uninterrupted volume's, and the fold of its journal's frames gives exactly the actor's pointers (`same_pointers`, with no allowance). Run it again with the switch appended by a step in place of `on_built`: a rows-only step (`record_rows`) after the `settle`, whose `record_with` switches the branch. Run it a third time with nothing between the `settle` and the second save: that save's `record_with` sees the new built point while the branch is already at the save's own unbuilt step, and switches nothing for the branch. Last, a second `on_built` with no build between appends nothing | **P2.15a** apply the switch without appending its record: every reopen after the second save fails with `PointerMoved`. **P2.15b** drop the "s at or below the built point" test in `switch_built`: in the third run the second save's unbuilt step is switched to the first save's commit, and the reopened head and reads differ |
| 2.16 | The same for a row chain | `compactor.rs::tests::a_row_chain_reopens_after_its_switch`: a row step on a chain (`record_rows`), `settle`, `on_built`, so the switch record names the chain's pointer. Then a second row step on the chain, which names the chain's oid as its `old`. Clean shutdown and kill at the same two points as 2.15: the volume opens, and the chain's rows read back as the uninterrupted volume's. Then a build and a compaction past the switch record: `step_heads` holds no row chain | **P2.16** leave row chains out of `switch_built`: `step_heads` still holds the chain after that compaction |

**How the tests park the compactor (re-cut 2026-11-22; replaces "The test hook" and the process-wide gate).** There is no hook on History and no `hold_compactor_with`. The tests of 2.1, 2.2, 2.4, 2.5, 2.8, 2.11, 2.12, 2.14, 2.15 and 2.16 are unit tests in `history/compactor.rs`: they open a `TreeActor` with `TreeActor::create_with(.., upkeep)` and copy its state area, which integration tests cannot reach. Each test's `Upkeep` carries its own `ChannelGate`: the test receives each gap the compactor reaches and answers it with `Ok(())` or an error. Integration tests do the same through the public `Upkeep`, `ChannelGate` and `History::wait_checkpointed`. Tests that run in parallel in one binary never share a gate. A bound on a wait (`recv_timeout`) only fails a test that would hang; it never stands in for an event.

**The compaction gate in the binary, failpoints builds only.** `AGENTFS_COMPACT_GATE=<dir>`, read by `launcher::upkeep()` only, gives every tree volume the binary opens `Upkeep::gated(FifoGate::open(dir))`. The FIFOs `at` and `go` work the way `AGENTFS_TAKE_GATE`'s do (the protocol is on `FifoGate`). The e2e script, the conformance leg's mounted replay and the power-loss driver use it. It is event-driven and has no sleeps. `docs/configuration.md` gets its row under "Test builds only".

**Test helpers.** `tests/support/trees.rs` gains `new_actor_with(dir, upkeep)`, `reopen_with(dir, upkeep)`, `compacting()` (`Upkeep::gated` over a `ChannelGate` pair with both ends dropped: a compaction at every built event, never parked) and `compacted_through(v, through) -> Checkpointed`. That last one reads `history().checkpointed()` and, while its `built` is below `through`, calls `wait_checkpointed` past its generation. The in-crate tests' `tree_actor::test_support` gains the same `compacting()` and `compacted_through`, plus `never_due()` (`Upkeep { segment_bytes: 1, compact_from: u64::MAX, gate: None }`, so the thread never compacts) and `compact_now(&v)`. `compact_now` runs `settle`, then `compact_once(v.history(), &JournalCompaction::new(<journal dir>, Arc::clone(v.durability()), None))`, on a `never_due()` volume. The two copies of `compacting` and `compacted_through` sit on each side of the crate boundary, because integration tests cannot see `cfg(test)` items. Each is a few lines.

**Tests and scripts that assumed a compaction at open or at a call.** Each becomes the following. Nothing else in the workspace depends on open or a call compacting. Checked with `grep -rn -i compact` over `crates/*/tests`, `crates/agentfs-core/src`, `crates/agentfs-conformance`, `crates/agentfs-fuse/src`, `crates/agentfs/src` and `ci/`.

| Where (at b60dcc4) | What it assumed | What it becomes |
|---|---|---|
| `tests/tree_actor.rs:1255` `open_numbers_never_repeat_across_compaction` | every open compacts; the journal is then checkpoint frames in one segment | `new_actor_with(compacting())`; after the put, `compacted_through(&v, v.settle())`. No segment holds a `Checkpoint` frame, and none but the newest holds a record at or below `checkpointed().built`. Then the reopens, as now: open numbers still increase |
| `tests/tree_actor.rs:1283` `fill_journal` | the poke open after `settle` is the first call after catch-up, so it compacts | reads `history().checkpointed().generation` at its start; after the close and `settle`, `wait_checkpointed(generation)`. The poke goes. Sizes stay (default `Upkeep`, due at 64 MiB). Its users at `:1306`, `:1331` (first half) and `:1380` change no further |
| `tests/tree_actor.rs:1331` `a_held_open_pins_only_its_own_bytes`, second half (the poke at `:1366`) | the poke after `settle` compacts: "a checkpoint in a third segment, the two before it kept" | the poke becomes `wait_checkpointed` past the generation read before the 70 writes. `segments().len()` is 2, because the checkpoint is a file now; carried bytes stay 0 |
| `tests/tree_actor.rs:1380` `carried_frames_free_their_old_segments` | through `fill_journal` | unchanged beyond `fill_journal`. `journal_blocks` counts carry files, so the bound now covers the carried frames |
| `tests/tree_actor.rs:1532` `compact_worker`, `:1745` `a_kill_at_either_compact_failpoint_loses_nothing` | kills at the deleted failpoints, the first hit at open | **deleted**. Covered by 2.1 (the state area copied at each gap) and by the e2e script's SIGKILL at each gap |
| `tests/tree_actor.rs:1793` `a_fork_right_after_a_compaction_reopens_after_a_kill`, `:1808` `fork_after_compaction_worker` | "The first record after history caught up: it compacts, then records" | the worker reads the generation before its writes; after the close and `settle`, `wait_checkpointed(generation)`, then the fork. `tree.build_after_pack=kill@3` is unchanged, because a compaction is not a builder batch |
| `tests/tree_actor.rs:1889` `a_failure_before_the_drop_keeps_serving_and_the_next_compaction_drops` | `tree.compact_before_drop=error`; "Open compacts: the old segments go" (`:1904`) | `new_actor_with(Upkeep::gated(gate))` with a `ChannelGate` that answers `Err` at the first `BeforeUnlink` and `Ok` elsewhere; no fill, a put is enough. The checkpoint is published (`wait_checkpointed`), the covered segments stay, the actor serves, and the reads hold. Then `reopen_with(dir, compacting())`: the old segments are **still there**, because open does not compact. After one put, `compacted_through`, they are gone |
| `tests/tree_actor.rs:1912` `a_failure_before_the_checkpoint_is_retried_at_the_next_step` | `tree.compact_before_checkpoint=error@*`; the next step compacts | a `ChannelGate` that answers `Err` at `Covered` while armed (`NotPublished`; the test sees the `Covered` report as its event). Disarmed: one put, `settle`, then `wait_checkpointed` past the generation read at the start returns, and no record below its `built` survives outside the newest segment |
| `tests/tree_actor.rs:1932` `a_failed_compaction_fsync_stops_the_actor` | `tree.compact_before_fsync=error`, hit by the last open (`:1937`) | a `ChannelGate` that answers `Err` at `Renamed` (the directory's fsync, `Stopped`). `wait_checkpointed` returns `Err(WaitError::Stopped)`; then the next call fails and `is_stopped()` holds. The reopen half is unchanged |
| `tests/tree_dirs.rs:609` `directory_numbers_and_times_survive_compaction_and_reopen` | "A reopen with every step built compacts at once" (`:620`); its `segments().len() == 1` passes only because a small volume never rolls | `new_actor_with(compacting())` and `reopen_with(dir, compacting())`. Before each drop, `compacted_through(&v, v.settle())`. The segment count becomes `checkpointed().built` equal to the settled point. 2.12 stays the definitive test |
| `tests/tree_stages.rs:298-394` `each_kind_of_hold_is_recorded_when_its_work_runs` | "the first call after history built every step compacts" (`:373`); one `compaction` hold | the generation is read before the big writes; `holds_during` wraps the close, `settle` and `wait_checkpointed`. `compaction` is 0 and `Stages::checkpoints` grew. The doc comment says compaction runs off the lock and `compaction` counts only `relocate_if_moved`'s walk. The kinds vectors (`:107`, `:320`) gain `("page_image", 0)` |
| `tests/tree_stages.rs:433` | `after.overdue.count` | removed with `GateCause::Overdue` |
| `src/store.rs:548` `created_survives_a_change_and_a_compaction` | "Every step built: the reopen compacts to one checkpoint" (`:577`); one segment | `Recording::create_with(.., compacting())`; after the change, `compacted_through`. The reopen asserts the reads, and the checkpoint's `built` read before the drop is at or past the change's step |
| `src/tree_actor/mod.rs` tests `compaction_keeps_the_fold` (`:1817`), `a_compaction_keeps_a_closed_opens_page_write_out_of_recovery` (`:1894`), `reopen_with` (`:1918`), `restart_worker` (`:2103`), `a_restart_after_a_kill_reads_each_tree_once` (`:2212`), `restart_replays_from_the_checkpoint` (`:2247`) | `v.compact()`; the checkpoint as `frames[0]` | `test_support::compact_now(&v)` on a `never_due()` volume. The checkpoint is read from `journal/checkpoint`. `reopen_with` edits that file and writes it back, where it rebuilt the journal with `Journal::create`. `compaction_keeps_the_fold` compares the fold's pointers with the actor's exactly: `same_pointers` loses its allowance for a switched step. It no longer asserts that a record after the checkpoint fails to fold on the journal before it, because C4 makes the opposite the rule |
| `src/tree_actor/mod.rs:2090` `a_compaction_threshold_is_constants` | `compact_at(len)` | moves to `compactor.rs`'s tests as `compact_at(COMPACT_FROM, len)` with the same figures, plus one small `from` |
| `src/tree_actor/mod.rs:2295` `compaction_progresses_under_constant_writes_to_one_file` | "the next write compacts, though history is not built through it" | the generation is read before the writes; after `wait_built(step)`, `wait_checkpointed(generation)`. Then `journal_dropped >= 1` and the length fell. The log's later write is no longer the trigger |
| `src/tree_actor/journal/readback.rs:261` | `v.compact()` | `test_support::compact_now(&v)` on a `never_due()` volume |
| `journal/commit_point.rs` and `journal/mod.rs` tests of the checkpoint as a segment's first frame | the deleted functions | deleted, or rewritten for the checkpoint file (C5), as above |
| `ci/e2e-tree-recording-faults.sh:144-165` | "each opening of the state area passes them": `init` killed at `tree.compact_before_checkpoint` and `tree.compact_before_drop` | the opening loop keeps `tree.after_ref_rebuild` alone, and its comment says "the failpoint". A new loop follows it, below |
| `ci/power-loss-trees.sh:120-151`, the HC leg | "the next call compacts" (the `poke`); waits on `journal_dropped` | no poke. It reads `stages.checkpoints` before the big writes, then waits through the metrics file's renames until `stages.checkpoints` has grown and `journal_carried_bytes > 0`. The file is rewritten at each published checkpoint, an event. The leg keeps its default upkeep (no gate) |
| `ci/trees-figures.py` `nudge` (`:891`, `:1066-1081`, the docstring at `:104-110`) | the first call after catch-up runs the compaction then due | the fig-nudge save goes. When the workload's journal reached 64 MiB (its `journal_bytes` in the metrics file), it waits through the metrics file until `stages.checkpoints` passes its value at the workload's start, so the probe still covers the compaction. `GATE_CAUSES` loses `overdue`; `LOCK_KINDS` gains `page_image` |
| `ci/measure-trees.py:158` `journal_size`, `:518` `compactions` | only `*.seg`; `stages.compaction.count` | `*.seg` and `carry-*`; `stages.checkpoints` |
| Any test that asserts a literal record, open, object, fork or event number after a build, or that indexes the journal's frames after one (re-cut for "B") | every record is a call's | it reads the number from the call's result and skips `Entry::Switch` frames, or it holds the builder (`tree.build_before_batch=error@*`). A `grep` for literal fork and event numbers against a real engine found none: `tests/tree_actor.rs:974` forks before any build. The implementer changes what the test run shows, and lists it in the report |

**The e2e script's compaction kills.** They are driven in the script, which can make a volume due cheaply through the gate. After the opening loop, for each gap in `covered written renamed dir_synced before_unlink`:
1. Start from a fresh state area, `init --volume t`, and `mkfifo` `at` and `go` in a gate directory.
2. Run `start '' root` with `AGENTFS_COMPACT_GATE` set to that directory: one frame per segment, and a compaction at every built event.
3. Write `kept`, then open both FIFOs read and write. Read lines from `at` with a bound (`read -t`), answering `go` to each, until the line is the gap.
4. `kill -9` the mount while its compactor is parked there, wait for it to exit, and `fusermount3 -uz`.
5. `init --volume t` again: `kept` reads `saved`, `git fsck --full --strict` passes, `tree_git_no_duplicate_opens` holds, and a second `init` leaves `root` where it was.
6. Write `PASS compact:<gap>` to `results.tsv`.

`kept` was returned before the kill, so it must survive whichever compaction the kill met. This is the real-process half of 2.1, at about five mounts. The `phase-1.yml` step "kill at recording boundaries" runs it unchanged.

**Start from the fourth attempt's branch.** `wip/agentfs-ln6.9` at cf66512 holds the slice's code as far as it got. Its code is the diff `e2ad4dc..cf66512` under `crates/`; its one new errata entry is already carried over. It contains the second attempt's partial patch, so `slice2-partial.patch` is no longer applied. Written there: the journal (checkpoint file, carry files, recovery with the C7 rules), `JournalCompaction`, the compactor thread, `History` (the published event, `at_built`), `Upkeep` through `TreeActor`, `Recording` and `TreeStateArea`, the gate types, `FifoGate`, the telemetry counter and the in-crate tests. Not written: the acceptance tests 2.1 to 2.16 (`history/compactor/tests.rs` is a stub), the integration tests in the table, `launcher::upkeep()` and the binary's plumbing, the conformance legs, the CI scripts, the docs and 2.9's figures. **Remove the two `EXPERIMENT` edits**, which were the erratum's option A: in `journal/commit_point.rs` (`:23-25` at cf66512) the bound `.min(self.history.checkpointed().built)`, so the bound is History's built point again; and in `journal/fold.rs` (`:551-567`) the rewrite of a change's `old` from a head's oid to its step, so the fold checks every `old` strictly again. Then build the switch record as above, and delete what "What goes" lists. Everything else on the branch is the starting point, with the smaller decisions its erratum lists, which stand (see the re-cut responses below).

**Gates (durability changes):**
1. **The model-verification regression tier.** `cargo test --workspace` runs the model's scenarios, the regression seeds and conformance's replays, and `phase-1.yml` runs on the PR. Not the full sweep (owner, 2026-11-13), because no rule of the model changes: the model has the journal only as rule 12. git-model §3's compaction paragraph and §12's journal row are amended through `crates/agentfs-model/VERIFYING.md` "The procedure".
2. **The power-loss runs.** `ci/power-loss-trees.sh` with the marks of 2.6, in the `ci.yml` crash-injection job. Run them locally under root with dm-log-writes if the container allows it. Otherwise mark them NOT RUN locally and let CI judge. **The kills:** `ci/e2e-tree-recording-faults.sh` with its gap loop (`phase-1.yml`, "kill at recording boundaries"), run locally against a failpoints build (it needs `/dev/fuse`).
3. fmt, clippy `-D warnings`, and `cargo test --workspace`. CI `ci` and `image` green on the pushed SHA.

**Docs in the same commit:** `docs/trees.md` (the journal, compaction, the lag gate without the overdue stop, state format 14; the switch record and the step heads, in place of the paragraph "No record after a checkpoint names a step the checkpoint covers"), `docs/telemetry.md` (no `overdue` cause, the `page_image` hold, and what `lock_hold.compaction` now measures, and `agentfs.journal.checkpoints`), `docs/architecture.md` (the `commit_point.rs`, `compactor.rs`, `compaction.rs`, `journal/mod.rs`, `lag.rs`, `state_area.rs` and `failpoints.rs` rows), `docs/configuration.md` (`AGENTFS_COMPACT_GATE` under "Test builds only"; the three deleted failpoints have no rows to remove), `docs/library/core.md` (`Upkeep`, `ChannelGate` and the published event, under "Git-backed tree views"), `docs/runbooks/upgrading-the-sidecar.md` (format 14), and git-model §3 and §12 (§3's compaction paragraph names the switch record as a record that is no step).

**Costs this slice states in trees.md:**
- Restarts replay from B, so they take longer while history trails.
- With the overdue stop gone, only the lag gate bounds what follows B. Each tree's unbuilt steps are held to its lag and overlay limits (`lag.rs:186-191`), so that bound grows with the number of trees. A live open's calls are bounded by nothing but the open. Once the builder has failed, the gate admits everything, and the journal grows until the state area's disk fills and `ENOSPC` on the journal stops recording (trees.md, the untracked area).
- Each compaction re-reads the records from the last checkpoint through B, which the builder has already read. While it runs, its fold holds every live open's calls as `(seq, Call, FramePos)` (`fold.rs:516-521`). Under `write_1m` that is one more transient copy of a 1M-call open, on the compactor thread: the memory slice 5 exists to cut. Slice 5's 5.3 report says whether a compaction ran during its run (`journal_dropped` in `--metrics`). A fold that keeps the opens without their calls is a residual if it does.
- A long-lived open whose frames were carried pins the old segment until the actor's next built event.
- A failed compaction is reported at the next built event, not at a call.
- A reopened volume compacts at its first built event, not at open.
- Each built point at which a pointer is still at a step now built adds one switch record: about 100 bytes per pointer, no fsync, and under the lock a pass over the pointers and one append.
- When a mounted volume goes quiet, the builder runs one more batch for the last switch record: one journal fsync and one built-log entry, with no commit.
- Record, object, open, fork and event numbers depend on when the actor saw a built event. They stay unique and increasing.

**Review responses (slice 2), protocol review of ca2d098:**
- High, head leak (C3/C4): fixed. The switch is a journal record (re-cut for "B"), so a checkpoint at or past it folds the pointer as an oid and keeps no head. Probe in 2.11. Row chains stay in, for the reason given under C4.
- High, empty `locals` at open: fixed. `at_built` returns `None` before the first built event, open no longer compacts, and 2.12 tests it.
- High, guard against the page image: named exemption `Hold::PageImage`, measured, with its one line in `pages.rs` owned by slice 2. Design rule 1 restated and P2.10b added. Moving the build off the lock is a residual after slice 5.
- High, C7 refusals: kept. A gap is the first invalid frame under the durable-mark and built-log rules, and 2.13 tests it. What stays uncovered is stated.
- Medium, two sources for "rolled past": fixed. The segment table is the one source, and `History::rolled` is deleted.
- Medium, proof types: `Sealed` binds `Durable` and `Carried` to the checkpoint. The check runs when the seal is made, and the page says so.
- Medium, no carry tested: 2.14, P2.10a moved there, and `held_open` in 2.9.
- Medium, P2.1 could survive: 2.1 now runs a builder batch while parked.
- Medium, wrong bound: the costs now name the lag gate, the failed builder and `ENOSPC`.
- Low, refold cost: stated against slice 5's figure, and not fixed here, because a fold without calls is a second fold until a figure asks for it.
- Low, `Folded` lacked the old heads: the fold seeds `pointers` from `step_heads`, so `Folded` needs no new field.
- Low, three deciders: open's compaction is deleted. The gate is the one stated exception, for tests.
- Low, the brief described the CSI host channel: not this page, so no change.

**Re-cut responses (slice 2), second implementer block of 2026-11-22:**
- The e2e script killed `init` at compaction failpoints that only an opening compaction reached: the three compaction failpoints are deleted, and the script kills a gated mount at each gap. The script is now in lane A's paths.
- `tree_dirs.rs:609`, `store.rs:548` and the "first call after catch-up compacts" tests were not listed: every one is in the table above, with what it becomes.
- A process-wide gate would park every volume that in-process tests open in parallel: the gate is an `Upkeep` option of each open. Only the binary reads `AGENTFS_COMPACT_GATE`, at its edge. Tests wait on History's published event, not on `journal_dropped`.
- `ci/measure-trees.py` undercounted carry files: it counts them, and so do the tests' `journal_len` and `journal_blocks`.
- `Relocations::dropped` and `FramePos::relocated` compared by `first`, and every carry file has `first` 0: they compare by path (`Segment::same_file`).
- Error injection for `NotPublished`, `NotDropped` and `Stopped` used the deleted failpoints: a gate's `Err` at a gap, classed by `Gap::fault`.
- `compact_once` returned `None` when nothing was covered, so a gated small volume would never publish: it now returns `None` only when B has not moved past the last checkpoint.
- The partial patch is applied first.

**Re-cut responses (slice 2), fourth implementer block of 2026-11-22, for the owner's answer "B":**
- The switch left a journal that recovery could not fold: it is a journal record, `Entry::Switch`, which the fold replays. `step_heads` and the builder's trim stay, each for the reason given. `History::switched`, its map, `AtBuilt::switched`, C4's exception and 2.11's switch-record parts (its second half, P2.11a, P2.11b) are deleted.
- The wip branch called `switch_built` from `caught_up`, after a step had already woken the builder. With a record to append, that would let a batch start between a step and its switch record, and the n-th-batch kill tests would lose their batches. It is called in `record_with` before `appended_step`, and in `on_built`.
- A switch record that `on_built` appends has no step behind it, and the builder builds only what it is asked to. `drain` in `ci/trees-figures.py` and the power-loss driver wait for the built point to reach the last record, and would wait for ever. `on_built` asks with `History::want_through`.
- A new `Entry` variant needs an arm in `history/builder.rs`, which was lane C's. Lane C has landed, and that arm and one comment are now lane A's. The branch's deletion of `Pointers::to_targets` in `pointers.rs` is lane A's too.
- P2.1's stated failure rested on the unrecorded switch. 2.1 now checks C3 directly while parked, and P2.1 fails there.
- New acceptance tests 2.15 and 2.16: a volume reopens after a switch and before the next checkpoint, after a clean shutdown and after a kill, for a branch and for a row chain.
- The branch's smaller decisions stand, and where they differ from a signature above, they are the signature:
  - `Journal::open(dir, built)` stays under `#[cfg(test)]`, because three tests in `recording/mod.rs` call it. The serving path calls `Journal::open_with(dir, built, segment_bytes) -> (Journal, frames, Published)`. `Journal::create` takes no `segment_bytes`: it writes no frame and returns no journal.
  - `Published` and `Carried` each carry one more private list, `unlink`: the covered segments whose live frames have a copy. `Published` also carries `carried_bytes`. A covered segment whose live frames outweigh its dead bytes is kept, as today, and `unlink_covered` leaves it.
  - `Durability::segments()` returns the table and then the carry files, so the builder's `CallReader::of_segments` still finds a carried frame. `Durability::table()` is the table alone, for the coverage rule.
  - `Durability::len()` is O(segments + unlinked files still held), not O(1): each `Segment` keeps its own length.
  - `compaction_progresses_under_constant_writes_to_one_file` checks the linked files' bytes on disk, not `Durability::len()`: the map that held the unlinked segment is dropped on the deferred-drops thread, and no event says when.
  - `BuiltSnapshot::all_objects` and `Pointers::to_targets` lost their only caller and are deleted.
- Nothing was built or run for this re-cut: it is a reading of e2ad4dc and cf66512. Two claims are the implementer's to confirm first, and a miss on either is an erratum: that a settled step still has one batch (the n-th-batch kill tests), and that P2.1, P2.15a, P2.15b and P2.16 fail where the page says.

### Slice 6: a close returns once its change is in the journal

**Bead:** to be created, "latency slice 6: close stops waiting for the disk". **Lane A, after slice 2 has landed.** **Delivers:** the owner's 2026-11-21 answer "A" to Q2. close(2) no longer waits for an fsync. fsync, FSYNCDIR and O_SYNC/O_DSYNC writes still wait for the disk. O_SYNC is this design's reading of the owner's "fsync and FSYNCDIR", not his words.

**Every place that decides whether a close waits for the disk, settled one by one:**

| Place | Today | After |
|---|---|---|
| FLUSH, `tree_view.rs` `flush` | `session.durable_then(handle, …)` | `session.flush(handle)` answered at once |
| A client's death, `session.rs` `unbind` | `close_writer`, then `recording.durable_all()` | `close_writer` only. No program waits on it, and its records are durable at the next fsync, builder batch or shutdown. |
| `Drop for Recording`, `call_path.rs` | `make_all_durable` | unchanged: a clean shutdown makes every record durable before the lease goes |
| fsync, FSYNCDIR, O_SYNC/O_DSYNC write | wait for the disk | unchanged |

**Signatures (committed; bodies are this slice's work):**

```rust
// crates/agentfs-core/src/tree_actor/call_path.rs
impl Recording {
    /// FLUSH of write-open `open`: answers at once. Takes neither the actor's
    /// lock nor Durability's fsync; never returns a journal fsync's error.
    pub fn flush(&self, open: u64) -> Result<(), Error>;   // Err(Error::Stopped) once Durability::has_failed()
}
// crates/agentfs-core/src/session.rs
impl TreeSession {
    pub fn flush(&self, handle: u64) -> Result<()>;
    /// Every record so far durable, blocking the caller, never under the
    /// actor's lock: what a move out of the untracked area waits for (D2).
    pub fn durable_all(&self) -> Result<()>;
}
```

`tree_view.rs`'s `flush` answers `reply.ok()` on `Ok`, and `EIO` (from `errno`) on `Err`. Its doc comment states the new contract.

**D2: a change outside the journal that destroys data waits for the record that carries the data.** Both directions are in `tree_view.rs`'s rename and `tree_view/untracked.rs`:
- **Into the tree** (`Move::Import`, `Move::ImportLink`): after the step (`replace_file` or `symlink`) and `drop(v)`, call `self.session.durable_all()?`, then `self.untracked.unlink(&from)`. The wait runs on the view's thread, outside the actor's lock.
- **Out of the tree** (the move that `place`s an area copy, then `unlink`s the tracked name): `place` fsyncs the area file and its directory before the actor's `unlink` is recorded. Today it does neither.

**D1, the invariant.** A power cut loses a suffix of the journal. After it, the volume equals an uninterrupted run cut after some record r, and r is at or after the last record covered by a returned fsync, FSYNCDIR or O_SYNC write.

**Acceptance criteria and mutation probes:**

| # | Criterion | Test that proves it | Mutation probe |
|---|---|---|---|
| 6.1 | FLUSH never waits for an fsync | `call_path.rs::tests::a_close_answers_while_the_fsync_is_held`: `Durability::hold_fsync_with` blocks the fsync leader; `Recording::flush` returns, and `durable_all` from another thread is still waiting | **P6.1** make `flush` call `make_durable`: the test's flush blocks (10-second bound) |
| 6.2 | A failed journal fsync is never FLUSH's error before the actor stops, and it is `Stopped` (EIO) after | `call_path.rs::tests::flush_reports_only_a_stopped_actor`: a failed fsync, then `flush` gives `Error::Stopped`; before any failure, `flush` is `Ok` | **P6.2** return `Ok` after `has_failed()`: the second assertion fails |
| 6.3 | fsync, FSYNCDIR and an O_SYNC write still wait | `call_path.rs::tests::fsync_still_waits`: with the fsync held, `durable_then` has not answered until it is released. `ci/power-loss-trees.sh` marks F, SF and HC keep "loses nothing" | **P6.3** route `fsync` through `session.flush`: the power-loss F marks fail (CI), and the unit test fails locally |
| 6.4 | After a power cut, the journal is a prefix of what was appended and holds every record up to the last returned fsync, FSYNCDIR or O_SYNC write; the volume opens; recovery saves what survived (D1) | `ci/power-loss-trees-check.sh` rewritten to check exactly those four. The XW marks become may-lose. New legs: close A, rename A over B, close C, create D with no fsync; and the editor pattern, an untracked temp file, with and without fsync, renamed into the tree | **P6.4** append records out of order in the oracle's fixture (or skip the prefix check): the checker must fail on a planted non-prefix image |
| 6.5 | A rename from the untracked area into the tree never loses bytes a program fsynced (D2) | the editor leg of 6.4 with fsync: after any cut the bytes are in the area or in the tree. `tree_view.rs` unit test: the area file is still present when `durable_all` has not returned | **P6.5** unlink the area file before `durable_all`: the editor leg loses the bytes at the mark between them |
| 6.6 | `close_4k` is faster and the cost moved elsewhere is measured | figures (warn only): `close_4k` p50 and p99 before and after; fsync p99 while another open writes 1 GiB with no fsync; the append's lock hold (`lock_hold.other`) during that write, which shows whether writeback throttling stalls the write under the lock | none: figures. Read in the report. If throttling shows, it is a follow-up (RocksDB's `wal_bytes_per_sync`), not this slice. |

**Gates (durability changes):** the model-verification regression tier (`cargo test --workspace`, `phase-1.yml` on the PR) with git-model §3 rows 142 and 179 and §18 row 1058 amended through `VERIFYING.md` "The procedure". The power-loss runs (`ci/power-loss-trees.sh` with the legs of 6.4 and 6.5). fmt, clippy `-D warnings`, CI `ci` and `image` green.

**Docs in the same commit:** `docs/guarantees.md` (what a close promises and what a power cut can lose), `docs/trees.md` ("Recording and durability"), git-model §3 and §18, and `tree_view.rs`'s FLUSH comment.

**Costs this slice states in guarantees.md and trees.md:**
- A power cut can lose every change since the last fsync, FSYNCDIR or O_SYNC write, or since the last builder batch. That can be many saves.
- `sync(1)` and `syncfs(2)` send a FUSE mount no request, so they do not make closed saves durable.
- A failed journal fsync first shows as a later call's EIO, once the actor has stopped.
- One program's fsync pays for every view's unsynced bytes, because group commit fsyncs the whole journal.

### Slice 1: deferred drops leave the request threads

**Bead:** to be created, "latency slice 1: deferred drops off request threads". **Lane B.** **Delivers:** the maps and shapes a catch-up or a rebase lets go are dropped on a thread that serves no requests, not under the lock and not on a view's FUSE thread or the gate thread. Slice 0 measured the `drops` share of one hold at 60 ms (`write_1m`), 41 ms (`close_frag`) and 37 ms (`create`).

**Already in place (5a80783, e1eee36):** `SavedTree::rebuilt` returns a `Retired`. `TreeActor::rebuild` and `TreeActor::rebase_built` hand theirs to `History::defer_drops`, and shutdown calls `History::finish_drops` after the builder and the repacker have stopped and before the lease is released. `History::new` returns `io::Result` for the thread's start. So this slice changes `retired.rs` only.

**Signatures (committed; bodies are this slice's work), `history/retired.rs`:**

```rust
#[derive(Default)]
#[must_use]
pub(in crate::tree_actor) struct Retired { values: Vec<Box<dyn Send>> }
impl Retired {
    pub(in crate::tree_actor) fn push<T: Send + 'static>(&mut self, value: T);
    pub(in crate::tree_actor) fn append(&mut self, other: Retired);
    pub(in crate::tree_actor) fn is_empty(&self) -> bool;
}

pub(in crate::tree_actor) struct DeferredDrops { /* private: the queue and the thread */ }
impl DeferredDrops {
    pub(in crate::tree_actor) fn start() -> std::io::Result<Self>;  // spawns `tree-drops`
    pub(in crate::tree_actor) fn defer(&self, values: Retired);    // O(1), never waits for a drop
    pub(in crate::tree_actor) fn finish(&self);                    // every value deferred before it dropped, thread joined
}
impl Drop for DeferredDrops { fn drop(&mut self); }               // new: finish() if it has not run
```

**Contract:**
- `defer` takes the queue's lock for one push and a notify. It never drops a value and never waits on the drop thread.
- The thread takes the whole queue under the lock, releases the lock, then drops each value. No lock the call path takes is held across a drop. A base file's drop takes the open-bases cache's lock for its O(256) retain and unlinks outside it, as `BaseFile::drop` does today.
- `finish` and `Drop` mark the queue finishing, wake the thread and join it. The thread drops what is left and ends. A join error is logged as `tree DropsFailed` and never panics. Calling `finish` twice is a no-op.
- The drop thread holds no `Arc` of History's `Inner`, so no cycle keeps it alive.
- The `Hold::Drops` section leaves `defer`. After this slice the `drops` kind of `lock_hold` reads zero, because no drop runs under the lock.
- The same path serves `Recording` and a `TreeActor` used directly, the conformance library replay included. There is no second mode.

**Departure from the review's fix, with its cost:** the review asked for a histogram of the time drops take off the lock. It is not added, because the drop thread serves no request, so no caller feels that time. Adding it would grow the stage vocabulary across `stages.rs`, `telemetry.rs`, `ci/trees-figures.py` and `docs/telemetry.md`, which lane A owns. **Cost:** if drops ever fall behind, the memory queued for them is not measured. That is a `residual:` bead if a figure ever points at it.

**Acceptance criteria and mutation probes:**

| # | Criterion | Test that proves it (unit tests in `retired.rs`) | Mutation probe |
|---|---|---|---|
| 1.1 | A deferred value is dropped on `tree-drops`, never on the caller's thread | `a_deferred_value_drops_on_the_drop_thread`: a sentinel whose `Drop` sends `thread::current().name()` over a channel | **P1.1** drop inline in `defer`: the name is the test thread's |
| 1.2 | `defer` never waits for a drop | `defer_returns_while_a_drop_runs`: the first sentinel's `Drop` signals that it has started, then blocks on a channel; a second `defer` from the test thread returns while the first is still blocked; then the first is released | **P1.2** drop while holding the queue's lock: the second `defer` blocks (a 10-second bound fails the test rather than hanging it) |
| 1.3 | `finish` returns only after every value deferred before it is dropped, a base file's link included | `finish_waits_for_every_drop`: a blocked sentinel and a real `BaseFile` (`BaseFile::new` over a temporary file). `finish` runs on another thread and has not returned while the sentinel is blocked; after the release, `finish` returns and the base link is gone | **P1.3** return from `finish` without joining: `finish` returns while the sentinel is blocked |
| 1.4 | No drop under the lock or on a request thread in a real catch-up (the purpose) | figures (warn only), local release build on ext4, `trees-figures.py --only W --metrics --probe --probe-view FORK` for `close_frag`, `write_1m` and `create`: the `drops` share reads 0. Report the same-view and other-view probe latencies and the longest gate wait beside slice 0's | none: figures, read in the report |

**Finding to report, not to fix here:** the page's "close_frag longest hold under 10 ms" cannot follow from this slice. Slice 0 shows `close_frag`'s other kinds holding the lock 233 ms (compaction, gone after slice 2), 123 ms (take) and 55 ms (close). Slice 1's bar is the `drops` share, not the longest hold.

**Gates:** fmt, clippy `-D warnings`, `cargo test --workspace` (the regression tier runs within it), CI `ci` and `image` green. This slice changes no durability.

### Slice 5: one copy of each open's calls

**Bead:** agentfs-ln6.10. **Lane C.** **Delivers:** a close no longer copies an open's calls into new vectors. Today a 1M-call close copies them up to four times beyond the one copy each part keeps:
- In the actor: `CallReader::calls` builds one `Vec`, then `pages::close_calls` copies it into a second.
- In the builder: `open.calls`, its persistent copy, is collected into `own`, then `recorded_close_calls` copies that into a third.

After this slice each part keeps one copy: the actor the `Vec` it reads back from the journal, the builder its `BuildOpen.calls`. Every pass reads a lazy merge over them.

**Measure first.** Before changing code, run `ci/trees-figures.py --only write_1m --metrics` on the release build and record its peak over memory before (slice 0's figure: 774 MiB, against a bar of 256 MiB). If removing the copies does not move the figure by at least the copies' size (about 1M × `size_of::<(u64, Call, FramePos)>()` each), the report says so and names the next candidates: the large map's leaves left half full by a million appends, and sequential write pieces that are never merged. Both live in `recording/file_map.rs`, which no lane owns, so each becomes a `residual:` bead, not part of this slice.

**Signatures.** Committed in `recording/pages.rs` (e4beefd):

```rust
pub(in crate::tree_actor) enum CloseCall<'a, L: Located> { Own(&'a Call, &'a L), Piece(Call, L) }
impl<L: Located> CloseCall<'_, L> {
    pub(in crate::tree_actor) fn call(&self) -> &Call;
    pub(in crate::tree_actor) fn at(&self) -> &L;
}
#[derive(Clone)]
pub(in crate::tree_actor) struct CloseCalls<'a, L: Located + 'a, O>
where O: Iterator<Item = &'a (u64, Call, L)> + Clone
{ own: Peekable<O>, pieces: Peekable<std::vec::IntoIter<(u64, Run<L>)>> }
impl<'a, L: Located + 'a, O: Iterator<Item = &'a (u64, Call, L)> + Clone> Iterator for CloseCalls<'a, L, O> {
    type Item = CloseCall<'a, L>;
}
```

Changed by this slice. Each replaces the function of the same name, and every caller and test moves with it:

```rust
// recording/pages.rs
pub(in crate::tree_actor) fn close_calls<'a, L: Located + 'a, O>(own: O, pieces: Vec<(u64, Run<L>)>) -> CloseCalls<'a, L, O>
where O: Iterator<Item = &'a (u64, Call, L)> + Clone;
pub(in crate::tree_actor) fn calls_for_close<'a>(
    saved_len: u64, own: &'a [(u64, Call, FramePos)], writes: &[(u64, Run)], saved_through: u64,
) -> CloseCalls<'a, FramePos, std::slice::Iter<'a, (u64, Call, FramePos)>>;
pub(in crate::tree_actor) fn recorded_close_calls<'a, L: Located + 'a, O>(
    own: O, writes: &[(u64, Run<L>)], pieces: &[PagePiece],
) -> Option<CloseCalls<'a, L, O>>
where O: Iterator<Item = &'a (u64, Call, L)> + Clone;

// recording/layout.rs
impl Layout {
    pub fn of_close<'a, L: Located + 'a>(saved_len: u64, calls: impl IntoIterator<Item = CloseCall<'a, L>>) -> Result<Self>;
    pub fn replay<'a, L: Located + 'a, O>(saved_len: u64, own: O, writes: &[(u64, Run<L>)], pieces: &[PagePiece]) -> Result<Option<Self>>
    where O: ExactSizeIterator<Item = &'a (u64, Call, L)> + Clone;   // ExactSize: the skew failpoint drops the last own call
}

// recording/mod.rs (private)
fn replay_judge<'a, O>(saved: &FileMap, calls: CloseCalls<'a, FramePos, O>) -> Result<(SaveOutcome, Option<Decided>)>
where O: Iterator<Item = &'a (u64, Call, FramePos)> + Clone;   // clones the merge once, for Layout::of_close
```

In `history/builder.rs`, the save's replay passes `open.calls.iter()` to `Layout::replay` and builds `known` from the same iterator. The `own` vector is deleted. `history/verify.rs` follows `Layout::replay`'s new signature.

**Contract:** the merge yields exactly what the old `close_calls` vector held, in the same order. Own calls and page-write pieces are ordered by journal number, and the two never share one. No function on a close's path collects an open's calls into a new vector. The types now carry that, so a later caller cannot make a copy without writing `collect` by hand.

**Acceptance criteria and mutation probes:**

| # | Criterion | Test that proves it | Mutation probe |
|---|---|---|---|
| 5.1 | The lazy merge yields what the old vector did | `pages.rs` tests: `calls_for_close_table` and the seeded property test at `pages.rs` (today about line 869) unchanged in what they assert, plus `the_lazy_merge_matches_the_vector`: random own calls and pieces, the old merge kept in the test module as the oracle, compared call by call, and compared again after a `clone` taken midway | **P5.1** in `CloseCalls::next`, take the piece whenever both are present: the table test and the property test fail |
| 5.2 | The builder's replay and `agentfs verify` are unchanged | `cargo test --workspace`: the builder's replays, `tree_verify.rs`, the skew failpoint test, phase-1 conformance at the regression seeds | **P5.2** drop the last own call always (not only under `tree.build_replay_skew`): every save flags as unverified and the verify tests fail |
| 5.3 | `write_1m`'s peak falls by the copies removed | figure (warn only): peak-over-before, before and after, release build, local ext4, with the measured size of one copy | none: a figure. Report it beside 774 MiB and the 256 MiB bar. |

**Gates:** fmt, clippy `-D warnings`, `cargo test --workspace` (the regression tier runs within it), CI `ci` and `image` green. This slice changes no durability and no rule.

### What this cut leaves open

- **Slices 3 and 4** stay gated on a figure (above).
- **The view's single FUSE thread** held by a large close's own decision, 509 ms in `close_frag` and 970 ms in `write_1m`, is unchanged by every slice here. The question to the owner about it is asked separately and does not gate these slices.
- **The lag gate's waits** for writes to other files of a busy tree were not measured by slice 0, and nothing here changes them.
- **Docs bead:** after lanes A, B and C land, as listed under "Lanes and order".
