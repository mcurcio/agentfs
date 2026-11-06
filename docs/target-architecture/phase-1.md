# Phase 1: implementation and end-to-end validation

Phase 1 is the MVP of PR #23. The owner described its purpose on 2026-11-05:
"The goal is to build the phase 1 / MVP of the proposed design in the PR so we
can run real end-to-end tests and validate the model".

**Phase 1 is not validated.** [git-model §18](git-model.md#18-phase-1-implementation-and-end-to-end-validation)
defines the exit tests. The [rules ledger](rules-ledger.md) governs outcomes.
Implementation is paused; the [handoff](phase-1-handoff.md) records the exit-test
snapshot, measured hour and remaining work. Owner decisions in `921528b` now
cover R10.8, R1.4, the R10.3 clarification and the journal pointer commit point.
The journal pointer commit point is implemented (below); the jobs named under
"Verification and recovery" judge it.
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
| `e8d12a6` | Real mounts passed the fixed two-way flow, whole-save loss, neighboring edits, inserts, unfinished-tail, edited-place, P1/P2 and byte-call cases. The strict finding-32 case failed in [CI logs](https://github.com/mcurcio/agentfs/actions/runs/42220263763/job/127900249921); the initial log pipeline masked its exit status and is being corrected. Fault and power-loss jobs also failed and require diagnosis |
| `4d90329` | Finding 32 correctly fails CI. The [two-writer page-cache job](https://github.com/mcurcio/agentfs/actions/runs/42221484166/job/127904349124) passes four of eight descriptor/mapping cases; shared mappings expose unsaved bytes to another mapping, and closing the first mapped writer may leave its save unpublished |
| `2a33e90` | Journal-kill and allocation-forcing `ENOSPC` write checks pass in the [fault log](https://github.com/mcurcio/agentfs/actions/runs/42223004088/job/127909394344); the supplementary full-state rename returns `EIO` rather than its test's expected `ENOSPC`. Power replay still fails: [ext4](https://github.com/mcurcio/agentfs/actions/runs/42223004088/job/127909394603) reports a corrupt reftable; [XFS](https://github.com/mcurcio/agentfs/actions/runs/42223004088/job/127909394686) cannot recover the reference store |

Linux can request `GETATTR` without a handle during an anonymous inode's
permission check. Searching only named paths caused the failure. The repair
uses the anonymous object and preserves its zero link count. This is an adapter
correction, not a new landing rule.

## Milestones and completion

| Milestone | Status |
|---|---|
| Recording, byte calls and object lifetime | Descriptor, byte and orphan cases pass; multiple shared mappings fail visibility/publication checks |
| Line-map storage and whole-save parent referee | Connected; mounted fixed-case comparisons pass except finding 32 |
| Live landing/sync, P1/P2, writer locks, no alarm | Mounted finite progress scenarios pass; quantitative wait measurements are unavailable |
| Journal kills, state exhaustion and power loss | The journal is the commit point for pointer steps; refs are a copy rebuilt on every open. Each row's evidence is in CI for the commit that claims it |
| Model conformance and §18 measurements | Finding 32 blocks validation. Actual hour recorded; calls and 10,000-save jobs reached their two-hour limits without reports |

Known model finding 32 reproduced on the real mount: a losing truncate to `y`,
followed by an offset write of `n6` before the parent decision was readable,
left live lines `n6` and `m1.1\n` joined as `n6m1.1\n`. Its pure diagnostic is
ignored with an explanation; the mounted case has no expected-failure waiver.
R10.8 now decides this as a parent-winning collision: the later save loses whole
and stays in history. The recorded failure predates that approval; implementing
and testing the decision remains due. Passing the other cases does not validate
the whole phase.

The page-cache probe uses independent opens in one tree. With two writable
shared mappings and a separate read-only mapping, stores through the writers
become readable through the observer before either writer closes, while ordinary
`pread` still returns saved bytes. A first writer's close can also leave saved
bytes unchanged until the other mapping closes. Those observations led to R1.4:
native shared pages are allowed, while descriptor privacy remains and Linux's
bytes must not be lost. The probe still contains the earlier privacy assertions;
its expectations and the mixed-write implementation need to follow R1.4 when
work resumes.

Power-loss recovery is also unresolved. Initial repository files now receive
explicit file and directory syncs. Git configuration and loose-object directory
syncs have also been corrected. Source inspection of Git 2.52 and 2.56 identifies
an additional storage issue: its reftable writer flushes before appending the
footer, and table-list publication and obsolete-table cleanup lack the required
directory ordering. Syncing a completed command cannot by itself protect a
previously acknowledged state from a cut during another command. The current
reftable backend did not establish §18's crash guarantee. A synced journal record
is now the commit point for each step's pointer changes and Git's refs are
rebuildable copies in the files format (`docs/design.md` §19.1, git-model §12).

## Run the tree views

Use Linux with `/dev/fuse`, `fusermount3` and Git with `core.fsync` (2.36) and
`rev-list --missing=print` on a blob tip. `ci/check-tree-git.sh` probes the rebuild of
the refs and that check; Git 2.47.3 was tested locally. Build with
`cargo build --locked -p agentfs`:

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
Each step's pointer changes (branch moves, queue entries numbered by their step,
collision refs, object tables) are one record in the journal, the commit point.
Git's refs are a copy in the files format and nothing decides from them; a `git`
command run during a step may see one half copied. Git children retain the actor
lease across process death.

The state area contains `history.git`, `calls.wal`, `actor.lock`, `format` and
private checkout caches, and `calls.wal.torn` after a torn tail. Caches expose only
the workload subtree, contain no `.git` entry, and are rebuilt before serving.
Workloads use the views, not the caches. Earlier state formats and `refs migrate`
are dropped: state from earlier commits of this work does not open, and general
upgrades remain excluded.

Every journal append is fsynced before acknowledgement. A step writes its objects,
checks and appends its record, then installs and copies; git-model §12 has the order
and what each failure does. A failed save keeps serving and saves at the next step in
its tree. Only a failed `fsync` or rollback of the journal, or a compaction failing
after its rename, stops the actor. Recovery reads the journal, rewrites `config` and
`HEAD`, refuses a lost object at any pointer's tip, rebuilds the refs, builds each
tree from its branch snapshot and object table, closes abandoned opens once through
the ordinary step, and compacts. It takes no queue entries: the view's worker does.
Compaction keeps restart memory at most `max(64 MiB, twice the last compacted
length)` plus live calls.

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
- `e2e-tree-recording-faults.sh`: process kills at every journal, object, commit-point,
  ref-copy and checkout failpoint, and at those only an open passes (the ref rebuild,
  both compaction points, a first init). Every returned write is in the next save, none
  twice.
- `e2e-tree-enospc.sh`: actual state exhaustion. A write and a rename with its objects
  failing answer `ENOSPC` and change nothing; a rename past its commit point answers 0
  and stands, with the copies stale; both keep serving. A failed journal `fsync` stops
  the actor.
- `power-loss-trees.sh`: ext4/XFS dm-log-writes replay of the tree path, on main and on
  a fork's branch. Each checkpoint also removes `refs/` and `packed-refs` and requires
  the same heads from the journal alone.
- `measure-trees.sh`: plain-FUSE comparison, an actual hour of writes, 10,000
  saves, 1 MiB/100 MiB saves/takes and queue waits. Measurements are not budgets.

The phase-1 workflow runs on this PR branch. Measurement jobs require
`[phase1-measure]` in the pushed commit message. Artifacts and exact SHA links
must accompany results before any exit is marked passed.

Only flat regular files with git-safe UTF-8 names are currently implemented.
Directories, symlinks, permission changes and general hard links are not implemented;
only anonymous first link is covered. Journal frames are limited to 96 MiB. Rules,
profiles, transactions, nested forks, replication, CSI, several view hosts and
upgrades remain outside phase 1. No transaction model or `Client::Outside` behavior
is changed. Telemetry export remains unwired.

On a stopped actor, preserve state, fix the I/O or space problem, and reopen for
replay. On `CorruptJournal`, `MissingObject` or an unknown format, follow the
[runbook](../runbooks/recording-failures.md#5-a-tree-state-area-that-refuses-to-open).
Nothing truncates journal records or resets history as automatic repair.
