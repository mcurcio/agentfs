# Phase 1: implementation and end-to-end validation

Phase 1 is the MVP of PR #23. The owner described its purpose on 2026-11-05:
"The goal is to build the phase 1 / MVP of the proposed design in the PR so we
can run real end-to-end tests and validate the model".

**Phase 1 is not validated.** [git-model §18](git-model.md#18-phase-1-implementation-and-end-to-end-validation)
defines the exit tests. The [rules ledger](rules-ledger.md) governs outcomes.
The original recording commits (`43dad43`, `22b91ab`, `62457b8`) were outdated
and untested against that ledger. They have since been audited and repaired;
their original passing tests are not conformance evidence.

## Ledger audit and current filesystem evidence

Keep the CLI composition, journal framing and useful object/history primitives.
Repair the actor, view and assertions against the ledger. The unsupported
16 MiB cross-handle reservation was removed. No new reconciliation rule,
transaction behavior or snapshot-open mechanism is introduced.

| Increment | Linux observation |
|---|---|
| `f418e03` | Immediate same/cross-process close/reopen, shared descriptor final close, writer/saved-reader bytes, concurrent closes, orphan release events and history checks passed in [CI](https://github.com/mcurcio/agentfs/actions/runs/42159661006/job/127702307383) |
| `d9d4a81` | Shared mappings passed; anonymous open exposed an adapter `ENOENT` in [CI](https://github.com/mcurcio/agentfs/actions/runs/42161001368/job/127706373615) |
| `798e6fa` | Anonymous open and first link passed after repairing handle-free inode attributes; shared mappings and the other tree-view checks also passed in [CI](https://github.com/mcurcio/agentfs/actions/runs/42176555005/job/127756236285) |
| Current queue integration | Local real-repository tests cover two-way flow, whole-save loss with retained history, P1 ordering, P2 prefix sync, namespace changes and replay of an incoming object. Mounted conformance and failure matrices are pending |

Linux can request `GETATTR` without a handle during an anonymous inode's
permission check. Searching only named paths caused the failure. The repair
uses the anonymous object and preserves its zero link count. This is an adapter
correction, not a new landing rule.

## Milestones and completion

| Milestone | Status |
|---|---|
| Recording, byte calls and object lifetime | Mounted observations above; current integrated revision still needs CI |
| Line-map storage and whole-save parent referee | Connected; local production tests pass, mounted comparison pending |
| Live landing/sync, P1/P2, writer locks, no alarm | Connected; local production tests pass, mounted progress and wait measurements pending |
| Journal kills, state exhaustion and power loss | CI harnesses prepared; no results claimed for the integrated revision |
| Model conformance and §18 measurements | Fixed-case oracle and measurement harnesses prepared; mounted results pending |

Known model finding 32 remains unresolved. Its pure diagnostic is ignored with
an explanation; the mounted conformance case has no expected-failure waiver.
A mounted reproduction must be reported before proposing a rule change.
Passing a subset does not validate the whole phase.

## Run the tree views

Use Linux with `/dev/fuse`, `fusermount3` and Git supporting reftable initialization,
reference transactions and `refs migrate`. `ci/check-tree-git.sh` probes those
operations; Git 2.52 was tested locally. Build with `cargo build --locked -p agentfs`:

```sh
mkdir -p /tmp/agentfs-trees/main /tmp/agentfs-trees/sandbox
target/debug/agentfs --state /tmp/agentfs-trees/state trees init
target/debug/agentfs --state /tmp/agentfs-trees/state trees mount \
  /tmp/agentfs-trees/main --sandbox /tmp/agentfs-trees/sandbox
```

Use ordinary file calls in either view. Each close saves in its own tree;
queues carry changes automatically between main and the sandbox. The parent
referee decides collisions and retains losing saves. The process prints the
sandbox's `fork/N` name. Reuse it with `--fork fork/N`; `trees fork` creates one
without mounting. Unmount both views with `fusermount3 -u PATH` to exit.
Mountpoints must be empty, separate from each other and outside state. Writer
labels are trusted labels, not authenticated identities.

The adapter uses zero entry/attribute TTLs and direct I/O. Read-only descriptors
retain saved bytes; writable descriptors read their own private calls. `fsync`
flushes the journal; final `RELEASE` publishes. The strict ordering script checks
immediate reopen without publication polling. Its finite passing observations
are listed above. Linux mapping lifetime and anonymous first-link cases are
also tested; ordinary writeback cache and passthrough are not negotiated.

`--metrics-output FILE` writes aggregate save, landing, sync and wait counters
every 100 ms and at unmount. The existing parent directory must be outside state
and the views. Collection errors are returned after unmount. Measurements never
participate in ordering or reconciliation.

## Recording library and storage

`agentfs_core::tree_actor::Volume` owns an exclusive state-area lease.
`write_open`, `write`, `truncate`, `sync` and `close` implement recorded calls.
Close applies byte calls to the current saved file and aligns lines once using
histogram diff. Merge-file identity is separate from an open object's identity;
renaming a newly written temporary over a target preserves the target's merge
identity while orphaning the old object. Old handles cannot modify a replacement.

Main and one sandbox share a bare Git repository. Each new commit stores workload
files under `files/`, line maps in `metadata.json`, and name versions in
`namespace.json`; `Agentfs-Format: envelope-1` identifies this layout. These
private metadata names do not reserve names in the workload's root. Collision
decisions travel in their landing commits, and collision refs retain losers.
Queue entries, branch moves and queue ordering use atomic reftable transactions.
The Git executable supplies that backend because the current libgit2 cannot
open reftable. Git children retain the actor lease across process death.

The state area contains `history.git`, `calls.wal`, `actor.lock`, `format` and
private checkout caches. Caches expose only the workload subtree, contain no
`.git` entry, and are rebuilt before serving. Workloads use the views, not the
caches. Compatibility with earlier recording state has local tests; general
upgrades remain excluded.

Every WAL append is fsynced before acknowledgement. Checksummed frames detect
corruption; only an incomplete final frame is discarded. Recording failures
stop the actor and retain its WAL. Recovery seeds committed snapshots, rebuilds
open objects and replays outstanding calls, then closes abandoned opens once.
Per-branch event watermarks and `Agentfs-Open` trailers prevent duplicate saves.
The WAL is not compacted, so startup cost and space grow with recorded calls.

The FUSE host wakes queue progress on mutations and releases the actor between
takes. A waiting entry holds no client lock; writer closes wake it again. P1
preserves dependencies on files and sources; P2 holds the suffix beginning at
the first overwritten file with a writer. No alarm is raised for waiting opens.
Overlapping writers can postpone a take indefinitely under the approved locks;
the measurement harness records finite workloads rather than asserting a budget.

## Verification and recovery

Local `tree_actor`, `tree_git`, referee and sync tests use real repositories and
processes. They are not FUSE or power-loss evidence. CI scripts exercise:

- `e2e-trees.sh` and `e2e-trees-ordering.sh`: recording, objects, mappings,
  anonymous first link, immediate reopen and process restart.
- `e2e-tree-conformance.sh`: real calls compared with fixed existing model cases,
  independent byte/line-map checks and progress scenarios.
- `e2e-tree-recording-faults.sh` and `e2e-tree-enospc.sh`: process kills at journal
  and durable Git boundaries, acknowledged-call replay, and actual state exhaustion.
- `power-loss-trees.sh`: ext4/XFS dm-log-writes replay of the tree path.
- `measure-trees.sh`: plain-FUSE comparison, an actual hour of writes, 10,000
  saves, 1 MiB/100 MiB saves/takes and queue waits. Measurements are not budgets.

The phase-1 workflow runs on this PR branch. Measurement jobs require
`[phase1-measure]` in the pushed commit message. Artifacts and exact SHA links
must accompany results before any exit is marked passed.

Only flat regular files with git-safe UTF-8 names are currently implemented.
Directories, symlinks, permission changes and general hard links are not implemented;
only anonymous first link is covered. WAL frames are limited to 96 MiB. Rules,
profiles, transactions, nested forks, replication, CSI, several view hosts and
upgrades remain outside phase 1. No transaction model or `Client::Outside` behavior
is changed. Telemetry export and journal compaction remain unwired.

On recording failure, preserve state, fix the I/O or space problem, and reopen
for replay. On complete-frame corruption or unknown format, preserve a copy and
investigate. Do not truncate journal records or reset history as automatic repair.
