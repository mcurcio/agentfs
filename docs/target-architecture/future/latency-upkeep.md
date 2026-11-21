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

**Owned paths are disjoint.** Every lane builds into its own worktree's `target/`. No lane adds a dependency, so `Cargo.lock` is untouched. A lane that needs a file outside its list writes an erratum and stops.

| Lane | Owns (repo-relative) |
|---|---|
| A | `crates/agentfs-core/src/tree_actor/journal/`; `crates/agentfs-core/src/tree_actor/history/{compactor.rs,built.rs,lag.rs,built_log.rs}`; `crates/agentfs-core/src/tree_actor/{mod.rs,call_path.rs,stages.rs}`; `crates/agentfs-core/src/{session.rs,store.rs,telemetry.rs,failpoints.rs}`; `crates/agentfs-core/tests/{tree_actor.rs,tree_stages.rs,crash_points.rs}`; `crates/agentfs-fuse/src/tree_view.rs`, `crates/agentfs-fuse/src/tree_view/untracked.rs`; `crates/agentfs-conformance/`; `ci/power-loss-trees.sh`, `ci/power-loss-trees-check.sh`, `ci/trees-figures.py`; `docs/target-architecture/git-model.md`, `docs/trees.md`, `docs/guarantees.md`, `docs/telemetry.md`, `docs/architecture.md`, `docs/configuration.md`, `docs/runbooks/upgrading-the-sidecar.md` |
| B | `crates/agentfs-core/src/tree_actor/history/retired.rs` |
| C | `crates/agentfs-core/src/tree_actor/recording/{pages.rs,layout.rs,mod.rs}`; `crates/agentfs-core/src/tree_actor/history/{builder.rs,verify.rs}` |

**Interfaces lane A must keep, because lane C's files call them:** `Entry::Checkpoint` keeps its name (`builder.rs` matches `Entry::Checkpoint { .. }`, which also matches a tuple variant); `Durability::{segments, relocations, mark, appended}`; `CallReader::of_segments`; `Journal::call_reader`; `CallReader::calls`; `History::{checkpointed_through, publish_maps, next_work, failed_event, batch_read, repo}`; `BuildState::at`'s parameters; `FramePos::relocate`; `pages::relocate`; `FileMap::relocate`. **Lane C keeps** `pages::ObjectPages`' fields `writes` and `saved_through` and their visibility, which `journal/commit_point.rs` reads until slice 2 deletes those readers.

**Left to the milestone's docs bead, after every lane lands:** `docs/performance.md` (each slice's figures, from its report), `docs/architecture.md`'s `retired.rs` row and `docs/telemetry.md`'s "drops" kind after slice 1, the `README.md` if anything user-visible changed, and deleting this page.

### Slice 2: compaction off the lock (tracer bullet)

**Bead:** agentfs-ln6.9. **Lane A, first.** **Delivers:** another view's call never waits for compaction. The overdue stop is gone. Slice 0 measured compaction as the only cause of the longest stall in another view: 1,244 ms in `close_1g`, 1,166 ms in `write_1m`, 233 ms in `close_frag`.

**Settled here (parked above):**
- **C4: step heads are kept per pointer.** A checkpoint keeps, for each pointer whose latest step at or below B is a step, that step and the oid it built. The step comes from the fold (`Folded::pointers` gives `Target::Step(s)`). The oid is History's built value of the pointer at B, which is that step's head because no later step at or below B set the pointer. Heads of the previous checkpoint whose pointer no step in between set are carried over. A pointer the actor switched to an oid at or before record B + 1 has no head. So there is at most one head per pointer. The builder is unchanged: `BuildState::at` already takes `heads`, and it drops heads at or below `checkpointed_through()`, which never passes `Published::oldest_head() - 1`.
- **The actor switches its pointers at each built event**, under its lock (`TreeActor::switch_built`), and tells History before its next record (`History::switched(pointer, at)`). That replaces the switch compaction made in its own hold.
- **C6: holders move at the built event, as a named fallback.** The compactor copies live frames into `carry-<B>` files and records each copy in `Relocations`. A holder that still names an unlinked segment keeps reading it, because a segment is shared as `Arc<Segment>`, and those bytes count in `JournalCompaction::len` until the last holder lets go. The builder moves its own positions at its next batch, as today. The actor moves its positions in `TreeActor::relocate_if_moved`, which `on_built` runs only when `Relocations::generation()` moved. That walk is the one piece of compaction left under the lock, timed as `Hold::Compaction` (design rule 1: "named and measured"). It runs only when the compactor carried frames, which happens only when a live open's dead neighbours outweigh its own frames. No workload in slice 0 did that.
- **The compactor has its own thread**, started beside the builder, ended once History is stopping, and joined by `TreeActor`'s shutdown before the builder's join and before the lease is released. A publish in flight finishes or leaves only `checkpoint.tmp`.
- **The due rule has one home**, `history::compactor::compact_at`, read only by the compactor, which owns its threshold. `commit_point::compact_at` and `TreeActor::compact_threshold` are deleted.
- **B may fall between a take's chunk records and its step.** The fold's `bytes` (chunks with no step yet) become the checkpoint's `chunks`, and `carry` copies them.
- **The segment table and the journal's length have one owner: `Durability`**, which already holds `segments`, `add_segment` and `remove_segment`. Lock order: the actor, then History, then Durability, then Relocations. The `Journal` the actor holds keeps only its append position.
- **TreeActor::open still compacts before serving**, through `compact_once`, when History's built point covers a segment. That keeps conformance's reopen coverage of compaction.

