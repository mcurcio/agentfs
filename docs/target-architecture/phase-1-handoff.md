# Phase 1 handoff — 2026-11-06

**Implementation is paused at a reproducible checkpoint. Phase 1 is not
validated.** Real mounts exposed finding 32, shared-mapping behavior and Git
reference-store power-loss recovery gaps. The owner recorded current picks for
these areas in `921528b`; runtime, model and tests have not yet been brought up
to them. The observed failures remain available as historical evidence.

Work belongs to branch `docs/target-architecture` and [PR #23](https://github.com/mcurcio/agentfs/pull/23).
The owner merges. Product code at `1513b9784aebb1deabe30aae4b5df2f56bdf48f1`
is the implementation baseline for this handoff; the handoff commit also repairs
two test-harness defects described below. Nothing was pushed to main or
force-pushed.

Read the [ledger](rules-ledger.md), [git-model §18](git-model.md#18-phase-1-implementation-and-end-to-end-validation),
[phase-1 implementation page](phase-1.md), and
[VERIFYING.md, Fix passes](../../crates/agentfs-model/VERIFYING.md#fix-passes)
before continuing. The ledger governs outcomes. A decision must be reopened
when keeping it requires a workaround; it does not authorize an exception.

**Current decision authority:** [921528b](https://github.com/mcurcio/agentfs/commit/921528b8f426bbb19b2c5aa84628ffb6f9e9f331)
adds R1.4 (native shared mappings), R10.8 (whole-save loss for the unfinished
line case), extends R10.3 for finding 31, and records the synced journal as the
commit point for branch/queue pointers. Git refs become rebuildable copies.
These are approved current picks, subject to the ledger's blocking-consideration
rule. They supersede the earlier recommendations and the old §12
storage assumption. Implementation remains paused; no further owner answer is
needed before implementing these recorded choices.

## What is implemented

The core now connects recorded byte calls to line/name maps, the upstream referee,
whole-save landing, retained losers, durable queue entries, automatic sync,
P1/P2 progress, and snapshot-seeded recovery. Per-tree object bindings and replay
watermarks are separate from merge-file identities and move atomically with
branch/queue refs. The view host wakes queue work on events and releases its
actor mutex between takes. Wait counters measure delays without raising alarms.
This describes the existing implementation, before the approved journal-based
pointer commit point.

The FUSE adapter covers private writable descriptors, saved readers, anonymous
first link, orphaned handles and direct-I/O shared mappings. The latter now has
demonstrated failures with multiple opens. Git object/ref operations use the Git
executable because the current libgit2 cannot open reftable. Initialization,
configuration and loose-object directory synchronization have been repaired;
the reference store's in-flight crash guarantee remains unresolved.

| Original draft part | Disposition |
|---|---|
| `tree_actor` | Kept useful WAL/object/history primitives; replaced the incomplete reconciliation/recovery path and connected queues, referee and sync |
| `tree_view` | Repaired object and anonymous-inode behavior; added real mapping/close probes and an event-driven queue worker |
| `agentfs trees` | Kept the composition commands; added optional private measurement output |
| `ci/e2e-trees.sh` and other draft tests | Corrected isolation expectations to automatic flow, made close checks immediate, checked orphan events/history after release, and added independent byte/model/failure tests |
| 16 MiB reservation | Deleted: no ledger rule authorized per-file or cross-handle arbitration |

Transactions, `Client::Outside`, nested forks, Rules/profiles, replication, CSI,
several view hosts and upgrades remain excluded. Snapshot opens remain parked.
No extra exploratory model seeds or model fix passes were run. The new
`phase1-oracle` binary drives fixed existing model cases for mounted comparison;
it does not change model outcomes or checks.

## Exit-test snapshot

These are observed test results, not universal interleaving proofs. Every link
identifies its tested revision. The final handoff commit's routine CI is linked
from the PR description; it does not erase these failed exit tests.
The mapping assertions in these runs predate R1.4. Their original verdicts are
historical; update their expectations against the approved contract when work
resumes. The mixed descriptor/mapping byte-loss observations still require work.

| §18 exit | Status | Evidence and limit |
|---|---|---|
| Main + one fork: model scenarios through real calls | **FAIL** | [Mounted conformance at 1513b97](https://github.com/mcurcio/agentfs/actions/runs/42225254439/job/127916990566): loss, neighbors, concurrent inserts, unfinished tail, edited-place, R10.6, P1/P2 and byte cases passed; finding 32 failed. Fixed-case coverage is a subset of the conformance plan |
| Journal: kill boundaries and exactly-once recovery | **PASS** for the combined host/actor cases | [Ten WAL/durable-Git/checkout boundaries plus replaced-open-object recovery](https://github.com/mcurcio/agentfs/actions/runs/42225254439/job/127916990975) passed. The current composition does not independently kill a separate phase-1 history-service process |
| Journal: returned fsync/close survive power loss | **FAIL** | [ext4](https://github.com/mcurcio/agentfs/actions/runs/42225254439/job/127916990829) and [XFS](https://github.com/mcurcio/agentfs/actions/runs/42225254439/job/127916990908) fail recovery with a corrupt reftable at the first marked durability checkpoint |
| Full state area: write returns ENOSPC | **PASS** | [Allocation-forcing write and recovery](https://github.com/mcurcio/agentfs/actions/runs/42225254439/job/127916990975) passed. A supplementary full-state rename test returns EIO instead of its expected ENOSPC; that remains an implementation issue |
| Immediate close-to-open, same and another process | **PASS** for the exercised descriptor cases | [Tree-view job](https://github.com/mcurcio/agentfs/actions/runs/42225263487/job/127917021965): no polling barrier; includes dup/fork final-close and concurrent cases. Mapping close is covered separately and fails below |
| Byte calls and save alignment | **PASS** for the exercised saves | [Conformance byte cases](https://github.com/mcurcio/agentfs/actions/runs/42225254439/job/127916990566) compare separate-close split writes, append, overlap, holes, growth and shrink with Linux; saved maps are checked with an independent Git histogram diff. Complete shared scenario/seed-driver coverage remains due |
| Atomic replacement, unlink/rename while open, anonymous first link | **PASS** for the exercised cases | [Tree-view job](https://github.com/mcurcio/agentfs/actions/runs/42225263487/job/127917021965) and the journal orphan case check actual release and retained history |
| Old handle writes after a view-host restart | **NOT RUN** in the literal reconnect form | Recovery tests preserve object identity and reject orphan replay. The mounted crash test terminates the old client; it does not reconnect the same kernel handle and write after remount |
| Two stale-offset writers through page cache; shared mmap | **FAIL** | [Two-writer probe](https://github.com/mcurcio/agentfs/actions/runs/42225254439/job/127916990991): four of eight descriptor/map combinations pass; mapping privacy/publication and mixed-write byte results fail |
| Progress: unrelated stream, held log, new writers, cooperating programs, prefix sync | **PASS** for the finite scenarios | [P1/P2 mounted cases](https://github.com/mcurcio/agentfs/actions/runs/42225254439/job/127916990566) passed, including an upstream waiting for an appending child and a shell pipeline while a log take waits. Quantitative wait/starvation measurements remain unavailable |

The first integration run's `tee` pipeline masked the conformance program's
failure. `4d90329` added `pipefail`; subsequent phase-1 jobs correctly fail.
Use the individual case output when reading that earlier run.

## Measurements

Recorded, not gated: no figure here is a budget (git-model §18). Each report
(`ci/measure-trees.py`) is written when the run starts with status `RUNNING`,
replaced at every measurement, and carries a samples file (one JSON line per
100 saves, or per 30 seconds) and the mount's log; a run that is cut leaves what
it reached. The reports below are from commits `964618a` (run
[42323052176](https://github.com/mcurcio/agentfs/actions/runs/42323052176)) and
`12bd7a8` (run [42328771866](https://github.com/mcurcio/agentfs/actions/runs/42328771866),
calls only). The product code is that of `7feba71` in both; the commits between
changed only the harness. `FOPEN_DIRECT_IO`, the runner's ext4, Git 2.56.0,
Linux 7.0 (azure), debug build. Two runs of one mode on this runner differed by
up to 2 times (below), so read a figure as an observation.

| Measurement | Result | CI |
|---|---|---|
| Journal call cost against plain FUSE | 1,000 `pwrite` of 8 bytes at offset 0, one open descriptor. Plain FUSE (`bindfs -o direct_io`): median **27.4 us**, p99 **55.6 us**. Recording mount: median **407.5 us**, p99 **2,264.9 us**. Wall time around the syscall: it includes FUSE and the journal's `fsync`, not the journal alone | [calls job](https://github.com/mcurcio/agentfs/actions/runs/42328771866/job/128270258717), artifact `phase-1-measure-calls` (ID 13559071579) |
| One hour of journal growth | **502,485 bytes** over **3,600.000157 seconds**; 3,597 eight-byte `pwrite` calls, 28,776 payload bytes, a pause of one second after each. Logical delta of `calls.wal`, open and close records excluded. (The earlier figure, 415,628 bytes at `e8d12a6`, was before the journal records changed.) | [hour job](https://github.com/mcurcio/agentfs/actions/runs/42323052176/job/128251890571), artifact `phase-1-measure-hour` (ID 13555984904) |
| Line-map bytes per 10,000 saves | `metadata.json` is **195,676 bytes** after 10,000 one-line saves (about 19.6 bytes a save); the 10,000 snapshots are **960,893,676 bytes** logical and **241,234,350 bytes** as Git objects (10,000 distinct blobs); the 10,000 commits are 3,615,560 bytes logical and 2,256,009 on disk; the repository is 245,639,637 bytes of files, 425,697,280 allocated; `refs/` stays 164 bytes. The journal is 15,043,489 bytes after 10,001 saves (1,504 bytes a save). Elapsed **2,410.95 seconds** | [storage job](https://github.com/mcurcio/agentfs/actions/runs/42323052176/job/128251890644), artifact `phase-1-measure-storage` (ID 13555264336) |
| Save and take, 1 MiB | Write and close **24.3 ms**; actor save 275.5 ms; landing 581.4 ms, of which the sync back is 294.3 ms; both views and an empty queue after **2.03 s** | calls job above |
| Save and take, 100 MiB | Write and close **32.4 s**; actor save 25.7 s; landing 290.2 s; sync back 26.6 s; both views after **392.5 s**. (The run before, at `964618a`: 26.3 s, 16.7 s, 257.6 s, 11.2 s, 306.1 s) | calls job above; [earlier job](https://github.com/mcurcio/agentfs/actions/runs/42323052176/job/128251890610), artifact 13555421820 |
| Waits | One held log, one waiting fork save: **1** wait, **1,465 s** in total. The five independent saves became visible while it was held after **387 to 394 s** each, and five more opens of the held log took a median **283.4 s**, p99 **284.1 s**. Each was slow because the 100 MiB file was in the tree: every save and take rereads and rewrites it (below). The earlier run: 1 wait of 624 s, saves visible after 184 to 200 s | calls job above |
| Held log and compaction | A log held open while another file takes 24 saves of 8 MiB: the journal peaks at **66,149,105 bytes**, drops to 745 bytes at each of 2 compactions, and ends at 49,610,089 bytes. The held file's one line is all a compaction keeps. 24 saves took **159.7 s** | [held-log job](https://github.com/mcurcio/agentfs/actions/runs/42323052176/job/128251890523), artifact `phase-1-measure-held-log` (ID 13552934814) |

### Why 10,000 saves took over two hours: the pilot and its two causes

The 1,000-save pilot of the storage pattern (a file truncated and written with one
new line, closed each time, through the mount with a fork beside it) ran on
the commit that added the stage counters (`db557f5`,
[run 42302911952](https://github.com/mcurcio/agentfs/actions/runs/42302911952)).
It took **957.8 seconds**, and Git children per save rose from 92 in the first
hundred to 991 in the last. Per-stage totals, seconds: `event_owners` 865.3
(`queue_plan` 891.7 encloses it), `snapshot_read` 15.5, `checkout` 14.5,
`objects` 18.7, `ref_copy` 13.6, `align` 4.4, `journal_append` 1.8 (of which
`fsync` 1.5), `sync_merge` 0.7. Of 541,546 children, about 500,000 were the
`git cat-file` of one commit in a walk of the fork's whole history.

Each hypothesis, read against the pilot (P), the held-log run (H) and the final
calls and storage runs (C, S):

| # | Hypothesis | Verdict |
|---|---|---|
| 1 | `prior_decisions` walks first-parent history | **Not shown.** It ran 0 times in P, S and H (nothing lands on main in those patterns) and 15.5 s in the whole calls run (under 1 percent of it). Not changed |
| 2 | `event_owners` walks the DAG once per sync | **Confirmed, the main cause.** 865 of 958 s in P, and a child per commit: 92.0 children per save in the first hundred, 991.5 in the last. Fixed: one `git log` at open, then a map (design §17.3). After it, P took **92.5 s**, children per save were **40.9 at save 100 and 40.0 at save 900** (ratio 0.98), and S took 10,000 saves at a flat 40.0 |
| 2b | (found by H, not in the page) `undecided` asked Git `is_ancestor` once per line of the fork's copy | **Confirmed.** An 8 MiB file is 8,234 lines: 8,234 children and 70 to 130 s of each save's sync plan; 24 saves took **1,771.8 s**. Fixed: ask once per commit. After it, 24 saves took **159.7 s** and the run started about 1,100 children in all |
| 3 | `checkout_tree` rewrites every file on every save | **Not shown for a small tree.** 6.9 ms a run at save 100, 7.4 ms at 1,000, 9.9 ms at 9,900 with one file. Not isolated for many files, and with the 100 MiB file in the tree it is 50.3 s of the calls run. Not changed |
| 4 | `commit` writes a blob for every file | **Confirmed where it hurts, not fixed.** `objects` is 18.1 ms a save at 100 and 37.0 ms at 9,900 in S (it grows with the line map). In the calls run it is 755 s: the 100 MiB blob is hashed and synced on each later save. The fix the page allows (write only the changed files) is not made |
| 5 | `metadata.json` and `namespace.json` grow | **Confirmed, and it is the line-map measurement:** 19.6 bytes a save. It drives the remaining growth below. Never pruned |
| 6 | `progress_one` replans every entry, quadratic in a backlog | **Not shown.** The queue held at most one entry in P and S (`queue_plan` ran once per sync). The calls run's waiting entry is the only backlog and its cost is the 100 MiB file's |
| 7 | `call()` clones and replays the file on each write | **Not dominant.** A 100 MiB save is 26 to 32 s of write and close in C (of 306 to 393 s to converge). Tested only locally (Docker on macOS, debug build): 100 writes of 1 MiB took 27 s |
| 8 | `align` forks `git diff` in a temporary directory per save | **Confirmed as one child a save, and growing:** 2.1 ms a run at save 100, 64.7 ms at 9,900 (S), 321.6 s or 13 percent of the 10,000 saves |

What dominates the 10,000 saves now (S, 2,411 s, 40.0 children a save throughout,
77 ms a save at save 100 and 400 ms at save 9,900): `queue_plan` 1,194 s (50
percent: 218 ms a sync at the end), `snapshot_read` 338 s, `align` 322 s,
`objects` 278 s, `checkout` 173 s, `ref_copy` 158 s. These grow with the line
map and are not children any more. About **790 s (33 percent)** of the run is
inside `queue_plan` but in none of its measured parts (`snapshot_read`,
`sync_merge`): the copies of the fork's line maps, the comparison of two
snapshots and the checks of each line version. The frozen `Stages` has no field
for them, so the next measurement needs a new one.

With the 100 MiB file in the tree, every later save and take costs minutes:
`snapshot_read` is 2,899 s, `objects` 755 s and `judge` 244 s of the calls run
(`queue_plan` 1,984 s). `judge` and the sync plan grew faster than linearly with
the file's lines in a local run (Docker on macOS, debug): `judge` 28 s at 32 MiB,
109 s at 64 MiB, 316 s at 100 MiB. A memo of parsed snapshots keyed by commit,
writing only the files that changed, and the line referee's own cost are the
allowed fixes left; this slice used its two fix rounds on the causes above.

The first calls run, before the second fix (`36f07d2`,
[run 42308260086](https://github.com/mcurcio/agentfs/actions/runs/42308260086)),
reached its 110-minute step limit with its 100 MiB landing still queued: the
report it left (status `RUNNING`, 221 samples) is what the harness keeps for a
cut run. The mount publishes metrics between actor steps, so a long step looks
like a stalled file. Pilot and storage on that commit: **93.3 s** and
**2,092 s**, flat at 40 children a save.

## Approved continuation decisions

The owner recorded the following current picks in `921528b`, after the observed
runs. They remain subject to the ledger's blocking-consideration rule. This
handoff does not implement them or claim new passing results.

1. **Fork and upstream — queue; finding 32.** Start with six `-\n` lines.
   Hold a main write-open. Fork: `open(O_TRUNC)`, `write("y")`, `close`;
   then hold a fork write-open. Main: replace the second line with
   `m1.1\n` and close that writer. Fork: `pwrite("n6", 0)`, close;
   release the held main writer. The first fork save loses whole, but the
   next landing retains live lines `n6` and `m1.1\n`, rendering
   `n6m1.1\n`. This reproduces model finding 32 and blocks line integrity.
   **Recorded pick: R10.8.** This is a collision: the upstream wins and the
   later save loses whole, retained in history. Preserve line order under
   R10.7. The current model, runtime and mounted expectations need alignment
   with this approved rule.

2. **Programs sharing one tree — agentfs does not arbitrate.** Seed a page
   beginning `abcdefgh`. Open A and B writable with `MAP_SHARED`, and C as a
   separate read-only mapping. A stores `WXYZ` at offset 0; B stores `1234`
   at offset 2. Before close, C's mapping reads `WX1234gh` while `pread(C)`
   reads `abcdefgh`. Flush, close and unmap A while B remains open: a fresh
   descriptor can still read the original saved page.
   **Recorded pick: R1.4.** Mappings share pages as on Linux; R1.1 privacy
   applies to descriptors, not mappings. Each close saves what the kernel
   has flushed, possibly including another program's mapped stores. No bytes
   may be lost that Linux would keep. Shared visibility is now allowed;
   mixed-write data loss and publication behavior still need implementation
   and tests against the new contract.

3. **Fork and upstream — durable queue storage.** Initialize and mount both
   views, `open(main/f1, O_CREAT|O_WRONLY)`, write `acknowledged-1\n`, then
   `fsync` returns. Record an external receipt and dm-log-writes mark. Replaying
   the preceding logged durability boundary makes recovery fail with corrupt
   reftable data. Source inspection of Git 2.52 and 2.56 identifies footer,
   list-publication and obsolete-table cleanup ordering gaps; individual
   failures are not assigned to one mechanism without further proof.
   **Recorded pick: journal commit point.** One synced journal record commits
   each step's branch and queue pointer changes. Git refs are derived copies
   rebuilt after a crash. The owner set aside patching Git and adding SQLite
   for now. This supersedes §12's ref-transaction commit point and requires
   pointer replay plus journal compaction. Preserve R12's guarantee. The
   current runtime still uses Git as the commit point; this change is pending.

For the storage investigation, compare Git
[2.52 writer](https://github.com/git/git/blob/v2.52.0/reftable/writer.c#L769),
[2.56 writer](https://github.com/git/git/blob/v2.56.0/reftable/writer.c#L778),
[list publication](https://github.com/git/git/blob/v2.56.0/reftable/stack.c#L803)
and [reader cleanup](https://github.com/git/git/blob/v2.56.0/reftable/stack.c#L319).
Pinning 2.52 alone does not establish a fix.

## Review and continuation

One adversarial review covered the model/ledger exception inventory, vendor
backport, actor/recovery/referee/sync, FUSE boundary and CI oracles. The vendor
was compared with upstream fuser 0.18.0; its TMPFILE backport and provenance are
in `vendor/fuser/AGENTFS-PATCH.md`. The review found a power-loss receipt window,
stale documentation and missing explicit vendor-unit coverage. The bounded fix
added post-return replay marks plus earlier durability checkpoints, repaired
the documentation and invoked the vendor raw request test on CI. No unapproved
outcome rule, refusal, exception or weakened assertion was accepted. This was
static review with targeted verification, not exhaustive interleaving proof.

The handoff also corrects two test mechanisms: child direct saves must be found
through the full Git DAG after landing/fast-forward, and the lease test must
keep Git's stdin open so EOF cannot outrun the tested death signal. Neither
repair changes runtime outcomes or suppresses a failed invariant.

Final routine CI also exposed an existing RPC-test startup race: a Unix socket
pathname can exist before its listener accepts connections. The test helper now
waits for a connected channel within the same bounded startup period. Its later
shutdown/refusal assertion remains unchanged; no CSI runtime behavior changed.

Continue in this order:

1. Read the final handoff commit's CI/image results from PR #23. Known
   phase-1 failures are deliberate preserved evidence, not tolerated flakes.
2. Implement the current picks in `921528b`: R10.8, R1.4, the R10.3
   clarification, and the journal pointer commit point. Bring the model,
   runtime and conformance expectations up to those decisions. Keep the
   ledger authoritative; no further decision on these picks is pending.
3. Diagnose the supplementary full-state rename errno and complete missing
   crash/conformance coverage. Keep transactions and parked snapshot opens
   outside this work.
4. Make measurement reporting incremental, investigate the two long-running
   workloads, and collect the missing figures on the selected implementation.
5. Run all applicable §18 tests on Linux CI and update the implementation
   page and PR with exact source/run links before claiming validation.

FUSE and device replay run only on CI's Linux runners. Do not write FUSE
connection controls, restart Docker or run deadlock probes. Use the existing
build gate appropriate to the environment; keep build output outside source.
Do not merge, force-push, change unrelated branches, or revisit unrelated PRs.
