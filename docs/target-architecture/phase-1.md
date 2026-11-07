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
| `06663b4` | The journal as commit point. Kills at every journal, object, commit-point, ref-copy, checkout, rebuild, compaction and first-init failpoint, and real `ENOSPC` before and after the commit point, pass in the [fault job](https://github.com/mcurcio/agentfs/actions/runs/42295808940/job/128160614784). Power replay passes on [ext4](https://github.com/mcurcio/agentfs/actions/runs/42295808940/job/128160614975) (724 checkpoints) and [XFS](https://github.com/mcurcio/agentfs/actions/runs/42295808940/job/128160614951) (408), each also rebuilding the refs from the journal alone. Planted bugs: dropping `core.fsync` for objects [fails both](https://github.com/mcurcio/agentfs/actions/runs/42296893603). Dropping the journal's fsync per record ([run](https://github.com/mcurcio/agentfs/actions/runs/42296883413), branch `wip/agentfs-eh5.1-m1.15`) and the object directory syncs ([run](https://github.com/mcurcio/agentfs/actions/runs/42296888045), `wip/agentfs-eh5.1-m1.16`) pass both, so **those two orderings are unproved**: the workload fsyncs before every close, and a fsync on these filesystems may carry the new directory entry too |
| `3016140` | Shared mappings as R1.4 words them, on kernel 7.0.0-1017-azure. All 21 page-cache cases, `trees-mmap`, 64 closes reopened at once over a busy view (same process and another) and the view-host restart pass in the [page-cache job](https://github.com/mcurcio/agentfs/actions/runs/42312178878/job/128216066550). Planted bugs: ignoring `FUSE_WRITE_CACHE` [passes 13 of 21 cases](https://github.com/mcurcio/agentfs/actions/runs/42309475616) (M3.8, branch `wip/agentfs-eh5.3-m3.8`), no lock-owner route [passes 5 of 21](https://github.com/mcurcio/agentfs/actions/runs/42309478789) (M3.9), a size only with page state [fails `create-then-map` and `grown-mapping`](https://github.com/mcurcio/agentfs/actions/runs/42309480548) (M3.10), no invalidation after a take [fails `take-refreshes-mapping`](https://github.com/mcurcio/agentfs/actions/runs/42309484689) (M3.11), `max_background` 16 [reads old bytes in 4 and 1 of 64 reopens](https://github.com/mcurcio/agentfs/actions/runs/42312191546) (M3.12), an object found by name at restart [fails the replaced file](https://github.com/mcurcio/agentfs/actions/runs/42309490679) (M3.13). The close-to-open row rests on this kernel and on the source of v6.8, v6.14 and v7.0: a readahead in flight on the handle was not exercised |
| `e270061` | The model's recorded calls, [replayed on CI](https://github.com/mcurcio/agentfs/actions/runs/42330151965/job/128274575889). A fresh model: 69 scenario traces and all 94 seed traces. The library: 69 and 78, with 16 seed traces failing on findings 36 to 41. Real mounts, on kernel and git as the other rows: 66 and 16, none mismatched; 81 in-scope traces are not mountable (two forks, a fork after a client call, or one open that both appends and writes at an offset), and those include all 16 that fail on the library. **So the mount replay passing does not cover the findings.** Planted bugs, all caught: on mounts, [R10.8 off](https://github.com/mcurcio/agentfs/actions/runs/42330173796) (M4.1, branch `wip/agentfs-eh5.4-m4.1`); locally on the library, M4.2 to M4.7 and three more for `one_writer`, `no_duplicate_opens` and `loser_reachable`; M4.8 on the model. Not tripped by any planted bug: `full_lineage`, `converged`, `parent_kept` |
| `b1e6d85` | Findings 36 to 41 closed, [replayed on CI](https://github.com/mcurcio/agentfs/actions/runs/42395719726/job/128472547246) on git 2.56.0: a fresh model and the library each 72 scenario traces and all 94 seed traces, mounts 69 and 16, none mismatched, none listed. [ci](https://github.com/mcurcio/agentfs/actions/runs/42395719692) and [image](https://github.com/mcurcio/agentfs/actions/runs/42395719725) passed. |
| `e881bca` | The library at seeds 1 to 500 of the phase 1 generators, [four jobs](https://github.com/mcurcio/agentfs/actions/runs/42393965025) of about 50 minutes: 976 of 1,000 traces pass, 24 mismatch (findings 44 to 47). The same seeds' mechanisms fail on the code before these fixes. |
| `5871b19` | Findings 42 to 47 closed by the owner's answers of 2026-11-07, on git 2.47.3 in every job ([phase-1 run](https://github.com/mcurcio/agentfs/actions/runs/42415551804)): a fresh model and the library each 74 scenario traces and all 94 seed traces, mounts 71 and 16, none mismatched. At seeds 1 to 500 the library passes 989 of 1,000 traces; the other 11 are findings 48 (8) and 49 (3), open for the owner. The model passes all 3,000 runs of seeds 1 to 500, locally. |

Linux can request `GETATTR` without a handle during an anonymous inode's
permission check. Searching only named paths caused the failure. The repair
uses the anonymous object and preserves its zero link count. This is an adapter
correction, not a new landing rule.

## Milestones and completion

| Milestone | Status |
|---|---|
| Recording, byte calls and object lifetime | Descriptor, byte and orphan cases pass; shared mappings follow R1.4 and the restart row follows §18 as reworded: read the phase-1 page-cache job |
| Line-map storage and whole-save parent referee | Connected; R10.8 and R10.9 built; mounted result of findings 32 and 33: read the phase-1 conformance job |
| Live landing/sync, P1/P2, writer locks, no alarm | Mounted finite progress scenarios pass; the wait counts of one held-log run are in the [handoff](phase-1-handoff.md#measurements) |
| Journal kills, state exhaustion and power loss | The journal is the commit point for pointer steps; refs are a copy rebuilt on every open. Each row's evidence is in CI for the commit that claims it |
| Model conformance and §18 measurements | The replay passes at CI's seeds, findings 36 to 47 closed by the owner's answers of 2026-11-07. Findings 48 and 49 are open for the owner, both from seeds 1 to 500 on the library. The §18 measurements report on `964618a` and `12bd7a8` ([handoff](phase-1-handoff.md#measurements)): journal against plain FUSE, hour, 10,000 saves, 1 and 100 MiB, waits and the held log. No figure is a budget; 10,000 saves took 2,411 s once the history walk and the per-line Git calls were fixed |

Known model finding 32 reproduced on the real mount: a losing truncate to `y`,
followed by an offset write of `n6` before the parent decision was readable,
left live lines `n6` and `m1.1\n` joined as `n6m1.1\n`. Its pure diagnostic is
enabled and passes now; the mounted case has no expected-failure waiver.
R10.8 decides this as a parent-winning collision: the later save loses whole
and stays in history (`land_save`, `tree_line_rules.rs`). R10.9 hides the
parent's later lines behind an undecided unfinished line at a sync. Passing these
cases does not validate the whole phase.

The page-cache probe uses independent opens in one tree. With two writable
shared mappings and a separate read-only mapping, stores through the writers
became readable through the observer before either writer closed, while ordinary
`pread` still returned saved bytes. Those observations led to R1.4: mappings share
pages as on Linux, read-only descriptors keep saved bytes, and no writer's bytes
are lost that Linux would keep. The probe now expects that, each value from the
same calls on a plain Linux file, and `tree_actor::pages` implements it. A store
the kernel has not written back is in its page alone, so a view host's death loses
it, which Linux would keep. Mapped files stay revisable (the owner is "still on the
fence about memory mapped files"): the code is contained in `tree_actor/pages.rs`
and the read, write and size paths of `tree_view.rs`.

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
retain saved bytes; writable descriptors read their own calls and the file's page
writes. A read with no lock owner is a page fill and a write with `FUSE_WRITE_CACHE`
a page write, whatever `O_APPEND` says (R1.4). Every reply gives the arrival-order
length while the file has a write-open. The queue worker drops the kernel's pages of
each file a take changed, with the actor unlocked. `fsync` flushes the journal; final
`RELEASE` publishes. `max_background` is as high as the kernel allows, to keep
`RELEASE` ahead of a later `OPEN` (git-model §18). The strict ordering script checks
immediate reopen without publication polling. Its finite passing observations
are listed above. Linux mapping lifetime and anonymous first-link cases are
also tested; ordinary writeback cache and passthrough are not negotiated.

`--metrics-output FILE` writes aggregate save, landing, sync and wait counters,
the journal's length and, for each actor stage, its runs and time, with the
count of Git children started (`tree_actor/stages.rs`), every 100 ms and at
unmount. The existing parent directory must be outside state
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
- `phase1-replay` and `e2e-tree-replay.sh`: every in-scope scenario and seed of the model,
  its calls replayed on a fresh model, the real library and real mounts, what each tree
  shows compared, invariants read from git. VERIFYING.md "Phase 1" says what this does not
  prove.
- `e2e-tree-pagecache.sh`: writers in one tree in every descriptor and mapping mix,
  with closes in both orders, three writers, a grown mapping, create then map, an
  unlinked mapped file's `fstat`, a take refreshing a mapping and `sendfile` while
  writing; then close-to-open (`trees-close-open.py`), 64 closes over a busy view.
- `e2e-tree-restart.sh`: a view host killed with handles open. An old handle's writes
  fail, the file holds what returned before the kill, saved once, and a file replaced
  before the kill or through the new mount stays unchanged.
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
