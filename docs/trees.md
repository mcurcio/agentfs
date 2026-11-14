# Tree views

A tree view is a mount of a git-backed tree. Each close saves the file into its
own tree, and queues carry changes between an **upstream** and its **forks**. The
tree views run on their own state area, with their own commands (`agentfs trees`),
beside the recording mount described in [recording.md](recording.md).

Use them to try the model of upstreams and forks: a fork edits like an ordinary
directory, its saves flow to its upstream, and the upstream's saves flow back,
without anyone asking. What the model decides on every collision is written down
in the [rules ledger](target-architecture/rules-ledger.md) and the
[git model](target-architecture/git-model.md). This page says how to run the views,
what they do today, and where they stop.

## Terms

- A **tree** is a branch of one bare git repository.
- A **fork** is a tree made from another tree, its **upstream**. A fork of a fork
  has that fork as its upstream. A tree with no upstream is the root.
- A tree's **identity** is `root` or `fork/<n>`, where `n` is the journal event that
  made the fork; it is never reused. The tree's branch (`refs/heads/root`,
  `refs/heads/fork/<n>`), its queue, its checkout and the `Agentfs-Tree` trailer of
  every commit made in it name it by its identity, never by a branch name it is
  published under. Forks are taken in order of `n`, after the root.
- A **sync** brings the upstream's version into a fork. A **landing** brings a
  fork's save into its upstream. Two lines **collide** when each was written
  without seeing the other; the upstream's version wins and the loser's lines are
  kept.

## Run it

You need Linux with `/dev/fuse`, `fusermount3` and the `git` release pinned in
`ci/git-release` (`ci/check-tree-git.sh` checks it). Line identity depends on the
`git` release, so the library looks `git` up in a fixed `PATH` and fails on another
release in CI.

```sh
cargo build --locked -p agentfs
mkdir -p /tmp/agentfs-trees/root /tmp/agentfs-trees/fork
target/debug/agentfs --state /tmp/agentfs-trees/state trees init --volume trial
target/debug/agentfs --state /tmp/agentfs-trees/state trees mount \
  /tmp/agentfs-trees/root --sandbox /tmp/agentfs-trees/fork
```

Use ordinary file calls in either view. The command prints the fork's `fork/N`
name. Reuse it with `--fork fork/N`, or make one with `trees fork`. Unmount both
views with `fusermount3 -u PATH`; unmounting every view ends the process.

| Command or flag | Meaning |
|---|---|
| `trees init --volume NAME` | Create the volume, or find it under the same name, then recover it; prints `<name> <id> created` or `found`. Another name or another format exits 1 with nothing written |
| `trees fork` | Create a fork of the root at its saved head; prints its identity, `fork/N` |
| `trees mount ROOT` | Recover and mount the root on an existing empty directory. It never creates a volume: run `trees init` first |
| `--volume NAME` (`trees mount`) | Refuse, before anything is written, a volume with another name |
| `--owner UID:GID` | Every file's owner in the views (default: the mounting process's user and group) |
| `--no-allow-other` | Only the mounting user reaches the views. By default other users do (FUSE `allow_other`, which needs `user_allow_other` in `/etc/fuse.conf` when the mounting user is not root) |
| `--namespace`, `--pod`, `--pod-uid`, `--node` | Where the views run: the pod context of their commits (below) |
| `--sandbox DIR` | Also mount a fork in DIR (a new one, or `--fork`) |
| `--fork fork/N` | Reuse this fork (needs `--sandbox`); `root` is refused |
| `--writer LABEL`, `--sandbox-writer LABEL` | The root's and the fork's writer label (default `root-client`, `sandbox-client`). A label is trusted, not an authenticated identity |
| `--metrics-output FILE` | Write counters, the journal's length, each stage's runs, time and git children, and the objects stored, every 100 ms and at unmount |

