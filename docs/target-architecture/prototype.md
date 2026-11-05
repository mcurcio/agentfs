# Experimental target-model prototype

The initial MVP starts [git-model §18](git-model.md#18-prototype-plan-first-recording-slice-built),
without replacing the production recorder. It is experimental, has a separate state
format, and must use a disposable state directory. The full prototype's exit
criteria are still open. This page records implementation scope, not new merge rules.

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

On `EIO` or a save failure, stop serving and preserve the state area. Fix space or
I/O problems before reopening it; reopening replays pending calls. On a journal
corruption or unknown format error, preserve a copy and investigate manually.
Do not truncate records, reset branches, or reuse the directory with production
commands. No automated repair procedure is claimed yet.

Still required by §18: mounted-process crash injection at every step; power-loss
replay; state-area `ENOSPC`; immediate close-to-open ordering across processes;
page-cache and mapping semantics; long-lived-writer progress and prefix sync;
the remaining model conformance scenarios; and latency/space measurements.
