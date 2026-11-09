# Phase 1: implementation and end-to-end validation

Phase 1 is the MVP of PR #23. The owner described its purpose on 2026-11-05:
"The goal is to build the phase 1 / MVP of the proposed design in the PR so we
can run real end-to-end tests and validate the model".

**Verdict, 2026-11-09: Phase 1 is validated against §18 as written, with named limits: every pass-or-fail exit row passes on CI's Linux runners at `6312204`, and the measurements are recorded. It is not proof of every interleaving, of shared-mapping behaviour in full, of a client call waiting on a take, or of anything outside the rows. Four questions are open for the owner (below) and no row depends on them. The owner decides whether the limits suffice, and merges.**

[git-model §18](git-model.md#18-phase-1-implementation-and-end-to-end-validation)
defines the exit tests and the [rules ledger](rules-ledger.md) governs outcomes.
The [handoff](phase-1-handoff.md) holds the exit table, each row marked PASS, FAIL
or NOT RUN with the run that shows it, and the measurements. The evidence is run
[42563670392](https://github.com/mcurcio/agentfs/actions/runs/42563670392) of the
`phase-1` workflow on `6312204`, with the seeds of run
[42555683429](https://github.com/mcurcio/agentfs/actions/runs/42555683429) on
`e9ab747`. No product code changed between `e9ab747` and the commits after it
(a diff of `e9ab747..HEAD` over `crates`, `ci`, `vendor` and `.github` shows only
VERIFYING.md, the held-log harness and a workflow trigger).

## What is not covered

- **A client call waiting on a take.** The model's take is one atomic step (finding
  49, the owner's "B"), so R9 and R9.7's wait is untested by any replay. Only the
  finite mounted P1 and P2 cases and the held-log measurement meet it.
- **Shared mappings, R1.4.** No replay makes an `mmap` call. The page-cache job
  does, in 21 cases, and it records one gap: a mapped store that writes the bytes its
  page last held records nothing, so a descriptor write made meanwhile stands where
  Linux would keep the store. The owner accepted it for now (design §19.1).
- **Close-to-open** rests on kernel 7.0 and the source of v6.8, v6.14 and v7.0: 64
  closes over a busy view, same process and another. A readahead in flight on the
  handle was not exercised.
- **The object-directory syncs.** Prefix replay on ext4 and XFS cannot judge them
  (fsync of the journal carries the earlier rename). `ci/trees-object-dir-syncs.sh`
  proves only the order of system calls, not a crash.
- **Nested forks, transactions, `Client::Outside`, Rules and profiles, replication,
  CSI, several view hosts, upgrades, snapshot opens, directories and symlinks** are
  outside phase 1. Finding 50 (a nested fork's prefix sync past its upstream's
  fast-forward) is answered in design §19.1 and checked in the model only.
- **Seeds 1 to 500** ran on `e9ab747`, not on every later commit.
- **A debug build and one runner.** Each measurement is one run, on the runner's ext4
  and git 2.47.3. Two runs of one mode have differed by up to 2 times.

## The owner's answers

Finding 50, finding 49's cost, R1.4's gap and a fast-forward past a fork's own Sync
merge are answered in design §19.1. Each is general guidance, revisable; none changes
an exit row.

## What is built

Run the tree views on Linux with `/dev/fuse`, `fusermount3` and git 2.36 or later
(`ci/check-tree-git.sh` checks the release pinned in `ci/git-release`):

```sh
cargo build --locked -p agentfs
mkdir -p /tmp/agentfs-trees/main /tmp/agentfs-trees/fork
target/debug/agentfs --state /tmp/agentfs-trees/state trees init
target/debug/agentfs --state /tmp/agentfs-trees/state trees mount \
  /tmp/agentfs-trees/main --sandbox /tmp/agentfs-trees/fork
```

Use ordinary file calls in either view. Each close saves in its own tree and the
queues carry changes between the upstream and the fork. The upstream's queue decides
collisions and keeps the losers. The process prints the fork's `fork/N` name; reuse
it with `--fork fork/N`, or make one with `trees fork`. Unmount both views with
`fusermount3 -u PATH`. Mountpoints must be empty, separate and outside state. Writer
labels are trusted labels. `--metrics-output FILE` writes counters, the journal's
length and each actor stage's runs, time and git children every 100 ms and at
unmount; it never takes part in ordering.

**Library.** `agentfs_core::tree_actor::Volume` holds an exclusive state-area lease.
`write_open`, `write`, `truncate`, `sync` and `close` are the recorded calls. A close
applies the byte calls to the current saved file and aligns lines once (histogram
diff, `agentfs_align`, shared with the replay). A close that leaves the bytes as they
were commits nothing (R8.2). Merge identity is separate from an open object's
identity. Upstream and fork share one bare repository; a commit stores the files
under `files/`, line maps in `metadata.json` and name versions in `namespace.json`
(`Agentfs-Format: envelope-1`).

**Journal.** Every step's pointer changes (branch moves, queue entries, collision
refs, object tables) are one fsynced record in `calls.wal`, the commit point. Git's
refs are a copy in the files format, rebuilt on every open, and nothing decides from
them (git-model §12). A step writes its objects, checks and appends its record, then
installs and copies. A failed save keeps serving; only a failed journal `fsync` or
rollback, or a compaction failing after its rename, stops the actor. Recovery reads
the journal, rebuilds the refs and trees, closes abandoned opens through the ordinary
step and compacts. A compaction keeps the journal at most `max(64 MiB, twice the last
compacted length)` plus live calls, and keeps only the page writes memory holds.
Frames are at most 96 MiB. The state area is `history.git`, `calls.wal`,
`actor.lock`, `format` and rebuilt private checkout caches, which the workload never
sees. Earlier state formats do not open.

**View.** The adapter uses zero entry and attribute TTLs and direct I/O. Read-only
descriptors keep saved bytes; writable ones read their own calls and the file's page
writes. A read with no lock owner is a page fill and a write with `FUSE_WRITE_CACHE`
a page write (R1.4); the code is in `tree_actor/pages.rs` and the read, write and size
paths of `tree_view.rs`. `fsync` flushes the journal and the final `RELEASE`
publishes. `max_background` is as high as the kernel allows, to keep `RELEASE` ahead
of a later `OPEN`. The queue worker takes one entry at a time, releasing the actor
between takes, and drops the kernel's pages of files a take changed. P1 keeps
dependencies on files and sources; P2 holds the suffix from the first overwritten
file with a writer. No alarm is raised for a waiting open.

Only flat regular files with git-safe UTF-8 names are implemented; only an anonymous
first link is covered. Telemetry export is unwired.

## Verification and recovery

CI scripts judge the build; local `tree_actor`, `tree_git` and replay tests use real
repositories but are not FUSE or power-loss evidence.

- `e2e-trees.sh`, `e2e-trees-ordering.sh` (workflow `ci`): recording, objects, anonymous
  first link, immediate reopen.
- `e2e-tree-conformance.sh`: fixed model cases and byte calls against a plain Linux file.
- `phase1-replay`, `e2e-tree-replay.sh`: the model's in-scope scenarios and seeds
  replayed on a fresh model, the library and real mounts, comparing each tree and each
  collision's lost lines, with invariants read from git (VERIFYING.md "Phase 1").
- `e2e-tree-pagecache.sh`: R1.4 in 21 cases, `trees-mmap`, 64 closes reopened.
- `e2e-tree-restart.sh`, `e2e-tree-recording-faults.sh`, `e2e-tree-enospc.sh`: view-host
  death, kills at each failpoint, state exhaustion.
- `power-loss-trees.sh`: dm-log-writes replay on ext4 and XFS, on the upstream and a
  fork, each checkpoint also rebuilding the refs from the journal alone.
- `measure-trees.sh`: plain-FUSE comparison, an hour of writes, 10,000 saves, 1 and
  100 MiB, waits, a held log. Measurements are not budgets. `[phase1-measure]` in a
  pushed commit message runs them all, `[phase1-held]`, `[phase1-calls]` and
  `[phase1-pilot]` one mode, and `[phase1-seeds]` the library at seeds 1 to 500.

On a stopped actor, preserve state, fix the I/O or space problem and reopen. On
`CorruptJournal`, `MissingObject` or an unknown format, follow the
[runbook](../runbooks/recording-failures.md#5-a-tree-state-area-that-refuses-to-open).
Nothing truncates journal records or resets history as repair.
