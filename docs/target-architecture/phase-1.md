# Phase 1: implementation and end-to-end validation

**The recording code described here is outdated and untested** (the owner, 2026-11-05: it is to be noted as
"outdated and untested"). A Codex session wrote it (43dad43, 22b91ab, 62457b8) before the rules ledger's latest decisions, and
nobody has reviewed it against [the ledger](rules-ledger.md). Do not treat its tests or this page's status as
evidence that a rule holds. The ledger wins wherever this page or that code differs from it.

Phase 1 is the MVP implementation of the proposed design in PR #23. The owner
clarified its purpose on 2026-11-05: "The goal is to build the phase 1 / MVP of
the proposed design in the PR so we can run real end-to-end tests and validate
the model". This code is the implementation foundation to build on.

The contract is [git-model.md](git-model.md). Its
[§18 exit criteria](git-model.md#18-phase-1-implementation-and-end-to-end-validation)
define what the real filesystem must validate. **Phase 1 is in progress.** The
recording milestone is written, and outdated and untested (above). The
landing/sync and model-conformance milestones remain to be built.

## Milestones and completion

| Milestone | Status | Validation |
|---|---|---|
| Tree actor, byte-call journal, main/sandbox branches and views | Written; outdated and untested | Library tests and mounted Linux scenarios cover byte calls, saved-reader snapshots, object lifetime and restart replay |
| Line-map storage and parent referee; whole-save landing and preserved losers | Due in phase 1 | Drive parent-wins and nonconflicting-line cases through real mounts; inspect saved bytes, Git ancestry and collision records against the model |
| Live landing and sync with P1 progress, P2 prefix sync and no P3 | Due in phase 1 | Exercise held files and cooperating programs; verify disjoint progress, ordered delivery, and measure how long landings wait (no alarm, ledger R9.10) |
| Close-to-open ordering and recording failure/recovery boundaries | Partly covered; exit criteria remain open | Immediate reopen in the same and another process; kill at every durable step; full state area; power-loss replay; object/page-cache cases in §18 |
| Model conformance on the implemented subset and measurements | Due in phase 1 | Reuse supported scenarios and seeded calls, comparing visible bytes and history with the model and independent oracles; record latency and space costs |

Passing recording tests is evidence for that milestone. Phase 1 completes when
the pass-or-fail criteria in §18 pass on Linux CI and findings are resolved in
the implementation or the model. Do not weaken a model invariant or silently
exclude a failing scenario. The
[conformance plan](../../crates/agentfs-model/VERIFYING.md#phase-1-the-model-as-the-conformance-suite-integration-still-due)
describes integration options. The independent model stays independent of product
code. Current tests compare recording bytes with Linux; they do not yet compare
landing/sync behavior with the model.

Phase 1 covers full lineage, the only history mode, with main and one sandbox. §18 places nested forks, transactions, Rules/view profiles, replication, CSI,
several view hosts and upgrades beyond this phase. The implementation grows
through the domain APIs below.

## Run the tree views

On Linux with `/dev/fuse` and `fusermount3`, build with
`cargo build --locked -p agentfs`, then use empty mountpoints and a dedicated state directory:

```sh
mkdir -p /tmp/agentfs-trees/main /tmp/agentfs-trees/sandbox
target/debug/agentfs --state /tmp/agentfs-trees/state trees init
target/debug/agentfs --state /tmp/agentfs-trees/state trees mount \
  /tmp/agentfs-trees/main --sandbox /tmp/agentfs-trees/sandbox
```

From another terminal, use ordinary file calls in either mount. Files saved in
one stay isolated from the other. The sandbox starts at main's saved head when
created. The process prints its `fork/N` name; after stopping it, reuse that
sandbox with `--fork fork/N`. `trees fork` creates one without a mount and
prints its name. Unmount **both** directories with `fusermount3 -u PATH` to exit.
Only the mounting user can access these views. Mountpoints must be empty and
must not overlap the state directory or one another. `--writer` and
`--sandbox-writer` set trusted labels; they do not authenticate clients.

The FUSE adapter uses direct I/O and zero entry/attribute TTLs. A read-only open
holds a snapshot of saved bytes; a writable open reads its own private bytes.
Unsupported operations fail instead of passing through to an unrecorded backing
file. Modes are fixed at 0700 for the root and 0600 for files. The adapter requests
direct-I/O shared mapping support when the kernel offers it.
The shared mapping and anonymous-file paths need the Linux CI scenarios in
`ci/e2e-trees.sh` before they can be considered validated. The adapter does not
negotiate passthrough or ordinary writeback cache.

**Publication ordering remains incomplete.** `FLUSH`/`fsync` syncs the journal;
`RELEASE` publishes the save. Linux sends `RELEASE` asynchronously; whether an immediate open after `close(2)`
always sees the save has not yet been established. The existing mounted tests
wait explicitly for publication and do not establish that ordering. The strict
`ci/e2e-trees-ordering.sh` probe checks it without publication polling.

## Recording library

`agentfs_core::tree_actor::Volume` is a synchronous actor owning one exclusively
locked state area. `write_open` binds a handle to a tree, writer and object;
`write` takes byte offsets or appends; `truncate` shrinks or grows with zero bytes.
`sync` ensures durability without publication; `close` publishes by replaying the
calls onto the tree's saved bytes at that moment. Each changed close commits one
writer, with `Agentfs-Open`, `Agentfs-Tree` and `Agentfs-Writer` trailers. Creation,
rename and unlink are recorded namespace changes too.

Main is `refs/heads/main`. `fork()` snapshots main into `refs/heads/fork/<n>`
in the same bare repository and records main as its local upstream. Fork saves
remain isolated. Landing and sync are the next implementation milestones:
native Git merge cannot replace the model's line identities or parent referee.

The state area contains `history.git`, `calls.wal`, `actor.lock`, `format`, and
private checkout caches at `trees/main` and `trees/fork/<n>`. No checkout contains
a `.git` entry. These first caches use git2 checkout, not linked Git worktrees or
on-disk indexes yet. They are rebuilt from the branches before serving after a
restart. Workloads must use the view API, never mutate a checkout or state area.
State written by the first recording increments remains readable. The namespace
change does not rewrite its journal or Git history; new state uses the tree label.

The byte-call WAL stores checksummed frames and persistent, monotonic event/open
numbers. Every call is fsynced before acknowledgement, a deliberate performance
cost before group commit. A recording or save failure stops the actor: its WAL
is retained, and reopening replays it. A complete corrupt frame refuses recovery;
only an incomplete final frame is discarded. No acknowledged record is silently
dropped as corruption. The WAL is not compacted: boot time and disk usage grow
with all calls. Account for this limit when running long tests.

Recovery reconstructs object IDs from the journal, including rename-over and
unlink, rather than matching paths. An old handle's later writes never change
the replacement. The close returns `OrphanDropped`. Recovery appends closes for
abandoned opens; a per-branch event trailer makes replay after an already-written
commit idempotent. Checkout failures retain the journal too.

## Current implementation boundaries

Only flat regular files with git-safe UTF-8 names; no directories, symlinks,
permissions changes or general hard links. `O_TMPFILE` and its first link and shared mappings are under test;
general hard-link behavior is not specified by the current model. The WAL currently
limits individual encoded frames to 96 MiB. Writer labels are trusted operator inputs, not
authenticated client capabilities. All admitted names are recorded; ignore and
attribute rules and view profiles are not wired yet.

Line identity and parent reconciliation primitives are present in the library but
are not connected to the tree actor. Model finding 32 remains open.

Still to implement in phase 1: line-map storage, the landing queue/referee,
automatic sync, collision refs/notes and progress checks. Transactions,
nested forks, replication, multi-host views, CSI integration and upgrades
are beyond §18’s initial phase. Authenticated RPC, telemetry export and journal
compaction are not wired in this milestone.
Existing model findings, including 27, are unchanged. This slice does not settle
them and must not be presented as full model conformance.

## Verification and recovery

`cargo test --locked -p agentfs-core --test tree_actor` checks byte calls against
a plain Linux file, publication at close, append onto a partial line, replay onto
the current head, object-safe rename/unlink, restart idempotence, checkout repair,
fork isolation, the exclusive actor lock, unsafe names, and torn/corrupt tails.
These tests use real repositories and disk files, including state compatibility
with the first recording increment’s format label. They also SIGKILL a subprocess
after an acknowledged write, inject a stale ref lock and fail a checkout after
a branch move. Restart recovers without duplicating a save. This is process-crash
evidence, not power-loss evidence.

`ci/e2e-trees.sh`, in the CI workflow's tree-view job, drives main and sandbox
mounts using actual file calls: write, read, append, `fsync`, offset writes and
truncation against a plain-file oracle; atomic save over an old writer; unlink and
recreate; rename of an open file; isolation; and killing the serving process with
an acknowledged write still open, then remounting and checking replay twice.
This script needs `/dev/fuse`; unit tests do not. It does not claim power-loss,
full state-area `ENOSPC`, live landing, prefix sync or full conformance coverage.

On `EIO` or a save failure, stop serving and preserve the state area. Fix space or
I/O problems before reopening it; reopening replays pending calls. On a journal
corruption or unknown format error, preserve a copy and investigate manually.
Do not truncate records, reset branches, or reuse the directory with production
commands. No automated repair procedure is claimed yet.

Still required by §18: mounted-process crash injection at every step; power-loss
replay; state-area `ENOSPC`; immediate close-to-open ordering across processes;
page-cache and mapping semantics; long-lived-writer progress and prefix sync;
the remaining model conformance scenarios; and latency/space measurements.