Mount points must be empty, separate and outside the state area. Metrics never take
part in ordering. Every flag is in [configuration](configuration.md#flags-and-environment).

## Volume identity and trailers

`trees init --volume NAME` gives a new volume a random id (RFC 9562 version 4) and
writes it with the name to the state area's `volume` file, before the repository and
`format`. Every start reads `format` and `volume` before it writes anything. The id
never changes; the name is a label and a guard.

Every commit names its volume, its writer and where the writer ran, in this order:

```text
<subject>

Agentfs-Writer: <writer>
Agentfs-Cause: <Root | Direct | Landing | Sync>
Agentfs-Tree: <root | fork/<n>>
Agentfs-Event: <step number, 0 for the Root>
Agentfs-Volume: <volume name>
Agentfs-Volume-Id: <id>
Agentfs-Namespace: <namespace>     each pod field only when given
Agentfs-Pod: <pod>
Agentfs-Pod-Uid: <pod uid>
Agentfs-Node: <node>
Agentfs-Format: envelope-1         every commit but the Root
Agentfs-Open: <n>                  a save that closed an open
Agentfs-Collisions: <json>         a landing that decided any
```

A save carries its writer and the pod context of the mount it came through. The
journal keeps both with each write-open, so a save recovery makes after a crash
names the writer that opened the file and where it ran, not the restarted process.
A landing merge's subject is `Landing by <writer>`, and its writer and pod trailers
are those of the save it lands. A sync, a fork step and the Root commit are
`agentfs`'s, with the pod context of the process that serves the volume.

## What a tree does

- **Saves.** A close applies the file's byte calls to the current saved file and
  aligns its lines once (git's histogram diff, the one call in `agentfs-align`,
  shared with the model). A close that leaves the bytes as they were commits
  nothing: no commit and no queue entry. The history does not show that the program
  saved.
- **Close-to-open.** An open file is private until it closes: other readers see
  only saved content. A read-only descriptor keeps the bytes it opened on; a
  writable one reads its own calls and the file's page writes.
- **Flow.** The queues carry changes between the upstream and the fork. The
  upstream's queue decides collisions and keeps the losers. A fork receives what no
  open file holds back: a file open for writing, and its decisions, wait, and the
  rest arrives when the file closes.
- **Syncs.** When a fork has nothing waiting and shows what its upstream shows, a
  sync fast-forwards past the fork's earlier sync merge, which carries no edits. No
  ref reaches that merge any more.
- **Landings.** A landing on the upstream's head is a fast-forward: there is no
  landing commit, so history does not show when it landed or that a queue took it.
  Writer and tree still show. A landing locks names as well as files, so it can wait
  on an open file it does not change.
- **After a truncate**, writes count as a rewrite unless every call is an append.
- **The release of `git` decides alignment.** An upgrade is a known change: later
  saves can align repeated lines differently. Stored bytes and identities do not
  change.

`fsync` and close make the file's calls durable; the final release publishes the
file.

## Recording and durability

Every file call is in the journal before it returns: one frame, written with one
`write(2)` and no fsync. A call that returned survives a crash or kill of agentfs,
because its frame is in the kernel. It survives a power cut once one of these has
returned:

- `close(2)` of a descriptor that wrote (FUSE `FLUSH`), or `fsync(2)` of it;
- each `write(2)` of a descriptor opened `O_SYNC` or `O_DSYNC`;
- `fsync(2)` of the view's directory (FSYNCDIR), or of a read-only descriptor:
  every record so far, creates, renames and unlinks included.

A power cut loses the calls, renames and unlinks made since the last of these.
Waits share fsyncs (group commit): one fsync answers every wait whose records were
written before it began. The fsync runs off the view's request thread and outside
the actor's lock, so a waiting close holds no other call. A failed fsync stops the
actor: its waiters get its error and every later call `EIO` (see
[Recovery](#recovery)).

## The library

`agentfs_core::tree_actor::TreeActor` holds an exclusive state-area lease.
`TreeActor::create` makes a volume, `TreeActor::open` recovers one and never creates,
and `tree_actor::inspect` reads what a state area holds without the lease.
`tree_actor::Recording` serves a volume: the actor behind its one lock, with
`durable`, `durable_all` and their deferred forms waiting for durability outside it.
`Recording::open` makes every recovered record durable before it serves, and
dropping the last `Recording` makes every record durable. Each recorded call names
its writer as a `WriterAt` (the writer and its pod context). `write_open`, `write`,
`truncate` and `close` are the recorded calls. Upstream and fork
share one bare repository. A commit stores the files under `files/`, line maps in
`metadata.json` and name versions in `namespace.json` (`Agentfs-Format:
envelope-1`). Merge identity is separate from an open object's identity. See
[library/core.md](library/core.md).

## The journal

Every step's pointer changes (branch moves, queue entries, collision refs, object
tables) are one record in `calls.wal`, appended as a call is and made durable as a
call is. That record is the commit point. Git's
refs are a copy in the files format, rebuilt on every open, and nothing decides from
them. A `git` command run during a step may see the copy half written; each queue
entry is a loose ref file.

A step writes its objects, checks and appends its record, then installs and copies.

- A failed save keeps serving and saves at the next step in its tree. Only a failed
  journal `fsync` or rollback, or a compaction failing after its rename, stops the
  actor. A failed copy after the record is logged and the call succeeds.
- Recovery reads the journal, rebuilds the refs and trees, closes abandoned opens
  through the ordinary step and compacts. It refuses a missing journal, a missing
  `format` file, a lost object at a pointer's tip, and a journal that names a tree
  by anything but its identity, has no root, gives the root an upstream, gives an
  upstream to a fork with no branch, or gives a fork an upstream other than the root
  (forks are made only of the root). It deletes nothing outside a first init's own
  marker.
- Compaction keeps the journal at most `max(64 MiB, twice the last compacted
  length)` plus the live calls, and keeps only the page writes memory holds. A held
  file pins only its own calls. Frames are at most 96 MiB. The constants are not
  settings.
- Every frame carries the durable mark: the last record a finished fsync covered
  when the frame was written. At the first invalid frame, a later valid frame with
  this journal's salt whose mark passes the last valid record means a durable record
  was lost: corruption, and the journal refuses to open. Otherwise the hole is a
  torn tail of records no fsync had covered, saved to `calls.wal.torn` and cut off.
  Bit rot in the very last frame, or in records not yet durable, reads as a tear
  and drops those records; the log line and that file are the evidence.
- The state area is `volume`, `history.git`, `calls.wal`, `actor.lock`, `format`
  and rebuilt private checkout caches, which the workload never sees. A create writes
  an `initializing` marker first and removes it last; the next `trees init` starts a
  cut-short create over. **Earlier state formats (format 4 included) do not open**,
  and are never converted.

## The view

The FUSE adapter (`agentfs_fuse::tree_view`) uses zero entry and attribute TTLs and
direct I/O, and negotiates neither passthrough nor the writeback cache. A read with
no lock owner is a page fill and a write with `FUSE_WRITE_CACHE` is a page write.
`max_background` is as high as the kernel allows, to keep a release ahead of a later
open. One thread answers requests in order; a `FLUSH`, `FSYNC`, FSYNCDIR or `O_SYNC`
write is answered from the journal's waiter thread once its records are durable. The queue worker takes one entry at a time, releasing the actor between takes,
and drops the kernel's pages of files a take changed. A file open for writing
holds back that fork's syncs from its upstream until it closes; no alarm is raised
for a waiting open. See [library/filesystem.md](library/filesystem.md).

## Recovery

On a stopped actor, preserve the state, fix the I/O or space problem and reopen. On
`CorruptJournal`, `MissingObject` or an unknown format, follow
[runbooks/recording-failures.md](runbooks/recording-failures.md#5-a-tree-state-area-that-refuses-to-open).
Nothing truncates journal records or resets history as repair.

## Limits

What the tree views do not support:

- **Flat regular files with git-safe UTF-8 names only.** Directories, symlinks and
  non-UTF-8 names are not supported. Only an anonymous first link is covered.
- **Nested forks, transactions, `Client::Outside`, rules and view profiles,
  replication, CSI, several view hosts, upgrades and snapshot opens** are not
  built. The designs for them are in [target-architecture](target-architecture/README.md).
- **Telemetry export** is not wired. `--metrics-output` is the only measurement.
- **Large files are slow.** Every save and take reads and rewrites the whole tree, so
  a large file taxes every later step of every file. In one debug-build run on CI, a
  100 MiB file took 31.9 s to write and close and 339 s until both views converged
  (a 1 MiB file: 1.27 s), and five saves of unrelated small files took 238 to 241 s
  each to appear while it was queued. The line map is never pruned, so per-save cost
  grows with history: 10,000 one-line saves took 2,561 s in one run. See
  [performance.md](performance.md). These are observations from single runs, not
  budgets.
- **A call can wait.** A client call on a mount waits behind a take that has begun.
  A file held open can hold other files' decisions back for minutes in the
  measurements above. Only the mounted cases and the held-log measurement cover the
  wait; the model replays do not.
- **Shared mappings have a gap.** A write through an `mmap` is saved as a page. A
  mapped store that writes the bytes its page last held records nothing, so a
  descriptor write made meanwhile stands where Linux would keep the store: with a
  file `abcdefgh`, one writer `pwrite`s `WX` at 0, then another's mapped store writes
  back `ab`; Linux keeps `abcdefgh`, agentfs gives `WXcdefgh`. This was found by
  reading the code and has not been run. A store not yet written back is lost with
  the view host, which Linux would keep, and `stat` and `sendfile` show unsaved
  state.
- **Close-to-open** rests on kernel 7.0 and the sources of v6.8, v6.14 and v7.0: 64
  closes over a busy view, from the same process and another. A readahead in flight
  on the handle was not exercised.
- **The object-directory syncs** cannot be judged by prefix replay on ext4 and XFS
  (the journal's fsync carries the earlier rename). `ci/trees-object-dir-syncs.sh`
  proves only the order of system calls, not a crash.
- **Replays are not a proof.** The model's scenarios and seeded random interleavings
  are replayed on the model, the library and real mounts. They show no `mmap` call
  and no client call waiting on a take, and do not cover every interleaving.
- **A debug build and one runner.** Measurements are not budgets. Two runs of one
  mode have differed by up to two times.

## How it is verified

CI scripts judge the build; local `tree_actor`, `tree_git` and replay tests use real
repositories but are not FUSE or power-loss evidence. The scripts are mapped to their
jobs in [development.md](development.md); the method is in
`crates/agentfs-model/VERIFYING.md`.

- `e2e-trees.sh`, `e2e-trees-ordering.sh`: recording, objects, anonymous first link,
  immediate reopen.
- `e2e-tree-conformance.sh`: fixed model cases and byte calls against a plain Linux
  file.
- `e2e-tree-replay.sh`: the model's in-scope scenarios replayed on a fresh model, the
  library and real mounts, comparing each tree and each collision's lost lines, with
  invariants read from git.
- `e2e-tree-pagecache.sh`: shared-mapping behaviour in 21 cases, and 64 closes
  reopened.
- `e2e-tree-restart.sh`, `e2e-tree-recording-faults.sh`, `e2e-tree-enospc.sh`:
  view-host death, kills at each failpoint, state exhaustion.
- `power-loss-trees.sh`: `dm-log-writes` replay on ext4 and XFS, on the upstream and
  a fork, each checkpoint also rebuilding the refs from the journal alone.
- `measure-trees.sh`: plain-FUSE comparison, an hour of writes, 10,000 saves, 1 and
  100 MiB files, waits and a held log. `[phase1-measure]` in a pushed commit message
  runs them all.
