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

## File maps

No file's bytes are held in memory, by the views or by the builder. A file is a map: pieces of journal frames
and of the private checkout's file as built, by file offset. A write adds one
piece that points at its frame, so it costs its own bytes at any file size: a
4 KiB write into a 1 GiB file copies 4 KiB, into its frame. An open starts from
the saved map, shared until it writes (copy on write). A close applies the
open's calls to the saved map with the same function the builder replays, and
tells a close that changes nothing by comparing only the bytes the open's calls
touched. A view reads a map with the actor unlocked: the lock is held only to
clone it.

- After a batch is built and checked out, the builder reads each changed file
  from its checkout instead of the frames, through a hard link in the state
  area's `bases/` (cleared at each start), and the actor takes that map at its
  next call where it saved the same step. A checkout writes each file to a
  temporary name and renames it over the old one, so a reader that opened the
  old version keeps reading it through its link; the link goes when the last
  map lets go of it.
- A map holds no descriptor: reads open the link through a cache of at most
  256 open files per process, so a volume of any number of files opens and
  reads under the default open-files limit.
- A take's merged files are in chunk records of at most 16 MiB just before
  its step, written from the files whose lines it keeps (a version's bytes are
  the same in every file that holds it), and their maps read them there; a
  take of a file of any size records.
- The builder aligns a save reading both versions of the file a line at a
  time, with a sampled index of where lines start. A new file and an append
  run no `git diff`; any other save writes both versions to `align/` in the
  state area for one `git diff` and removes them. `git diff` itself reads both
  files whole, so an edit of a file near the host's memory needs that memory
  in the `git` child. Blobs over 8 MiB stream through `git fast-import` and
  `git cat-file` (`core.bigFileThreshold`), which never hold them whole.
- The save record holds the result's length and a digest of its pieces: the
  close's own pieces with their places, and the bytes it kept as runs. The
  builder's replay must reach the same digest (`Disagrees`).

## The library

`agentfs_core::tree_actor::TreeActor` holds an exclusive state-area lease.
`TreeActor::create` makes a volume, `TreeActor::open` recovers one and never creates,
and `tree_actor::inspect` reads what a state area holds without the lease.
`tree_actor::Recording` serves a volume: the actor behind its one lock, with
`durable`, `durable_all` and their deferred forms waiting for durability outside it.
`Recording::open` makes every recovered record durable and serves at once, from the
journal: it never waits for history. Dropping the last `Recording` makes every record
durable and builds it.
`Recording::history` is the one owner of the Git store (`History`: the built point,
`wait_built`, built snapshots, lag), and `settle` waits until every record so far is
built: call it before reading Git. `TreeActor::head` settles first. Each recorded call names
its writer as a `WriterAt` (the writer and its pod context). `write_open`, `write`,
`truncate` and `close` are the recorded calls. Upstream and fork
share one bare repository. Merge identity is separate from an open object's
identity. See [library/core.md](library/core.md).

## What a commit holds

Each commit (`Agentfs-Format: envelope-1`) keeps three entries per file, in two
levels of hashed folders:

```text
files/<aa>/<bb>/<name>   the file's bytes
lines/<aa>/<bb>/<name>   JSON [identity, line map]: the file's lines as runs, never their bytes
names/<cc>/<dd>/<id>     JSON name history of one file identity, ended files included
```

