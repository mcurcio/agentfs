# Experimental target-model prototype

The initial MVP starts [git-model §18](git-model.md#18-prototype-plan-first-recording-slice-built),
without replacing the production recorder. It is experimental, has a separate state
format, and must use a disposable state directory. The full prototype's exit
criteria are still open. This page records implementation scope, not new merge rules.

## Try the filesystem slice

On Linux with `/dev/fuse` and `fusermount3`, build with
`cargo build --locked -p agentfs`, then use an empty, disposable directory:

```sh
mkdir -p /tmp/agentfs-prototype/main /tmp/agentfs-prototype/sandbox
target/debug/agentfs --state /tmp/agentfs-prototype/state prototype init
target/debug/agentfs --state /tmp/agentfs-prototype/state prototype mount \
  /tmp/agentfs-prototype/main --sandbox /tmp/agentfs-prototype/sandbox
```

From another terminal, use ordinary file calls in either mount. Files saved in
one stay isolated from the other. The sandbox starts at main's saved head when
created. The process prints its `fork/N` name; after stopping it, reuse that
sandbox with `--fork fork/N`. `prototype fork` creates one without a mount and
prints its name. Unmount **both** directories with `fusermount3 -u PATH` to exit.
Only the mounting user can access these views. Mountpoints must be empty and
must not overlap the state directory or one another. `--writer` and
`--sandbox-writer` set trusted labels; they do not authenticate clients.

The FUSE adapter uses direct I/O and zero entry/attribute TTLs. A read-only open
holds a snapshot of saved bytes; a writable open reads its own private bytes.
Unsupported operations fail instead of passing through to an unrecorded backing
file. Modes are fixed at 0700 for the root and 0600 for files. The adapter does
not negotiate passthrough, shared writable mappings or writeback cache.

**Publication ordering remains incomplete.** `FLUSH`/`fsync` syncs the journal;
`RELEASE` publishes the save. Linux sends `RELEASE` asynchronously, so an immediate
open after `close(2)` can still see the previous saved bytes. The mounted tests
wait explicitly for publication. This is a known gap against §18's immediate
close-to-open exit criterion, not an accepted change to that contract.

## Recording library

`agentfs_core::prototype::Volume` is a synchronous actor owning one exclusively
locked state area. `write_open` binds a handle to a tree, writer and object;
`write` takes byte offsets or appends; `truncate` shrinks or grows with zero bytes.
`sync` ensures durability without publication; `close` publishes by replaying the
calls onto the tree's saved bytes at that moment. Each changed close commits one
writer, with `Agentfs-Open`, `Agentfs-Tree` and `Agentfs-Writer` trailers. Creation,
rename and unlink are recorded namespace changes too.

Main is `refs/heads/main`. `fork()` snapshots main into `refs/heads/fork/<n>`
in the same bare repository and records main as its local upstream. Fork saves
remain isolated. There is deliberately no landing or sync implementation yet:
native Git merge cannot replace the model's line identities or parent referee.

The state area contains `history.git`, `calls.wal`, `actor.lock`, `format`, and
private checkout caches at `trees/main` and `trees/fork/<n>`. No checkout contains
a `.git` entry. These first caches use git2 checkout, not linked Git worktrees or
on-disk indexes yet. They are rebuilt from the branches before serving after a
restart. Workloads must use the view API, never mutate a checkout or state area.

The byte-call WAL stores checksummed frames and persistent, monotonic event/open
numbers. Every call is fsynced before acknowledgement, a deliberate performance
cost before group commit. A recording or save failure stops the actor: its WAL
is retained, and reopening replays it. A complete corrupt frame refuses recovery;
only an incomplete final frame is discarded. No acknowledged record is silently
dropped as corruption. The WAL is not compacted: boot time and disk usage grow
with all calls. Do not use this implementation for long-running workloads.

Recovery reconstructs object IDs from the journal, including rename-over and
unlink, rather than matching paths. An old handle's later writes never change
the replacement. The close returns `OrphanDropped`. Recovery appends closes for
abandoned opens; a per-branch event trailer makes replay after an already-written
commit idempotent. Checkout failures retain the journal too.

## Current limits

Only flat regular files with git-safe UTF-8 names; no directories, symlinks,
permissions changes, hard links, `mmap`, `O_TMPFILE`, or direct access by another
process. The initial size guard is 16 MiB, conservatively reserved across pending writers. This is a prototype resource limit,
not a new production policy. Writer labels are trusted operator inputs, not
authenticated client capabilities. All admitted names are recorded; ignore and
attribute rules and view profiles are not wired yet.

Not built: line-map storage, the landing queue/referee, automatic sync, collision
refs/notes, transactions, squashed history, nested forks, replication, RPC,
multi-host views, CSI integration, telemetry export, journal compaction and upgrades.
Existing model findings, including 27, are unchanged. This slice does not settle
them and must not be presented as full model conformance.

## Verification and recovery

`cargo test --locked -p agentfs-core --test prototype` checks byte calls against
a plain Linux file, publication at close, append onto a partial line, replay onto
the current head, object-safe rename/unlink, restart idempotence, checkout repair,
fork isolation, the exclusive actor lock, unsafe names, and torn/corrupt tails.
These tests use real repositories and disk files. They also SIGKILL a subprocess
after an acknowledged write, inject a stale ref lock and fail a checkout after
a branch move. Restart recovers without duplicating a save. This is process-crash
evidence, not power-loss evidence.

`ci/e2e-prototype.sh`, in the CI workflow's prototype job, drives main and sandbox
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
