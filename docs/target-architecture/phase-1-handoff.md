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

The core now connects recorded byte calls to line/name maps, the parent referee,
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
| Main + one sandbox: model scenarios through real calls | **FAIL** | [Mounted conformance at 1513b97](https://github.com/mcurcio/agentfs/actions/runs/42225254439/job/127916990566): loss, neighbors, concurrent inserts, unfinished tail, edited-place, R10.6, P1/P2 and byte cases passed; finding 32 failed. Fixed-case coverage is a subset of the conformance plan |
| Journal: kill boundaries and exactly-once recovery | **PASS** for the combined host/actor cases | [Ten WAL/durable-Git/checkout boundaries plus replaced-open-object recovery](https://github.com/mcurcio/agentfs/actions/runs/42225254439/job/127916990975) passed. The current composition does not independently kill a separate phase-1 history-service process |
| Journal: returned fsync/close survive power loss | **FAIL** | [ext4](https://github.com/mcurcio/agentfs/actions/runs/42225254439/job/127916990829) and [XFS](https://github.com/mcurcio/agentfs/actions/runs/42225254439/job/127916990908) fail recovery with a corrupt reftable at the first marked durability checkpoint |
| Full state area: write returns ENOSPC | **PASS** | [Allocation-forcing write and recovery](https://github.com/mcurcio/agentfs/actions/runs/42225254439/job/127916990975) passed. A supplementary full-state rename test returns EIO instead of its expected ENOSPC; that remains an implementation issue |
| Immediate close-to-open, same and another process | **PASS** for the exercised descriptor cases | [Tree-view job](https://github.com/mcurcio/agentfs/actions/runs/42225263487/job/127917021965): no polling barrier; includes dup/fork final-close and concurrent cases. Mapping close is covered separately and fails below |
| Byte calls and save alignment | **PASS** for the exercised saves | [Conformance byte cases](https://github.com/mcurcio/agentfs/actions/runs/42225254439/job/127916990566) compare separate-close split writes, append, overlap, holes, growth and shrink with Linux; saved maps are checked with an independent Git histogram diff. Complete shared scenario/seed-driver coverage remains due |
| Atomic replacement, unlink/rename while open, anonymous first link | **PASS** for the exercised cases | [Tree-view job](https://github.com/mcurcio/agentfs/actions/runs/42225263487/job/127917021965) and the journal orphan case check actual release and retained history |
| Old handle writes after a view-host restart | **NOT RUN** in the literal reconnect form | Recovery tests preserve object identity and reject orphan replay. The mounted crash test terminates the old client; it does not reconnect the same kernel handle and write after remount |
| Two stale-offset writers through page cache; shared mmap | **FAIL** | [Two-writer probe](https://github.com/mcurcio/agentfs/actions/runs/42225254439/job/127916990991): four of eight descriptor/map combinations pass; mapping privacy/publication and mixed-write byte results fail |
| Progress: unrelated stream, held log, new writers, cooperating programs, prefix sync | **PASS** for the finite scenarios | [P1/P2 mounted cases](https://github.com/mcurcio/agentfs/actions/runs/42225254439/job/127916990566) passed, including a parent waiting for an appending child and a shell pipeline while a log take waits. Quantitative wait/starvation measurements remain unavailable |

The first integration run's `tee` pipeline masked the conformance program's
failure. `4d90329` added `pipefail`; subsequent phase-1 jobs correctly fail.
Use the individual case output when reading that earlier run.

## Measurements

Only the hour result completed. It is an observation of `e8d12a6`, before the
later Git durability corrections, with `FOPEN_DIRECT_IO` on the runner's ext4
filesystem. The artifact records the binary hash, Git/kernel versions, mount
options and clean source state. These figures are not budgets.

| Measurement | Result | CI |
|---|---|---|
| One-hour WAL growth | **415,628 bytes** over **3,600.000128498 seconds**; 3,597 eight-byte `pwrite` calls, 28,776 payload bytes, one-second pause after each call. Logical WAL delta excludes open/close records | [Hour job](https://github.com/mcurcio/agentfs/actions/runs/42220263763/job/127900250449), artifact `phase-1-measure-hour` (ID 13505399957) |
| Journal median/p99 vs plain FUSE | **NOT RUN to a report**: calls job reached its 120-minute job limit | [Calls job](https://github.com/mcurcio/agentfs/actions/runs/42220263763/job/127900250331) |
| Line-map bytes per 10,000 saves | **NOT RUN to a report**: storage job reached its 120-minute job limit | [Storage job](https://github.com/mcurcio/agentfs/actions/runs/42220263763/job/127900250209) |
| Save/take time, 1 MiB and 100 MiB | **NOT RUN to a report**: no artifact was produced before the calls job timed out | Same calls job |
| Landing wait incidence/duration | **NOT RUN to a report**: finite conformance progress passed, but the measurement artifact is absent | Same calls job |

Both timed-out jobs finished with cancellation conclusions at their configured
limits; they were not manually cancelled as a fix. Do not extrapolate figures
from them. The harness writes its report at the end, so intermediate samples
were lost. Persist incremental measurements and investigate queue-planning and
line-placement cost before repeating the large jobs. All these earlier runs
had finished when this handoff was prepared.

## Approved continuation decisions

The owner recorded the following current picks in `921528b`, after the observed
runs. They remain subject to the ledger's blocking-consideration rule. This
handoff does not implement them or claim new passing results.

1. **Sandbox and parent — queue; finding 32.** Start with six `-\n` lines.
   Hold a main write-open. Sandbox: `open(O_TRUNC)`, `write("y")`, `close`;
   then hold a sandbox write-open. Main: replace the second line with
   `m1.1\n` and close that writer. Sandbox: `pwrite("n6", 0)`, close;
   release the held main writer. The first sandbox save loses whole, but the
   next landing retains live lines `n6` and `m1.1\n`, rendering
   `n6m1.1\n`. This reproduces model finding 32 and blocks line integrity.
   **Recorded pick: R10.8.** This is a collision: the parent wins and the
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

3. **Sandbox and parent — durable queue storage.** Initialize and mount both
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