**Signatures (committed; bodies are this slice's work).** `crates/agentfs-core/src/tree_actor/journal/compaction.rs`:

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
    pub step_heads: BTreeMap<String, (RecordNo, String)>,
    pub carried: Vec<RecordNo>,
}
impl Checkpoint { pub(in crate::tree_actor) fn next(&self) -> RecordNo; }

pub(in crate::tree_actor) struct Durable { through: RecordNo }          // only wait_durable makes one
pub(in crate::tree_actor) struct Published { built: RecordNo, carried: Vec<RecordNo>, oldest_head: Option<RecordNo> } // only publish makes one
impl Published {
    pub(in crate::tree_actor) fn built(&self) -> RecordNo;
    pub(in crate::tree_actor) fn oldest_head(&self) -> Option<RecordNo>;
}
pub(in crate::tree_actor) struct Carried { pub built: RecordNo, pub files: Vec<RecordNo>, pub bytes: u64 }

#[derive(Debug, thiserror::Error)]
pub(in crate::tree_actor) enum CompactionError {
    NotPublished(std::io::Error),   // logged `tree CompactionFailed`, retried at the next built event
    NotDropped(std::io::Error),     // checkpoint stands; the segment stays for the next compaction
    Stopped(#[from] DurableError),  // a directory or segment fsync failed: the actor stops
}

impl JournalCompaction {
    pub(in crate::tree_actor) fn new(dir: PathBuf, durability: Arc<Durability>) -> Self;
    pub(in crate::tree_actor) fn wait_durable(&self, through: RecordNo) -> Result<Durable, DurableError>;
    pub(in crate::tree_actor) fn fold_through(&self, built: &Durable) -> Result<Folded, CompactionError>;
    pub(in crate::tree_actor) fn carry(&self, checkpoint: &Checkpoint) -> Result<Carried, CompactionError>;
    pub(in crate::tree_actor) fn publish(&self, checkpoint: &Checkpoint, durable: &Durable, carried: &Carried) -> Result<Published, CompactionError>;
    pub(in crate::tree_actor) fn unlink_covered(&self, published: &Published) -> Result<DropStats, CompactionError>;
    pub(in crate::tree_actor) fn len(&self) -> u64;
}
```

`crates/agentfs-core/src/tree_actor/history/compactor.rs`:

```rust
#[derive(Clone)]
pub(in crate::tree_actor) struct AtBuilt {
    pub built: RecordNo,
    pub pointers: Arc<Pointers>,
    pub objects: BuiltObjects,
    pub trees: imbl::OrdMap<TreeId, Tree>,
    pub switched: imbl::OrdMap<String, RecordNo>,
    pub rolled: RecordNo,
}
pub(in crate::tree_actor) fn compact_at(len_after_last: u64) -> u64;   // max(64 MiB, 2 × len), moved from commit_point.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) enum Gap { Covered, Written, Renamed, DirSynced, BeforeUnlink }
pub(in crate::tree_actor) struct Compactor { thread: Option<JoinHandle<()>> }
impl Compactor {
    pub(in crate::tree_actor) fn start(history: History, journal: JournalCompaction) -> std::io::Result<Self>;
    pub(in crate::tree_actor) fn join(self);
}
pub(in crate::tree_actor) fn compact_once(history: &History, journal: &JournalCompaction) -> Result<Option<DropStats>, CompactionError>;
```

`History` (`history/built.rs`), each O(1) or O(log pointers) under History's lock, never across I/O:

```rust
pub(in crate::tree_actor) fn at_built(&self) -> AtBuilt;
pub(in crate::tree_actor) fn rolled(&self, first: RecordNo);
pub(in crate::tree_actor) fn switched(&self, pointer: &str, at: RecordNo);
pub(in crate::tree_actor) fn published(&self, published: &Published);   // replaces `checkpointed`
```

The actor (`journal/commit_point.rs`) and the segment table (`journal/segment.rs`):

```rust
impl TreeActor {
    pub(in crate::tree_actor) fn switch_built(&mut self);      // in on_built, under the lock
    pub(in crate::tree_actor) fn relocate_if_moved(&mut self); // in on_built, under the lock, Hold::Compaction
}
impl Relocations { pub(in crate::tree_actor) fn generation(&self) -> u64; }
```

The guard, new in `stages.rs` (design rule 1, landed with this slice as the review asked):

```rust
/// Work that never runs while this thread holds the actor's lock, except in
/// a hold of a named exemption: `Hold::Close` (a close redone under the
/// lock, a client death's closes) and `Hold::ForkCopy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) enum Unlocked { Fsync, JournalRead, Unlink }

/// Debug builds: panics when this thread holds the actor's lock in a hold
/// that is not a named exemption. Release builds: nothing.
pub(in crate::tree_actor) fn assert_unlocked(work: Unlocked);
```

Called at `Segment::sync`, at every unlink of a segment or carry file, and at `CallReader`'s frame reads.

**Deleted:** `commit_point::{compact, compact_if_due, compact_at, relocate_frames, resolve_compacted}`, `TreeActor::{compacted, compact_threshold, pages_saved_through, pages_held}`, `commit_point::holds`, `CompactError`, `Journal::{checkpoint, drop_behind}`, the checkpoint as a segment's first frame, `History::{checkpointed, compaction_overdue}` and `History::steps_built` if nothing else reads it, `LagGate::compaction_overdue` and its term, and `GateCause::Overdue` with its `overdue` stage, OpenTelemetry label and figures column. `Entry::Checkpoint` becomes `Entry::Checkpoint(Checkpoint)`, the frame kept in the checkpoint file. A segment holding one is `Error::Format`, as today.

**Recovery (C5, C7).** `Journal::open` reads `journal/checkpoint` first. The salt, the redo start `built + 1` and "no record after `built`" come from it. It ignores `checkpoint.tmp`. It reads the `carry-*` files the checkpoint names as carried frames. It skips records at or below `built` in segments that survived, except the live frames the checkpoint names. A missing or wholly torn segment after `built` is a torn tail, not corruption. A volume with no checkpoint file does not open. `Journal::create(dir, first: &Checkpoint)` writes the checkpoint file, then the first segment named `first.next()`. State format `agentfs-trees-13` becomes `agentfs-trees-14`, and 13 does not open (trees.md policy, no conversion).

**Contract.** C1 to C9 as written in `compaction.rs`'s module doc and on this page.

**Acceptance criteria and mutation probes.** Each probe is planted, run and reverted before the slice lands. The slice report names the test that caught it.

| # | Criterion (from "Proven by" and the corrections) | Test that proves it | Mutation probe |
|---|---|---|---|
| 2.1 | A kill at any gap of a compaction loses nothing returned and the volume opens with the same reads (C1 to C5) | `compactor.rs::tests::a_kill_at_each_gap_keeps_every_returned_call`: for each `Gap`, park the compactor (below), copy the state area as it stands, release, then open the copy and compare every read with the uninterrupted volume | **P2.1** call `History::published` before `JournalCompaction::publish`: the copy taken at `Written` fails to open or reads differently |
| 2.2 | A segment is unlinked only after the actor rolled past it (C2a) | `compactor.rs::tests::an_append_during_the_coverage_gap_survives_a_kill`: park at `Covered` while the actor's newest segment is covered by B, append one call and return it, release, kill, reopen, read the call | **P2.2** drop the `rolled` check in `unlink_covered`: the returned call is gone after the reopen |
| 2.3 | The journal always opens (C7) | power-loss marks "covered segments unlinked, next segment's first frame not durable" and "no segment left" (2.6), and `compactor.rs::tests::a_checkpoint_with_no_segment_after_it_opens` | **P2.3** treat a missing segment after `built` as corruption in `Journal::open`: the "no segment left" case refuses to open |
| 2.4 | A record after B that names a step at or below B folds the same on the old and the new checkpoint (C3, C4) | `compactor.rs::tests::a_step_named_after_the_checkpoint_folds_on_both`: a pointer set at step s, built through B ≥ s, compacted before the actor's built event switches it, one more step naming Step(s), reopen at each of the two checkpoints | **P2.4** leave the head out of `step_heads` once the fold gives `Target::Step` (only switched pointers kept): the reopen fails with `Format` |
| 2.5 | No lock the actor takes is held across I/O or a wait (C9) | `compactor.rs::tests::calls_run_while_the_compactor_is_parked`: parked at each `Gap`, another thread appends a call, runs `durable_all` and catches up, each finishing (a 10-second bound fails the test rather than hanging it) | **P2.5** hold Durability's state lock across `unlink_covered`'s unlink: the append blocks at `BeforeUnlink` |
| 2.6 | Power loss at each step of a compaction, with records after B not yet durable, keeps the journal a prefix of what was appended and every record up to the last returned fsync | `ci/power-loss-trees.sh`: new marks at `Written`, `Renamed`, `DirSynced`, `BeforeUnlink` and after the unlink, plus the two C7 marks, driven through the compaction gate; each replayed from an empty device and checked by `power-loss-trees-check.sh` | **P2.6** skip the directory fsync in `publish`: the mark after `Renamed` replays with no checkpoint (run locally under root with dm-log-writes, or on CI) |
| 2.7 | Phase-1 coverage of compaction across model steps | the conformance leg: `phase1-replay library` and the mounted replay at the regression seeds with `AGENTFS_COMPACT_GATE` set, compacting at every built event and parking at each gap in turn while the replay goes on | **P2.7** the same plant as P2.4: the leg reports the seed and call where the library left the model |
| 2.8 | New work is admitted while history trails past twice the due length (the overdue stop is gone) | `lag.rs` unit test `a_long_journal_holds_no_tree`, and `compactor.rs::tests::creates_go_on_while_history_trails` with the builder held at its batch start (`History::batch_read` hook) and the journal past `2 × compact_at(0)` | **P2.8** restore the overdue term in `LagGate::admits`: both tests fail |
| 2.9 | A call never waits for compaction (the purpose) | figures (warn only): `close_1g` and `write_1m` with `--metrics --probe --probe-view`, local release build on ext4: `lock_hold.compaction` longest under 1 ms and the other view's longest probe call under 50 ms (slice 0: 1,244 and 1,166 ms) | none: a figure. Read in the report. |
| 2.10 | The guard holds | the whole workspace test run in debug: `assert_unlocked` panics on any unexempted fsync, journal read or unlink under the lock | **P2.10** add a `Segment::sync` call inside `relocate_if_moved`: `compactor.rs`'s tests panic in the guard |

**The test hook, unit tests only.** The tests of 2.1 to 2.5 and 2.8 are unit tests in `history/compactor.rs` (they open a `TreeActor` and copy its state area, which integration tests cannot reach). They park the compactor through a hook on History, stored beside today's `batch_read` hook in `history/built.rs` and run at each `Gap` before the compactor's next step:

```rust
#[cfg(test)]
pub(in crate::tree_actor) fn hold_compactor_with(&self, hook: impl Fn(Gap) + Send + Sync + 'static);
```

**The compaction gate, failpoints builds only.** `AGENTFS_COMPACT_GATE=<dir>` holds FIFOs `at` and `go`, the way `AGENTFS_TAKE_GATE` does. With it set, the compactor compacts at every built event, ignoring `compact_at`. At each `Gap` it writes the gap's name to `at` and waits for a line on `go`. The test, the conformance leg and the power-loss driver each use this one mechanism, which is event-driven with no sleeps. `docs/configuration.md` gets its row.

**Gates (durability changes):**
1. **The model-verification regression tier.** `cargo test --workspace` runs the model's scenarios, the regression seeds and conformance's replays. `phase-1.yml` runs on the PR. Not the full sweep (owner, 2026-11-13): no rule of the model changes, since the journal is modelled only as rule 12. git-model §3's compaction paragraph and §12's journal row are amended through `crates/agentfs-model/VERIFYING.md` "The procedure".
2. **The power-loss runs.** `ci/power-loss-trees.sh` with the marks of 2.6, in the `ci.yml` crash-injection job. Run them locally under root with dm-log-writes before pushing if the container allows it, otherwise mark them NOT RUN locally and let CI judge.
3. fmt, clippy `-D warnings`, `cargo test --workspace`. CI `ci` and `image` green on the pushed SHA.

**Docs in the same commit:** `docs/trees.md` (the journal, compaction, the lag gate without the overdue stop, state format 14), `docs/telemetry.md` (no `overdue` cause; what `lock_hold.compaction` now measures), `docs/architecture.md` (the `commit_point.rs`, `compactor.rs`, `compaction.rs`, `journal/mod.rs` and `lag.rs` rows), `docs/configuration.md` (`AGENTFS_COMPACT_GATE`), `docs/runbooks/upgrading-the-sidecar.md` (format 14), git-model §3 and §12.

**Costs this slice states in trees.md:** restarts replay from B, so they take longer while history trails. Nothing bounds the journal's length except the tree's space limit (C6): the overdue stop used to bound it, and nothing replaces it. A long-lived open whose frames were carried pins the old segment until the actor's next built event. A failed compaction is reported at the next built event, not at a call.

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
