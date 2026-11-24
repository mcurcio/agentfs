# Latency: upkeep still under the volume lock

**Proposed, not built, and gated.** Each volume has one lock, and every recorded call and every lookup in every view of the volume waits while it is held. This page proposes two slices that move more upkeep out from under that lock. Upkeep is work that keeps the engine tidy and belongs to no call. Neither slice is committed: each waits for a figure that shows it matters. The commit that lands the last of them deletes this page and moves the behaviour into the library and operator docs.

**Layer: System** (Domain · **System** · Service · Module · Interfaces · Implementation). The domain is unchanged: trees, the journal, history, takes and the model's rules stay as they are.

## What is built

The rest of the design this page once held is built, and described where it lives:

| Built | Described in |
|---|---|
| The journal compacts on history's own thread, the compactor, and no call waits for it. A pointer changes only by an appended record | [trees](../../trees.md#the-journal) |
| What a catch-up with history replaces is freed on the volume's `tree-drops` thread | [trees](../../trees.md#history-trails-the-files) |
| A close returns once its calls are in the journal. `fsync`, FSYNCDIR and `O_SYNC` writes wait for the disk | [trees](../../trees.md#recording-and-durability), [guarantees](../../guarantees.md#tree-views) |
| The lock's holds by kind of work, the waits to take it, the lag gate's waits and each view's request times | [telemetry](../../telemetry.md#the-volume-lock) |
| The figures before and after | [performance](../../performance.md#tree-views-release-build) |

## Requirements and background

**R1 (requirement, 2026-11-20, asked as absolute, answer "A").** When history's rebuild of a saved file disagrees, what the workload saved wins.

**Background (the owner's guidance, revisable; OWNER-GUIDANCE.md, Performance):**

- "My biggest concern right now is latency." (2026-11-21). Asked which wait he meant: "Interruption to the host program due to the held locks which prevent it from opening/reading/writing new files" (2026-11-21).
- Asked how much of the design to commit, with these two slices left only if measurement shows they matter: "Agreed" (2026-11-21).
- A new bar warns and never fails CI: "No" to failing (2026-11-20). "Im just looking to cut waste" (2026-11-20).
- "we absolutely must avoid deadlocking" (2026-11-05). "It's all event driven" (2026-10-24).

**Design rules (design choices, not requirements):**

1. **Under the lock, work is proportional to the call.** Nothing under the lock fsyncs, reads the journal, unlinks a file, walks the volume, or does the last drop of a large value. A rare fallback that still does is named and measured.
2. **Each fact has one owner.**
3. **Nothing reorders what the journal promises.** Closes keep their order within each tree, and recovery folds every record the same whichever checkpoint survives a crash.

## What stays under the lock today

| Under the lock | Why it is there |
|---|---|
| Decide the step, check the pointers, append one frame, apply it | Principle 1's commit point. Record order is the model's order |
| Catching up with history at a step or a built event: the walk of the tree's overlay | Slice 3, gated |
| A close's land, and its redo when the saved step moved. Pending closes, and a dead client's closes | Slice 4, gated, for a dead client's closes; the rest has no slice |
| A fork's step, its untracked area's copy included | Slice 4, gated |
| A take's inputs and its land | No slice |
| The actor's walk onto the frames a compaction carried | No slice; see "Also open" |
| A page fill or writeback that builds a file's image from every live open's calls | No slice; see "Also open" |

The lag and overlay gates and every wait for the disk are outside the lock.

## The gated slices

| # | Slice | What it changes | Proven by | The figure that would commit it |
|---|---|---|---|---|
| 3 | The catch-up's overlay walk | The overlay becomes layers, each frozen at a record and dropped whole once history has built it, never before history's check of that save (R1). A catch-up then lets a layer go where it walks the overlay today | `create`'s slowest call flat across tree sizes; reads identical before and after | The catch-up's hold, or `create`'s slowest call, growing with the tree's size |
| 4 | A dead client's closes, and the fork copy | A dead client's closes are saved one handle at a time, on the unbind thread. The fork copy is made durable before its record | Another view's wait for the lock, at p99, while a client with an open of a million calls dies | A workload that kills a client, or forks a tree with a large untracked area, and shows another view waiting |

Neither changes a rule of the model. A slice that touches the order closes save in goes through the model-verification procedure first.

## What the figures say so far

One local run of `create` (50,000 one-line files in one directory, with a fork's view mounted; release build, ext4, one 4-CPU container, commit 26ddacf; [performance](../../performance.md#tree-views-release-build) has the method):

| Files in the tree | Median save, ms | Slowest save in the window, ms |
|---|---|---|
| 100 | 0.48 | 0.75 |
| 200 | 0.38 | 18.6 |
| 500 | 0.43 | 1.8 |
| 1,000 | 0.52 | 10.8 |
| 2,000 | 0.45 | 4.5 |
| 5,000 | 0.39 | 10.3 |
| 10,000 | 0.16 | 32.9 |
| 20,000 | 0.42 | 50.5 |
| 50,000 | 0.50 | 7,139 (a wait at the lag gate) |

- **Slice 3.** The catch-up's longest share of one hold was 178 ms, and its holds came to 7.1 s in all through the workload's 73 s and its build's 155 s. The slowest save rises with the tree from 5,000 files on. A slowest save includes any wait at the lag gate, and this is one run on one machine, so it shows where to look, not that the slice is due.
- **Slice 4.** No workload kills a client or copies a fork's untracked area, so nothing has been measured.

## Also open

Found while the built slices were measured. None has a slice.

- **The walk onto carried frames.** It held the lock 114 ms in `close_frag`, whose open has 200,000 pieces. No figures workload holds a file open across a compaction to measure it on purpose.
- **The page image.** A page fill or writeback builds a file's image under the lock from every live open's calls. No workload measured it.
- **The lag gate's waits for other files of a busy tree.** A write to another file of a tree whose history trails waits at the gate. The probe never writes, so no figure shows it.
- **`write_1m`'s memory.** A million small appends in one open peak 885 MiB over the view's memory before. Sequential writes are not merged into one piece, and a million appends leave the large map's leaves about half full. Neither has been measured apart from the other.

**Set aside:**

- **Upkeep cut into short slices under the lock.** The total time under the lock stays the same.
- **One lock per tree or per file.** Steps still meet at the single journal append, and ordered multi-lock steps are a deadlock risk (2026-11-05). Revisit only if the wait for the lock stays over 1 ms at p99 with 8 busy views. Read snapshots for lookups would be the next step after that.

## New terms

| Term | Meaning | Why needed | Existing term it could be |
|---|---|---|---|
| upkeep | work that keeps the engine tidy and belongs to no call | names what moves | — |
| frozen layer | an overlay layer closed at a record, dropped whole once built and checked | slice 3 only | the overlay |

## For the code architects (parked)

- Frozen layers: a bound on how many there are, and how `readdir` merges across them.
- The fork copy: it must not reverse the lock order between the actor and the untracked area, and must not split the copy from its record.