`aa` and `bb` are hex digits 1-2 and 3-4 of `git hash-object --stdin` of the
name's bytes; `cc` and `dd` the same of the identity in decimal. The folders are
fixed (256 by 256), so a file's path never changes when files come or go, and a
commit that changes one file rewrites only the folders on its three paths: the
builder's cost per save is the same at 100 files and at 100,000. `git log --
files/2e/65/a` is the history of a file named `a` (`printf a | git hash-object
--stdin` starts `2e65`). Each tree's private checkout (`trees/<tree>/` in the
state area) mirrors `files/`: `trees/root/2e/65/a`.

A line map is `{"lines": [runs], "ancestors": [runs]}`. A line run is
`{"identity": "e:i", "version": "e:j", "count": n}`: `n` lines whose
identities and versions number on (`e:i`, `e:i+1`, ...), with `"dead": true`
for deleted lines and `"open": len` when its last line has no newline (that
line's length in bytes). The live lines divide the file's bytes in order,
each at its newline. An ancestor run `{"version": "e:j", "count": n, "over":
"d:k"}` says each of `n` versions was written over the one numbered alike
from `d:k`; without `over` they are new lines. A new file is one run of each
at any size, so a map costs its saves' edits, not its file's bytes.

Object numbers (which file an open handle writes to) are not in history. They are
the journal's: a checkpoint holds each tree's objects as of its built point, and
the steps after it move them.

## The journal

Every step (a create, a link, a close that saves, a rename, an unlink, a fork, a
take) is one record in the journal: the change itself and the pointer changes its
build publishes (branch moves, queue entries, collision refs), each
value an oid or "what step `n`'s build gives". It is appended as a call is and made
durable as a call is. That record is the commit point. The actor decides the step,
appends it and applies it to the saved state the views serve; it runs no git.

- The journal is a directory of segments, `journal/<first record>.seg`, each
  about 64 MiB; a frame never spans two, and a frame over 64 MiB has a segment of
  its own. A new segment's directory entry is fsynced before the durable mark
  covers any record in it.
- A failed step keeps serving and saves at the next step in its tree. Only a failed
  journal `fsync` (a leader's or a compaction's) or rollback stops the actor; a
  failed fsync is never retried, since the page cache then says nothing about the
  disk.
- Recovery reads the journal and serves from it without waiting for history: the
  saved state is the trees as built (read from Git and the private checkouts)
  with every later record replayed on them by the transitions the actor ran at
  the call. It then closes abandoned opens through the ordinary step; the builder
  catches up behind, as at any time (see
  [History trails the files](#history-trails-the-files)), and a failed build
  raises its alarm without refusing the open. The recovered records are made
  durable before anything is served (the directory first), and the steps past the
  built point count in the lag gate as they did at their call. A journal whose
  steps are all built compacts at once; otherwise compaction runs at the first
  call after the builder catches up. It refuses
  a missing journal, a missing `format` file, a lost object at a pointer's tip, and
  a journal that names a tree by anything but its identity, has no root, gives the
  root an upstream, gives an upstream to a fork with no branch, or gives a fork an
  upstream other than the root (forks are made only of the root). It deletes
  nothing outside a first init's own marker.
- Compaction runs when the segments pass `max(64 MiB, twice the last compacted
  length)`: it writes a checkpoint at the start of a new segment, then drops
  (unlinks) every older segment whose records the builder has read and which
  holds no live open's call and no page write memory holds. A held file pins only its own
  frames: they are copied forward (marked `carried`, their record numbers kept)
  once the dead bytes they pin exceed their own, so the segments stay under about
  twice the live opens' bytes plus a segment. A reader holding a dropped segment
  keeps reading it; its space returns when the last lets go. Frames are at most
  96 MiB; a take's bytes are in records of at most 16 MiB. The constants are not
  settings. A checkpoint's pointers are the built ones, so a compaction that is
  due waits for the first call after the builder has built every step and read
  past the oldest segment; calls after the last step (a file under constant
  writes) never hold it back, since the checkpoint holds them, and only the
  segment they share with dead records waits for the next step. While steps
  trail, the journal grows; once it is twice its due length, the lag gate holds
  every tree's new work until the builder has built every step. A compaction
  that fails before its checkpoint changes nothing and is retried at the next
  call; one that fails while dropping leaves those segments for the next. Its
  fsyncs (every segment the checkpoint covers, the directory, the frames it
  copies forward) stop the actor when they fail, as a leader's do.
- Recovery folds the last checkpoint, the live opens' calls and the held page
  writes it names from the segments before it (kept or carried, each once), and
  every frame after it.
- Every frame carries the durable mark: the last record a finished fsync covered
  when the frame was written. At the first invalid frame, a later valid frame with
  this journal's salt whose mark passes the last valid record means a durable record
  was lost: corruption, and the journal refuses to open. Otherwise the hole is a
  torn tail of records no fsync had covered, saved to `journal/torn` and cut off,
  with every later segment. A journal whose valid records end before the built
  log's point is corruption too: the builder made those records durable before it
  built them.
  Bit rot in the very last frame, or in records not yet durable or built, reads as
  a tear and drops those records; the log line and that file are the evidence.
- The state area is `volume`, `history.git`, `journal/`, `built.log`, `align/`,
  `bases/`, `actor.lock`, `format` and rebuilt private checkout caches, which the workload
  never sees. A create writes an `initializing` marker first and removes it last;
  the next `trees init` starts a cut-short create over. **Earlier state formats
  (format 8 included) do not open**, and are never converted.

## History trails the files

A builder thread makes each step's commits behind the call path, in batches of at
most 256 steps, so a long backlog ends in a built event every batch. For a batch it
makes every record through the last one appended durable (one fsync of the
journal), reads them, replays each step on the trees as built (the same transitions
the actor ran, then the line alignment), and feeds the commits to one
`git fast-import`, each changed file's blob streamed from its map. It installs the batch in this order: the pack, then its index;
the new pack files and `objects/pack` fsynced; the batch's entry in `built.log`
fsynced. That entry moves the built point. Only then are Git's refs, `config` and
the private checkouts copied, the checked-out files' maps rebased onto them
([File maps](#file-maps)), and waiters woken.

- A save is visible to every view at its release, before it is built. Git's refs,
  commits and checkouts show only what is built.
- Takes are decided on built history, off the actor's lock (below, "Takes").
- A commit is dated by its step's record, in `+0000`. A replay after a crash may
  make other objects for steps nothing had seen built.
- A failed batch (`tree HistoryBuildFailed: <e>`) changes nothing reachable and is
  retried at the next step or wait; recording goes on. A batch whose replay
  disagrees with its record (`Disagrees`) fails the same way at every retry: see
  [runbooks/recording-failures.md](runbooks/recording-failures.md). A failed copy
  after the built-log entry is logged (`tree CopyStale`) and redone whole at the next
  batch.
- Dropping the volume builds every record appended, so a clean shutdown leaves
  history built through its last record. After a kill the builder builds what
  was left unbuilt, behind the reopened views.
- No record after a checkpoint names a step the checkpoint covers: a step whose
  pointers were read before a compaction names those values as the oids they
  were built as, so a restart, which builds from the checkpoint, builds every
  later record as an uninterrupted run would.
- `--metrics-output` reports `history`: the built point, the last record appended,
  the oldest unbuilt step's age, the worst tree's lag and the builder's health.
- **The lag gate.** Each tree's lag is the builder's estimated time for its unbuilt
  steps: the larger of their count at a cost per step and the bytes they write
  (the bytes a save or take leaves, plus a constant for the commit and its three
  hashed folder levels, which do not grow with the tree) at a cost per byte, each
  measured on the last batch as if it bore the whole batch. Over 120 s, a call
  that starts new work in that tree (a create, a write-open, `O_TMPFILE`, a link,
  a rename, an unlink, a path truncate) waits for the built event that brings the
  lag under; other trees, writes, flushes and closes do not wait, and the view
  answers other requests meanwhile. One gate thread per volume answers every
  waiting call at the built or failed event that admits it.
  The kernel holds a directory's lock while a create, link, rename or unlink in it
  waits, so a lookup or listing of that directory waits with it. A failed builder
  holds nothing back. Over 30 s,
  `tree HistoryLagging` is logged once; over the limit, `tree HistoryStalled`.
  The levels are constants, not settings.

## Takes

A tree's queue entries are taken one at a time per volume, by the view's worker or
by `take_next`. Each take runs in three parts:

1. Under the actor's lock: pending closes in the tree save first, then the take
   reads its inputs: the keys the tree's write-opens hold, the last step that moved
   the tree's head, and the last record appended.
2. With the lock released: it waits for history built through that record, then
   plans and judges the first entry P1 lets go on that built snapshot. Calls go on
   meanwhile.
3. Under the lock: it lands as a take step, or is refused when a step after the
   decision touched a key it read (`FilesChanged`), a write-open holds a key it
   writes (`Held`), a fast-forward's head moved (`HeadMoved`), or its entry is
   gone (`EntryGone`). A refused take is logged (`tree TakeRefused`) and decided
   again at once, on the history that refused it.

A landed take read nothing of its tree that changed after its decision, so it decides
what a take made in one step at its land would. A sync reads its upstream's head as
of its decision, as a sync taken then would; the upstream's later saves queue later
entries. A fork's entries are planned once per take: each is a sync marker, and P1
takes the first or none. The worker wakes on built and failed events,
never a timer. A take whose files change faster than the builder's lag plus its
decision waits until they stop.

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
- **Large files.** A save costs its own file: the builder's memory does not
  grow with the file (17 MiB for a 1 GiB save in one local run, its history
  built in about 40 s; see [performance.md](performance.md)). Still: the
  state area needs about three times a large file while history builds (its
  journal segments, its blob, its checkout); an edit that is not an append
  runs `git diff`, which holds both versions; a take expands its files' line
  maps line by line and reads the files it merges, through `git cat-file` for
  any over 8 MiB; and a line map keeps every deleted line, so maps of files
  edited often grow with their edits. In one debug-build run on CI, before
  file maps and these line maps, a 100 MiB file took 339 s until both views
  converged; 10,000 one-line saves took 2,561 s.
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
- **The pack syncs** cannot be judged by prefix replay on ext4 and XFS (a later
  fsync carries the pack's directory entry). `ci/trees-object-dir-syncs.sh` proves
  only the order of system calls (pack files and `objects/pack`, then the built-log
  entry), not a crash.
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
