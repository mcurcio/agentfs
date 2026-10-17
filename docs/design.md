# agentfs design

Status: draft 2 (2026-10-12), kept current with the code. Phases 1 to 3
are built; phase 4 is built for several pods on one node (the history
service and fronts, §14.1, and the CSI driver, §14.2); several nodes per
volume are not supported (decided 2026-10-15, §19.1). Forked volumes
(§11.5), the Docker examples, the Helm charts and the example alert
rules are built too. §1.3 gives each phase's and extra's state, and
[performance.md](performance.md) the measured costs and the performance
budget (§18 test 13, §19.2 item 1). Draft 1 was reviewed adversarially:
see [reviews/2026-10-12-adversarial.md](reviews/2026-10-12-adversarial.md)
for the findings and what changed because of them. How the code is
organised: [architecture.md](architecture.md).

## 1. Objective

agentfs records every change an agent makes to a directory, in git history
the agent cannot see.

- **Ordinary files.** Any process, root or not, uses the directory with ordinary
  file calls and never sees git. The files live in a real directory on a backing
  volume.
- **Recording.** agentfs sits in front of that directory as a FUSE filesystem.
  When a file is closed after being changed, or renamed, deleted, linked or
  marked executable, agentfs durably records the new state before the call
  returns. It later turns recorded states into commits.
- **Pushing.** Commits are pushed at regular intervals to one or more git
  remotes. GitHub is one possible remote, not an assumption.
- **Logical projection.** Logically, the branch's history is the sequence of
  states of the directory. Physically, the directory is a normal tree, and the
  history lives outside the mount.
- **Rules.** Path rules use git's own idioms: `.gitignore`, `.gitattributes`,
  `.lfsconfig`, `$GIT_DIR/info/*` and git-config syntax.

### 1.1 Threat model

agentfs protects against an agent's **mistakes**, not a malicious agent. It
exists to:

- show every change an agent made, so mistakes can be found and recovered from;
- sync that history to remotes;
- let infrastructure-as-code (iac) change files with versioned patches instead
  of overwriting whole files.

"Invisible" means no history, journal, state or credential is reachable through
the mount. agentfs does not try to resist a workload that is actively trying to
defeat it.

Forks are the exception (decided 2026-10-15, §19.1 "Fork content"; §11.5):
what a sandbox writes into a fork is treated as potentially
hostile in everything agentfs itself does with it, while agentfs limits
nothing that users' own CI may do with what a fork publishes.

### 1.2 What "every change" means

A change is recorded at each **change boundary** (§3.1), before the call that
ends it returns. Intermediate states between boundaries, such as the bytes of a
file between two writes, are not recorded separately. A few boundaries can only
be recorded after the fact; §3.1 lists them.

### 1.3 Baseline, extras, phases

| Phase | Contents | State |
|---|---|---|
| 1 | The recorder as a native sidecar for one pod: recording, commits, interval push to any remote, path rules, operator-only safety settings, push checks, OpenTelemetry, a supported manual recovery path | Built: the FUSE layer, `agentfs mount`, recovery (§15), the container image and the Kubernetes sidecar, tested end to end in CI (§18) |
| 2 | Git LFS (§7), and the `fsync` class for databases (§8) | Built |
| 3 | The ordered patch queue for iac (§12) | Built |
| 4 | The CSI driver, including volumes mounted by several pods at once (§14) | Built for several pods on one node, one node per volume at a time (§14.1, §14.2); several nodes per volume not supported (decided 2026-10-15, §19.1) |

Built beside the phases, and planned:

| Extra | State |
|---|---|
| Forked volumes: create, finish, publish, deliver, through the CLI and the CSI driver (§11.5) | Built, tested end to end on kind |
| Docker examples, `docker run` and Compose ([docker.md](docker.md)) | Built, tested in the `docker` workflow |
| Helm charts: the CSI driver and the sidecar library ([helm.md](helm.md)) | Built, tested in the `helm` workflow, published to `oci://ghcr.io/mcurcio/charts` |
| Example Prometheus alert rules with runbooks (§13, `deploy/alerts`) | Built, tested in the `alerts` workflow |
| Local-only volumes on node-local disk with a git backend | In design; nothing built |

### 1.4 Not goals

- **Enforcing content rules.** agentfs records; it does not reject writes.
- **Detecting secrets.** agentfs does not scan content (§10).
- **Pushing remote changes into a running workload.** Remote changes enter only
  at mount time.
- **Showing history to the workload.**

## 2. Architecture

```
 workload processes
        │  ordinary file calls
        ▼
 ┌──────────────┐   reads/writes go straight to backing files (FUSE passthrough)
 │ agentfs FUSE │──────────────────────────────────────────────┐
 │   (front)    │  intercepts namespace ops + change boundaries │
 └──────┬───────┘                                              ▼
        │ record (synchronous)                         backing tree (real files)
        ▼
 ┌──────────────┐   async: hash, objects, commits, push    state area
 │   recorder   │─────────────────────────────────────▶  (journal, pending,
 │ (history svc)│                                          git dir, LFS cache)
 └──────────────┘
```

### 2.1 Backing tree

The visible directory is a real directory tree on the backing volume.

- **Reads and writes** go straight to the backing files. On Linux 6.9 and later,
  the kernel's FUSE passthrough means data I/O does not go through the daemon at
  all. fuser exposes this through `BackingId`.
- **Normal behaviour.** Because the files are real:
  - hard links, mmap, `O_APPEND`, sparse files and SQLite behave normally;
  - unlinking or renaming an open file behaves normally;
  - permissions, ownership, timestamps and extended attributes are kept by the
    backing filesystem.
- **What the daemon handles.** It handles lookups, directory operations, renames,
  links and attribute changes, and it sees the change boundaries it must record.
- **Recording is per inode.** A change is attributed to the inode's current
  paths at the moment of recording, not to the path it was opened with.

### 2.2 State area

The state area is a separate directory on the same volume, never visible
through the mount:

```
<state>/
  git/       the history repository (bare): objects, refs, config, info/*
  journal/   durable record log (§3.3)
  pending/   frozen copies of recorded content awaiting commit (§3.2);
             ids carry the writer's slot in their high 16 bits (0 is the
             single process or the history service, 1..1023 a front, §14)
  lfs/       local LFS object cache (§7)
    objects/aa/bb/<oid>  cached objects, git-lfs's layout; pruned only by
                         `agentfs lfs prune`
    tmp/                 objects being written
    incomplete/<oid>     resumable downloads (0600), each under its own flock
    lock                 flock: shared by restores and fetches, exclusive
                         while `agentfs lfs prune` deletes
  meta/      the daemon's own bookkeeping
    lock                 flock held by the one process using the state area (§2.3)
    push/<remote>.json   each remote's push state (§11.2)
    push/<remote>.lock   flock held while pushing that remote
    repairs              paths a mount could not record, for the next one (§3.5)
    patch.json           the patch queue's last run, a cache (§12)
    fork.json            a fork's published mark, written by the finisher
                         only after the fork remote confirmed it (§11.5)
    control/agentfs.sock the running daemon's control socket (§2.3, §15);
                         directory 0700, socket 0600
    stack/mnt            where a mount recovering over a stale one mounts FUSE
                         before stacking it over the mount point (§17.1)
    history/history.sock the history service's socket for fronts (§14);
                         directory 0700, socket 0600
    history/address      the address it serves, for `agentfs status`
    fronts/slot-N.lock   a front's lease of pending-id slot N (flock), holding
                         a holder line
    fronts/slot-N.repairs that slot's repair queue (§3.5)
```

### 2.3 Single writer of history

Exactly one recorder process owns the history repository and its branch. In
phase 1 that is the sidecar's daemon. With several pods (§14) it is a separate
history service per volume, fed by each pod's front. Either way history is a
straight line, and every commit is made by agentfs.

**Enforced by a state-area lock.** Every command that writes the state area
(`mount`, `run`, `serve-history`, `record`, `flush`, `patch apply`,
`fork create`, `fork finish`, `fork abandon`, `init` of an existing state
area, and `push`, `restore` and `revert` in direct mode, when no daemon
answers on the control socket) first takes an exclusive
`flock` on `meta/lock`, before it opens the volume, and holds it until it
exits. The kernel releases it when the process dies, even after `kill -9`,
so a crash never leaves a stale lock.

- The file holds a line describing the holder (`pid=… command=…
  since=<UTC>`), used only in messages; a line left by a dead holder is
  never taken for a held lock.
- A command that finds the lock held exits with status 75 (`EX_TEMPFAIL`)
  and names the holder, unless `--wait-lock <duration>`
  (`AGENTFS_WAIT_LOCK`) lets it wait.
- Every long-running daemon (`mount`, `run`, `serve-history`) serves the
  control socket (`meta/control/agentfs.sock`, §2.2) while it holds the
  lock. Its operations are `push`, `restore` and `revert`; a push runs
  beside a restore, restores and reverts one at a time.
- `agentfs push` first asks the running daemon over that socket: the
  daemon commits everything recorded (but the group of a restore still
  running), wakes each chosen remote's push loop past its interval or
  backoff, and waits up to `--timeout` (default 120s, clamped to 1s..10m)
  for the pushes it started. It prints the same lines as a direct push,
  under a `through the running daemon (pid N)` header, and exits 1 unless
  every chosen remote succeeded in time; a push still running then
  carries on in the background. A remote the daemon does not know is
  refused (exit 1). With nothing listening (or a daemon from before the
  `push` operation) it takes the lock and pushes directly.
- `restore` and `revert` (§15) first try the same socket: a mount serves
  them itself, writing through its own mount. `run` and `serve-history`
  answer that they serve no restores, and the command exits 75. With
  nothing listening they take the lock and write the backing tree
  directly; while `record`, `flush` or a direct `push` holds the lock they
  exit 75.
- `agentfs init` takes the lock when the state area already exists. It is
  safe to re-run, as a pod's init container does at every start: for the
  same volume (and branch, if given) it exits 0 and only re-creates
  missing directories and, with `--include`, sets the config includes;
  for another volume or branch it exits 1 and changes nothing, so a PVC
  mix-up never repurposes a history. (It used to overwrite
  `agentfs.volume`.)
- `agentfs status` takes no lock and changes nothing: it reads history and
  the journal without recovery (it used to run recovery, which could
  truncate the journal or prune frozen copies under a running daemon). It
  reports the lock's holder. A torn journal tail is reported, and trimmed by
  the next command that takes the lock.
- `status` probes the lock by taking a shared lock and dropping it at once;
  taking the lock therefore retries for at least 250 ms even with no
  `--wait-lock`, so a probe cannot make it fail.
- The lock is taken only on a filesystem whose locks keep other nodes
  out (`agentfs_core::shared_fs`): a state area on NFS older than 4.1,
  mounted `nolock`, or with `local_lock` other than `none` (read from
  `/proc/self/mountinfo`) is refused, because there `flock` excludes only
  this node's processes and a second history service could run on
  another (§14.2). Other filesystems are not checked.
- The per-remote push locks (§11.2) stay; they now only matter within one
  process.
- With several mounts of one volume (§14), `agentfs serve-history` is the
  lock holder and the only writer of the journal and the branch. Fronts
  (`mount --history-service`) take no lock: they write only `pending/`
  (in their own slot) and `meta/fronts/`. While the service runs, every
  single-process command exits 75 as above but `push`, which the service
  and `restore` and `revert`, which the service serves live (§15);
  fronts serve no control socket.
- Under the CSI driver (§14.2) the node plugin starts the volume's
  `serve-history` at NodeStage, so the service holds the lock while the
  volume is staged on a node. The controller takes the lock only briefly,
  to create or delete a volume; DeleteVolume of a staged volume fails with
  `FailedPrecondition`.

## 3. Recording

### 3.1 Change boundaries

| Event (FUSE op) | Recorded | Before return? |
|---|---|---|
| `close` (FLUSH) of a file changed since its last record | its content and executable bit | yes |
| `rename` | source removed, destination added (a directory: one move, reconciled before commit) | yes |
| `unlink`, `rmdir` | removal | yes |
| `link`, `symlink` | new path | yes |
| `setattr` changing size (`truncate`) or the executable bit | new content or mode | yes |
| `fsync` on a path marked `agentfs.commit=fsync` | its whole group (§8), coalesced | yes, at most once per interval |
| the trailing timer of an `fsync`-class group | its whole group (§8) | **no**: at most one `agentfs.coalesce` late |
| RELEASE (all handles and mappings gone) | content, if changed after the last FLUSH or that record's stamp was racy (below) | **no**: backstop for mmap and late writes |
| periodic sweep | content of files still open and changed | **no**: backstop for files never closed |

Rules that make this precise:

- **FLUSH records only if the inode changed.** FLUSH is sent on every `close()`
  of every duplicated descriptor, for example each forked child that inherited
  stdout. Unchanged inodes are skipped, so a build writing to a log does not
  record the log thousands of times. A changed one is still recorded at every
  close.
- **RELEASE is asynchronous.** Its result never reaches the caller, which is why
  writes through shared memory mappings, or after the last close, are recorded
  late.
- **Empty directories** are not recorded, because git cannot represent them.
  `mkdir` alone is not a boundary.

**How the FUSE layer implements this** (`agentfs-fuse`):

- **Dirty detection.** With passthrough the daemon never sees writes, so
  "changed" is read from the inode. An inode is *armed* by a write-open, a
  write the daemon serves, or a size change; FLUSH `fstat`s only armed
  inodes and records when the stamp (device, inode, size, mtime, ctime,
  mode) differs from the one taken just before the last freeze. Timestamps
  come from a coarse clock, so git's racy-timestamp rule applies: if the
  last record's stamp is within twice the coarse clock's resolution of the
  record time, the next boundary looks at it again even if the stamp
  did not move. RELEASE follows the last close at once, so a file
  written and closed within that window (nearly every file a program
  writes and closes) would be recorded twice: RELEASE is the only
  backstop for a write through a shared mapping in the same clock tick
  as the FLUSH freeze, which leaves the stamp unchanged. Instead, a racy
  record keeps its frozen copy open, and a boundary that would record
  only because of the racy rule (stamp unchanged, nothing forcing it)
  compares the file's bytes with that copy and records nothing if they
  are equal, the file still has the one name the copy was recorded at,
  and the copy is still the last state journaled at that path (no later
  write, symlink, removal or rename there, above or below it, by
  anything). Whoever journals keeps the frozen copy of the last write at
  each recently written path for 120 s to answer that; a front asks its
  history service (`StillLast`, §14). Any doubt (an unreadable file, a
  forgotten path, an older service) records, as before. The comparison
  reads both files, mostly from the page cache, which is far cheaper
  than a second freeze and a second hash and compression by the commit
  loop (`agentfs.fuse.racy_rechecks{outcome}` counts both outcomes). A
  boundary that proves the bytes unchanged recomputes the racy flag as a
  record would, as of a time taken before the comparison: once the
  window has passed, later boundaries trust the stamp, stop comparing and
  release the pinned copy. A
  created file is recorded at its first FLUSH even when empty (`touch`);
  a write-open of an existing file takes its stamp at open, so opening
  and closing without a change records nothing.
- **`O_TRUNC`.** `FUSE_ATOMIC_O_TRUNC` is negotiated, so a shell `>` is one
  open with `O_TRUNC` and one record at close, even when nothing is written
  after the truncation. `truncate`/`ftruncate` remain setattr boundaries.
- **What is frozen.** FLUSH, RELEASE and the sweep hand the recorder
  `/proc/self/fd/<n>` of the handle's own descriptor, so the frozen content
  is exactly that inode's, whatever renames happened. Other boundaries open
  the path with `openat2` first and do the same.
- **Current paths.** An inode's paths are the names the mount has seen for
  it (looked up, created or linked), resolved through the parent chain at
  record time. A hard link made before the mount and never looked up is not
  known, so a change through another name is not recorded at it.
- **Renames.** A file rename records the source's removal and the
  destination's content in one record. A directory rename is journaled as
  one *move* change (`{op: "move", from, to}`), under the mount's name lock
  so no other boundary can name a path beneath either directory until it
  is durable: history carries whatever it holds beneath the source to the
  destination, replacing what was there, in O(1) in the synchronous step
  (the tracked set is a tree of path components). A move is journaled only
  if something is tracked beneath either name or the rules do not exclude
  everything beneath the destination, so a rename inside `node_modules`
  records nothing.
  Each move leaves a *reconcile duty*: before each commit pass the commit
  loop walks the destination in the backing tree (never inside a `.git`
  directory, never through a symlink: every directory opened with
  `openat2` beneath the root, every entry examined by its one name in its
  opened directory) and records, in records of at most
  4096 changes as the move's caller and pod, what recording every file
  there would have decided differently: moved files the rules exclude at
  their new path are removed (renamed into an ignored directory or a
  nested repository), files there that were not tracked and that the
  rules record are added (an ignored directory renamed to where nothing
  ignores it), tracked paths no longer in the tree are removed, and a
  tracked file changed within 60 s of the move whose freeze stamp is not
  the one last journaled at its path is recorded as it is (another
  front's write that raced the rename and was skipped as gone, or a change
  made outside the mount). A moved file the path rules refuse at its new
  path (a directory renamed to `.gitmodules` or `x/.gitattributes`, or
  onto a prefix that takes a path past 4096 bytes) or that is over its
  `agentfs.maxSize` there is removed with a `path_refused` alarm, as
  recording it there would have refused it; history leaves such entries
  out of the move itself too, so no commit holds them even before the
  reconciliation. Every file the walk found is opened again for its
  record with `openat2` beneath the root (`RESOLVE_NO_SYMLINKS`), pinned
  by an `O_PATH` descriptor and frozen from it only if it is a regular
  file: a component the workload swaps for a symlink, or a leaf swapped
  for a FIFO or device, is skipped, never followed or opened (the daemon
  may run as root). A file that is gone by then is skipped too (whoever
  removed it records that), not a failure. Records are committed only
  below the oldest move not yet reconciled, and a move's commit includes
  the records that reconcile it when they are the same pod's, so history never shows a
  moved tree without its corrections. Duties are rebuilt at open from the
  uncommitted journal, so a crash before reconciling redoes it (records
  hold absolute state). A duty that fails (an I/O error) is retried
  after a backoff from 0.5 s doubling to 8 s; once it has failed 5 times
  over at least 30 s it raises `reconcile_failed` and stops holding
  commits back; the move is committed as journaled
  ([runbook](runbooks/reconcile-failed.md)). A move into a git directory
  (§5) commits as the source's removal. Without the backing tree
  (`agentfs flush` offline) moves are committed as journaled. Measured
  (§18 test 13): renaming a directory of 100,000 recorded files takes 23 to 24 ms through a mount and
  makes one record, committed with its reconciliation in 3.4 to 7.9 s.
  Through a front (§14) the move is a request to the history service
  made while the name lock is held, so while the service is slow every
  lookup, open and readdir on that mount waits with the rename, for at
  most `--history-timeout` (an unreachable service fails at once); the
  rename then fails with `EIO` and its paths are queued for repair.
  `RENAME_EXCHANGE` of directories, a rename to or from a path the mount
  never records (inside `.git`, an invalid name), and a front whose
  history service predates the `move` feature still record both names'
  removal and then every regular file and symlink under the destination;
  past 4096 files it is several consecutive records, the first carrying
  the removals, holding one descriptor per file while each is made (the
  daemon raises its open-file limit at start). Renaming one hard link over
  another of the same inode changes nothing and records nothing.
- **`mknod` of a regular file** has no FLUSH to follow, so it is recorded at
  once (trigger `close`). FIFOs, sockets and devices are never recorded.
- **RELEASE** of the last handle that could write records the inode if it
  changed after its last record (trigger `release`).
- **The sweep** runs every `agentfs.sweepInterval` (default 30 s, at least
  1 s): it records inodes open for writing whose stamp changed (trigger
  `sweep`) and retries the repair queue (§3.5). At unmount a final sweep
  records every armed inode with any handle still open.
- **The `fsync` class** (§8): paths with `agentfs.commit=fsync` are opened
  without passthrough and recorded as whole groups, at `fsync` (rate
  limited), by the trailing timer, at close and at unmount. Their own
  renames, unlinks, links and size changes are folded into the group's
  next capture rather than recorded alone; the sweep skips them.

### 3.2 The synchronous step

The work done before the call returns must not depend on file size:

1. **Freeze** the inode's current content into `pending/`, as a reflink where the
   backing filesystem supports it (XFS, btrfs), otherwise as a copy.
   A file already over the smallest remote blob-size limit (§11.3) by its
   size on disk is refused before it is copied, unless it would be stored
   as LFS (§7); the frozen copy is checked again, and its store decided
   and journaled, since the file can change in between.
2. **Journal**: append a framed, checksummed entry with sequence number, inode,
   current paths, pending id, mode, trigger, caller UID, GID and PID, and time.
3. **Group commit**: one `fsync` of the journal covers every entry that arrived
   while the previous `fsync` was in progress.
4. Return.

Hashing, compression, object writes and commits happen afterwards (§3.4):
LFS content is hashed into the object cache when it is committed, and
uploaded when it is pushed (§7). Without reflink support, step 1 costs a
copy of the file, so recorded volumes that hold large files should use
XFS. Measured by the benchmark (§18 test 13, [performance.md](performance.md);
CI runners, medians): the step for a small file takes 1.3 ms on XFS and
1.9 ms on ext4, mostly the journal `fsync`, and 1.8 to 2.4 ms through
FUSE. On XFS it does not depend on size: a 16 MiB freeze takes 0.44 ms and
closing a clean 1 GiB file 3 ms (2.6 s with its pages still dirty, as
the reflink waits for writeback). On ext4 the copy takes 44 ms for 16 MiB
and 2.9 s for a clean 1 GiB file. A file written and closed at once is
recorded once: RELEASE compares it with the copy FLUSH froze rather than
freezing and journaling it again (§3.1), so 100,000 new files make
100,004 to 100,009 records on CI where they made about 200,000, a
clone of 4,524 files 4,524 where it made about 9,046, and a 1 GiB file
just written is hashed and compressed once. The performance budget is
§19.2 item 1.

**Through a front (§14).** A front mount splits the step with the history
service. The front decides the path rules (asking the service with a
cheap, unsynced `Tracked` query only when a write's decision depends on
whether the path is tracked: an ignored or nested path), runs the
record-time checks and freezes into its own pending slot, because only it
holds the inode's descriptor. It then sends the draft in a `Record`
request. The service takes the path claims, decides again against the
authoritative tracked set, checks that the frozen copies exist in the
sender's slot, stamps the front's origin, journals with group commit and
`fsync`, and replies. The front returns only after that reply, so a
returned call is as durable as in single-process mode.

### 3.3 Journal and replay

- Entries hold **absolute state** (path → pending content and mode, or a
  removal), never deltas, so replaying an entry twice gives the same result.
  The one exception is a directory move (§3.1), which is why a commit
  skips entries at or below the head's `Agentfs-Seq`.
- **Torn entries.** A torn final entry, which fails its checksum, is discarded.
  Its call never returned, so nothing was promised.
- **Unknown entries.** An intact entry (its checksum holds) that does not
  parse was written by a newer agentfs, for example a `move`: opening or
  reading the journal fails with an error naming `agentfs flush`, and
  nothing is discarded. Run `agentfs flush` with the newer version before
  downgrading.
- **Commits record the last journal sequence number they include,** as a trailer
  (`Agentfs-Seq: <n>`).
- **Truncation.** The journal and `pending/` are truncated only after the commit
  and branch ref are durable.
- **libgit2 fsync** is enabled for objects and refs (`GIT_OPT_ENABLE_FSYNC_GITDIR`,
  set through libgit2-sys): each loose object, its fan-out directory
  (`objects/xx`) and each ref file are fsynced as they are written.
  libgit2 does not fsync `objects/` when it creates a fan-out directory,
  nor the directories above a new ref, so agentfs does.
- **Commit fsync order.** Blobs, trees and the commit object, then
  `objects/` (so every new fan-out directory entry is durable), then the
  branch ref (with its own file and directory), then the directories
  above the ref, then the journal truncation and the removal of the
  frozen copies. Objects are durable before any ref names them, and
  nothing a commit needs is dropped before the ref is durable, so a power
  cut at any point leaves either the old head with its records still
  journaled or the new head with everything it names.
- **After a crash,** the recorder resumes from the branch head's `Agentfs-Seq`
  and replays later entries.
- **Entries without their copies.** A record whose journal write failed
  after the bytes reached the journal (an error after the write, or a
  power cut before the failed batch's truncation) has its frozen copies
  removed, and its call returned an error, so nothing was promised. A
  commit drops such an entry whole when any of its frozen copies is
  missing from `pending/` and raises `RecordFailed` naming it, rather than
  failing every later commit on the missing file. Found by the crash
  harness (§18, `record.after_journal`).
- **Failed truncation.** When rewriting the journal fails after the new
  file replaced the old one, the journal keeps appending to the new file,
  never to the unlinked old one, so later records stay durable.
- **Stale ref locks.** A ref update writes `<ref>.lock` and renames it
  over the ref; a crash or power cut in between leaves the lock, and
  libgit2 then refuses every later update of that ref. Opening the volume
  (always under the state-area lock, §2.3, so no other process writes the
  history) removes `HEAD.lock`, `packed-refs.lock` and every
  `refs/**/*.lock`, with a warning naming each. Found by the power-loss
  workflow (§18): a cut in the branch update left
  `refs/heads/agentfs/<volume>.lock` and no commit was possible after it.

### 3.4 Commits

- A commit builder turns journal entries into commits a few milliseconds after
  they are recorded. The service's commit loop wakes every `commitWindow` and
  commits each group that can no longer grow: one whose first entry is more
  than `commitWindow` old, or is in the future after the clock stepped back.
  A failed commit raises `CommitFailed` (§13), and the loop backs off (doubling,
  up to 30 s) while the entries stay journaled. On shutdown everything left is
  committed, including the open group.
- **Repack.** Every `agentfs.repackInterval` (default 1h) the commit loop
  runs `git repack --geometric=2` between commits, then refreshes its object
  database (§8). A failure raises `RepackFailed` (§13).
- **A commit whose tree would not change is not made**: its records are
  consumed without one, so a capture of an unchanged database costs a
  copy, not a commit. The exceptions are marker commits with the head's
  tree: a patch that changes nothing, or a duplicate of a patch already
  applied, so the branch records that it was consumed (§12); and a fork's
  finish commit, the last commit of its history (§11.5).
- **Atomic records** (an `fsync`-class group, §8) are committed whole: if
  the commit-time checks refuse any change in one, the whole record is
  dropped, with a `PathRefused` alarm for the refused path.
- **Grouping only affects how history reads,** not whether a change is kept.
  Entries close together in time are grouped. Inside a nested repository with
  `nested = working-tree` (§5), its `index.lock` bounds a group, with a timeout
  for stale locks.
- **Commit metadata** is in §9.
- **Every path is checked twice** against git's fsck rules and the configured
  limits of the remotes (§11.3):
  - **at record time**, in the synchronous step. This is the gate: a path that
    fails is never journaled or tracked, its frozen copy is removed at once,
    and the rest of the record carries on;
  - **again at commit time**, as a backstop for records journaled before the
    limits changed, for example records recovered after a remount with a
    stricter remote. A refused change is dropped from its commit.

  A refused path stays in the backing tree, its last recorded content (if
  any) stays in history, and an alarm is raised (`PathRefused`, §13).
- **The checks**, beyond the path rules of §5:

  | Rule | Threshold | git fsck id |
  |---|---|---|
  | Path length | over 4096 bytes | none: agentfs's own (`PATH_MAX` at checkout) |
  | Symlinked `.gitmodules`, `.gitattributes`, `.gitignore`, `.mailmap`, in any NTFS or HFS+ spelling, including 8.3 short names (`gitmod~1`, `gi7eba~1`) | | `gitmodulesSymlink`, `gitattributesSymlink`, `gitignoreSymlink`, `mailmapSymlink` |
  | A path beneath a directory named `.gitmodules` or `.gitattributes`, in the same spellings (git requires both to be blobs) | | `gitmodulesBlob`, `gitattributesBlob` |
  | Symlink target length | over 4096 bytes | `symlinkTargetLength` |
  | Symlink target with a `.git` component, in any spelling | | `symlinkPointsToGitDir` |
  | `.gitattributes` size | over 100 MiB | `gitattributesLarge` |
  | `.gitattributes` line length | 2048 bytes or more (git's test is `>=`) | `gitattributesLineLength` |
  | `.gitmodules` (any spelling, any directory) that git's config parser cannot read | | `gitmodulesParse` |
  | `.gitmodules` submodule name empty or with a `..` component (split on `/` and `\`) | | `gitmodulesName` |
  | `.gitmodules` `url` that is an option (`-…`), holds a decoded newline, climbs out of its root with `../` followed by `:` or `/`, or is an `http(s)`/`ftp(s)` URL git refuses | | `gitmodulesUrl` |
  | `.gitmodules` `path` starting with `-` | | `gitmodulesPath` |
  | `.gitmodules` `update` starting with `!` (a command) | | `gitmodulesUpdate` |
  | `.gitmodules` size | over 512 MiB (git's `core.bigFileThreshold`) | `gitmodulesLarge` |
  | Blob size | over the smallest `remote.<name>.agentfsMaxBlobSize`, unless stored as LFS (§7) | none: remote policy |
  | LFS object size | over the smallest `remote.<name>.agentfsMaxLfsObjectSize`, when stored as LFS (§7); reason `lfs_object_too_large` | none: remote policy |

  There is one history for every remote, so the limits that apply are the
  smallest across remotes. Changing a remote's limits takes a remount. A path
  refused at commit time stays in the in-memory tracked set (§4) until the
  next open.
- **The synchronous step stays bounded.** Size checks are a `stat` and
  always run at record time. Content scans (the `.gitattributes` line
  length, the `.gitmodules` rules) run at record time only for files up to
  1 MiB, the bound tree rule files already have (§4.1). A larger file is
  scanned in full at commit time, where a refusal drops the change and
  raises `PathRefused` with stage `commit`, like any commit-time refusal.
- **`.gitmodules` checks** are a port of git's config parser and of the
  submodule checks in git's fsck (`agentfs_core::gitmodules`), tested
  against the `git` binary: the parser against `git config --list`, the
  checks against `git fsck`. git changed its check of `http(s)` and
  `ftp(s)` submodule URLs in 2.40 (from `credential_from_url` to
  `url_normalize`); remotes run either, so agentfs refuses what either
  version refuses. Entries before a parse error are still checked, as in
  git.
- `symlinkPointsToGitDir` and `gitmodulesParse` are only a warning or
  info in default git. agentfs always refuses them: a workload symlink
  into a nested repository's `.git` is not recorded, and history stays
  acceptable to remotes with stricter fsck settings.

### 3.5 When recording fails

If the synchronous step fails (for example, the state area is full):

- the call returns an error (`EIO` from FLUSH, or the operation's own error);
- the inode stays marked as changed, so the next boundary records it;
- agentfs raises an alarm.

Many programs ignore errors from `close`, so the alarm, not the error code, is
what people will see.

**Namespace operations.** A rename, unlink, rmdir, link, symlink or setattr
has already changed the backing tree when its record fails, so "the inode
stays marked" is not enough. It returns `EIO` and puts the affected paths
on a **repair queue**. The sweep (every `sweepInterval`, and at unmount)
re-derives each queued path's absolute state from the backing tree (a
file's content, a symlink, a directory's files, or a removal) and records
it with trigger `sweep`. Because records hold absolute state, a repair is
idempotent and cannot undo a later change. Until it succeeds, history
differs from the tree at those paths; the `RecordFailed` alarm says so. A
RELEASE whose record fails queues its paths too, since no handle is left to
record from.

The queue outlives the daemon. It is kept in `meta/repairs` in the state
area (one line per path: the caller's uid, gid and pid and the path in hex;
a front's file also has `@origin <JSON>` lines naming the pod the entries
after it are committed as, below),
rewritten whenever it changes (a temporary file, fsynced, renamed over
it), and the next `agentfs mount` repairs what it lists from the backing
tree before it serves any request. A crash between the write and the
rename leaves the new queue in `repairs.tmp`; the next mount loads its
complete lines as well, since a repair is idempotent (found by the
`repairs.before_rename` failpoint, §18). A repair made
from an older entry never drops one queued again meanwhile. Paths still
queued at unmount are listed on exit with where they are saved; if the file
cannot be written either (the state area is full or unwritable), the exit
message lists them and says that history differs from the tree there until
each path is changed again.

**Through a front (§14).** When the history service is down, hung (no
reply within `--history-timeout`) or refuses a request, the boundary fails
the same way: `EIO` from FLUSH (or the operation's own error with a
repair entry), `RecordFailed`, and `HistoryServiceUnavailable` once per
outage (`HistoryServiceRestored` when a request succeeds again). A
front's repair queue is `meta/fronts/slot-N.repairs`, for its slot, and
its sweep retries it, so history catches up once the service is back. The
file keeps the pod each entry is for: a front writes its own pod before
its entries, and a front that inherits the file from a dead predecessor
in the slot records that pod's entries as that pod's (a `sweep` record
naming the origin, which the service accepts only for sweep records
whose committer it can render, §14.1). A
draft is never sent twice: a request that timed out may still have been
journaled, which is harmless because records hold absolute state, while a
resend could journal one frozen copy twice.

## 4. Path rules

agentfs follows git's model: **ignore rules decide only whether new
(untracked) paths are recorded. A tracked path stays recorded until it is
deleted, even if a rule later matches it.** Rules never decide where data
lives; all data lives in the backing tree.

As with git's index, a path is **tracked** if it is in the branch head or has
been recorded and is awaiting commit, so this holds during commit lag and
after crash recovery. A removal is recorded only if something is tracked at
or below the path; removing a path that was never recorded is not journaled.
A record holds its paths, and every path above or below them, from the
decision until the tracked set is updated, so a removal racing an in-flight
first write of the same path waits for it and is journaled after it.
Records for unrelated paths do not wait for each other.

### 4.1 Where rules come from

| Concern | Tracked (in the tree; edits are recorded) | Operator (in the git dir; iac sets them; invisible) |
|---|---|---|
| Ignore | `.gitignore` in any directory | `$GIT_DIR/info/exclude` |
| Attributes | `.gitattributes` in any directory | `$GIT_DIR/info/attributes` |
| LFS endpoint | `.lfsconfig` | `$GIT_DIR/config` |
| agentfs settings | none | `[agentfs]` in `$GIT_DIR/config` |

**Safety settings are operator-only.**

- `agentfs.*` attributes are read only from `info/attributes`.
- LFS endpoints and credentials are read only from operator config (§7).
  A tracked `.lfsconfig` is content only: agentfs never reads it, though
  it may steer the git-lfs of whoever clones the history.
- Tracked `.gitignore`, `.gitattributes` and `.lfsconfig` files are recorded as
  content. They take part in rules only if `respectTreeRules = true`, and even
  then they cannot set `agentfs.*` attributes or LFS endpoints.

The default is `false`, so an agent's edit to a tracked rule file cannot turn
recording off or redirect uploads.

Built-in system- and user-level git attribute and exclude files are never
consulted. Only the files above count.

One rule is built in: an NFS client's silly-rename name (`.nfs` and 24
hex digits, what a Linux client renames a file removed while open to, and
removes at its last close) is never recorded, tracked or not
(`SkipReason::NfsSillyRename`). It stays visible through the mount, and a
restore never writes over it (it is never in history). Without the rule a
backing tree on NFS (§14.2) would record the client's own bookkeeping.

**How the sources combine.** These are deliberate departures from git, so
that operator rules always apply:

- Operator and tree ignore rules are evaluated as separate layers, and either
  one excludes a path. In git a `.gitignore` takes precedence over
  `info/exclude`, so a tree `!pattern` could re-include an operator exclude;
  in agentfs it cannot.
- Within each layer git's own rules hold: the last matching pattern wins, a
  deeper `.gitignore` wins over a shallower one, and nothing inside an
  excluded directory can be re-included.
- Tree attribute macros are kept apart from operator ones, so a tree
  `[attr]` line cannot change what an operator line means. `agentfs.*` values
  from tree files are dropped after macro expansion. Macros are honoured only
  in `info/attributes` and the root `.gitattributes`, as in git.
- Matching is always case-sensitive; `core.ignoreCase` is not consulted.
- Tree rule files are read only if they are regular files, never through a
  symlink (as git does since 2.32), and a file over 1 MiB is ignored (git
  ignores oversized attribute files), so the synchronous step stays bounded.
- `info/exclude` and `info/attributes` are read when the volume is opened.
  Changing them takes a remount, which fits iac setting them.

### 4.2 agentfs attributes (operator `info/attributes`)

| Attribute | Values | Default | Meaning |
|---|---|---|---|
| `agentfs.commit` | `close`, `fsync` | `close` | Boundary class (§3.1, §8) |
| `agentfs.coalesce` | duration, at least `1s` | `30s` | Minimum interval between `fsync` records for one group, and the trailing timer's delay (§8) |
| `agentfs.group` | name | unset (a group of one) | Paths with the same group *and the same stem* are recorded together (§8) |
| `agentfs.maxSize` | size (`512k`, `100m`, `2g`) | unset | Content larger is refused (`max_size`), whatever the store (§8) |
| `agentfs.store` | `blob`, `lfs` | §7 | Force ordinary git object or LFS; `blob` does not override `filter=lfs` |
| `-agentfs` | | | Never record this path |

- `-agentfs` stops writes to a path being recorded even when the path is
  already tracked. A removal of a tracked path is still recorded, since it
  carries no content and keeps history consistent with the tree. History is
  never rewritten (§10).
- Attribute patterns follow git: a directory pattern such as `secrets/` does
  not cover the directory's contents. Write `secrets/** -agentfs`.
- `agentfs.commit`, `agentfs.coalesce`, `agentfs.group` and `agentfs.maxSize`
  count only in the operator's `info/attributes`, never in a tracked
  `.gitattributes`. The values of the first, second and fourth are
  checked when the volume opens: a bad one (`agentfs.commit=sometimes`,
  `agentfs.coalesce=100ms`) fails the open with the line number, so a typo
  cannot silently turn the class off.

### 4.3 Settings

```ini
[agentfs]
    respectTreeRules = false   # tracked .gitignore/.gitattributes are content only
    nested = exclude           # §5
    commitWindow = 200ms       # group journal entries this close in time
    pushInterval = 60s         # §11
    sweepInterval = 30s        # §3.1: backstop for files left open; min 1s
    repackInterval = 1h        # §8: git repack; 0 or off disables; min 1m
    lfsThreshold = 8m          # §7; unset by default (no size-based LFS)

[lfs]                           # optional, as in git-lfs (§7)
    url = https://lfs.example.com/acme/agent-history   # every remote's endpoint

[remote "origin"]               # one section per push remote (§11)
    url = https://github.com/acme/agent-history.git
    pushurl = …                 # optional; wins over url, as in git
    agentfsMaxBlobSize = 50m    # optional; §3.4, §11.3
    agentfsMaxPushSize = 1g     # optional; §11.3
    agentfsCredentials = /run/secrets/agentfs/origin   # optional; §11.1
    lfsurl = …                  # optional; the LFS endpoint, unless lfs.url (§7)
    agentfsLfsCredentials = /run/secrets/agentfs/origin-lfs   # optional; §7

[remote "backup"]               # an SSH remote (§11.1)
    url = ssh://git@git.example.com/acme/agent-history.git
    agentfsSshKey = /run/secrets/agentfs/backup/ssh-privatekey   # this or agentfsSshAgent
    agentfsSshKeyPassphrase = /run/secrets/agentfs/backup/passphrase   # if the key is encrypted
    agentfsKnownHosts = /run/secrets/agentfs/backup/known_hosts  # required for SSH
```

Every key is described in [configuration.md](configuration.md).

Settings and remotes may also come from operator-owned fragments named by
`include.path` in `$GIT_DIR/config` (a Kubernetes ConfigMap, §17.1). They
are read as part of the repository's own config, and checked more strictly
than git does: each include must be an absolute path to a readable file (a
missing include fails the volume instead of being skipped), must not set
`agentfs.volume` or `agentfs.branch`, and must not be under the backing
tree or the mount point (a mount preflight error, like credentials);
`includeIf` is refused.

### 4.4 Implementation note

Rules are evaluated by agentfs over the pure-Rust `gix-ignore` and
`gix-attributes` parsers, fed only with buffers agentfs reads itself:
`info/exclude`, `info/attributes`, and the tree's `.gitignore` and
`.gitattributes`. Nothing global can leak in.

Draft 2 proposed pointing libgit2 at the backing tree first. Checking
libgit2 1.9.7's source ruled that out:

1. `git_ignore__for_path` always loads every working-tree `.gitignore` on the
   path, with no way to turn that off, so `respectTreeRules = false` could
   not be honoured.
2. It merges every source by git's precedence (a `.gitignore` beats
   `info/exclude`) and cannot report which file an attribute came from, so
   tree files could neither be kept from re-including operator excludes nor
   from setting `agentfs.*`.
3. Its attribute cache takes `core.excludesFile` and `core.attributesFile`
   from the merged system, global and XDG config, falling back to the XDG
   defaults. Disabling that needs process-wide search-path overrides.
4. `git2::Repository` is not `Sync`, so every check from the FUSE threads
   would be serialised behind a lock.

Nested repositories (§5) are detected by probing the backing tree directly,
not by opening them with libgit2, which would read global config.

## 5. Nested repositories

**Detection.** A directory is a nested repository if it contains:

- a `.git` directory;
- a `.git` file or symlink that points to a git directory inside the tree; or
- a git directory itself: the layout of a bare repository (`HEAD`, `objects/`
  and `refs/`), or any mark of one still being created (below).

The last case covers `git clone --bare`, `git init --bare` and tool-managed
stores such as Hermes' `checkpoints/store`.

**Default (`nested = exclude`).** A nested repository's directory is not
recorded: neither its working files nor its git directory.

**Option (`nested = working-tree`).** Working files are recorded, the git
directory is not, and the inner `index.lock` bounds commit groups (§3.4).

**Rules that prevent surprises:**

- **The mount root is never treated as nested.** `git init` at the root creates
  an ordinary directory named `.git`, which agentfs does not record.
- **Tracked paths stay tracked** (§4). If `git init` runs in a directory whose
  files are already recorded, those files keep being recorded. Only new paths
  under it are excluded. Nothing is deleted from history because of nesting.
- **Paths git refuses** are not recorded. A path with a component that is
  exactly `.git` is skipped silently when that `.git` is git's own data: a
  directory that is (or is becoming, by the markers below) a git directory,
  as after `git init` at the root or in a clone, or a gitfile or `.git`
  symlink pointing at a git directory in the tree. A `git clone` on the
  mount would otherwise raise an alarm per object. Any other `.git` (an
  ordinary directory or file someone named `.git`) raises `PathRefused`
  once per such `.git` per mount, so its contents never leave history
  without a trace. A git directory written by a custom template whose first
  files are none of the markers can raise that alarm once. Every other path
  git refuses raises an alarm (`PathRefused`, §13): `.git` look-alikes in their
  NTFS and HFS spellings, such as `.GIT`, `git~1` and `.git.`, names that
  are not UTF-8, and the checks of §3.4. A refused path is reported per
  path; it never fails the rest of its record, and the operation itself
  still succeeds in the backing tree.

**Detection details.**

- Only the proper ancestor directories of a path are examined, never the
  mount root and never the path itself.
- **Repositories under construction.** `git init --bare` writes
  `description`, `hooks/`, `info/exclude`, `refs/`, `HEAD` and `config`
  before `objects/` exists, and each file is a change boundary. Because
  tracked paths stay tracked, anything recorded then would be recorded
  forever, including later `config` edits that may carry credentials in a
  remote URL. So any one of these marks a git directory: a `HEAD` or
  `HEAD.lock` whose first line is a symbolic ref (`ref: refs/…`) or an object
  id; a `config` or `config.lock` whose `[core]` section sets
  `repositoryformatversion`; or any file of git's default template: its
  `description`, its `info/exclude`, or one of its sample hooks
  (`hooks/<name>.sample` starting with `#!`). git copies the template in
  directory order, which differs between filesystems, so any of these may be
  written first: the FUSE tests caught `git clone --bare` recording its
  sample hooks before `description` existed.
- **Commit-time recheck.** A custom template's files written before `HEAD`
  match none of these marks, so the recorder journals them. When their
  records are committed, every write or symlink at a path new to history
  is checked again: if a proper ancestor directory has become a git
  directory (a bare layout) meanwhile, the change is dropped silently, like
  git's own data, and counted on the commit span
  (`agentfs.git_dir_dropped`). Paths already in history and removals are
  never dropped, and working trees (`.git` beneath) are not affected. The
  recheck needs the real backing tree, so it runs for `mount`, `record`
  and `restore`, and for `flush` and `run` only with an explicit `--root`.
  Two limits remain: a custom template's files committed before `HEAD`
  appears (a commit window of 200 ms, by default, between them) stay in
  history, and a new file recorded into an ordinary directory that becomes
  a bare repository within the commit window (`git init --bare` into a
  non-empty directory) is dropped. The in-memory tracked set keeps a
  dropped path until the next open, as it does a refused one.
- **`.git` files and symlinks.** A `.git` file counts only if its first line is
  `gitdir: <path>`; a `.git` symlink is read, never followed. Either target
  must be a git directory inside the backing tree: `HEAD` plus `objects/` and
  `refs/`, or `HEAD` plus `commondir` (a linked worktree), or a linked
  worktree's admin dir `<git dir>/worktrees/<name>` holding `gitdir` (`git
  worktree add` writes the `.git` file before its `HEAD`). Targets are paths
  in the workload's view of the mount, so they are never resolved on the
  host:
  - a relative target is resolved lexically against the directory holding
    `.git`, and is ignored if it climbs out of the tree;
  - an absolute target (what `git worktree add` writes by default) is a
    path in the workload's view of the mount, mapped exactly through the
    *views* agentfs knows: the mount point, every other mount of the same
    FUSE filesystem in the daemon's mount namespace (bind mounts, at the
    tree directory each shows), the paths given with `--workload-mount`
    (`PATH` or `PATH=DIR` for a `subPath`; a container's `mountPath`,
    which only the pod spec knows), and, at a history service, the views
    each front sends in its hello (`FrontInfo.views`). A CSI volume gets
    its fronts' `--workload-mount` from the `workloadMounts` StorageClass
    parameter or volume attribute (docs/csi.md §5); the node plugin never
    learns a container's `mountPath` otherwise. Under the deepest
    matching view, `/workspace/main/.git/worktrees/wt` is
    `main/.git/worktrees/wt`. Views are process-wide and only ever added
    to: when several views share the deepest path (fronts of one volume
    whose pods mount different `subPath`s at the same `mountPath`), each
    one's mapping is tried, and nothing else: a suffix that happened to
    name a git directory would make an ordinary directory nested and
    leave its files unrecorded. A target under no view falls back to
    trying each of its suffixes from the tree root, and each such match
    counts in `agentfs.nested.gitdir_heuristic`: a nonzero count means a workload
    mounts the tree where agentfs was not told
    ([runbook](runbooks/worktree-heuristic.md)). Views are not yet keyed by
    front, nor dropped when a front's lease ends.
- **Bounded reads.** These files are workload-controlled and read in the
  synchronous step, so they are opened without following symlinks and
  without blocking (a FIFO cannot hang the daemon), must be regular files,
  and are read only up to 4 KiB; a larger one does not count, as git refuses
  oversized gitfiles.
- Under `nested = working-tree`, a directory that is a git directory is
  still skipped entirely. This also covers a submodule's git directory kept
  inside the tree.
- With several nested repositories on a path, the outermost one is reported.

## 6. Metadata and permissions

- **Kept by the backing filesystem:** all metadata, including permissions,
  ownership, timestamps and extended attributes.
- **Recorded in history:** what git can represent: file content, the executable
  bit, and symlinks.
- **Not recorded:** ownership, other permission bits, timestamps and extended
  attributes. Recovery (§15) restores content and the executable bit only.
- **Access.**
  - The mount uses `allow_other` and the kernel's `default_permissions`. The
    kernel checks the real stored ownership and modes, so root and non-root
    callers behave as they would on the backing filesystem.
  - `chown`, `chmod` and `utimens` are passed through to the backing tree,
    and so are extended attributes (on regular files and directories).
    `chmod` of a symlink is not supported (Linux has no `lchmod`).
  - **Ownership of new objects.** The daemon performs each operation with
    its own privileges once the kernel has checked the caller's, except
    creating. With `CAP_SETUID`, `CAP_SETGID`, `CAP_CHOWN` and `CAP_FOWNER`
    (root, or a non-root daemon given them; decided from the effective
    capability set, not the uid), it creates every file, directory,
    symlink and FIFO **as the caller**: for the one creating system call,
    the worker thread switches its own filesystem uid and gid
    (`setfsuid(2)`, `setfsgid(2)`) and supplementary groups (read from the
    caller's `/proc/<pid>/status`) to the caller's, with a umask of 0 in a
    filesystem context of its own (`unshare(CLONE_FS)`), and switches back
    (the process aborts if it cannot). The kernel drops the thread's
    filesystem capabilities meanwhile, so the creation is checked as the
    caller's would be, and the object is the caller's from the instant it
    exists: another front opening the same name as the caller never finds
    it root's for a moment (§14.1), as long as the caller's groups are
    known (below). A creation refused as the caller
    (`EACCES` or `EPERM`: a device node needs `CAP_MKNOD`) is made as the
    daemon instead, which, with `CAP_CHOWN` and `CAP_FOWNER`, then gives it
    to the caller: the caller's group, except under a setgid directory,
    where the backing filesystem already chose the directory's group. The
    daemon then sets exactly the mode the caller asked for, so its own
    umask never applies. Without those capabilities everything it creates
    belongs to its own user. Owner and mode are always set on a descriptor
    of the new object (opened with `O_PATH | O_NOFOLLOW` and checked to be
    what was just created: the right type, owned by the daemon or the
    caller it was made as, one link unless a directory), never by name, so
    a name swapped for a symlink or hard link in the backing tree meanwhile
    is refused with `ESTALE` and nothing outside is touched. `create` makes
    the backing file with `O_EXCL`: if another writer made the name first,
    an exclusive create fails with `EEXIST`, and any other opens the
    existing file as `open` would (checking the caller against its mode
    bits, with the caller's primary and supplementary groups), without
    changing its owner or mode.
  - **Callers whose groups are unknown.** FUSE reports pid 0 for a
    caller outside the daemon's pid namespace: a pod's process seen from
    a CSI node plugin that does not share the host's (the shipped
    manifest sets `hostPID: true` for this reason, §14.2). Its
    supplementary groups cannot be read then (the daemon says so once on
    stderr), so a creation that only a supplementary group allows (a
    directory whose group is the pod's `fsGroup`, and the pod's primary
    group another) is refused as the caller and made as the daemon, then
    given to the caller: another front's racing open of that name can
    still find it the daemon's and fail with `EACCES`, and `create`'s
    check of a file another writer made first counts the primary group
    only. Every object made as the daemon when it was to be made as the
    caller (device nodes included) is counted by
    `agentfs.fuse.create_as_caller_fallbacks`.
  - **POSIX ACLs are not enforced** through the mount (`FUSE_POSIX_ACL` is
    not negotiated); only mode bits are.
  - **Inode numbers.** fuser sends the attribute's inode number as the FUSE
    node id, so `st_ino` (and `d_ino` in directory listings) is agentfs's
    node id: one per backing inode, shared by hard links, kept for the life
    of the mount even after the kernel forgets the node (at one small map
    entry per backing inode seen), but not stable across remounts. Tools
    that cache inode numbers, such as git's index for a repository on the
    mount, re-read files once after a remount.
  - **Confinement.** The daemon reaches the backing tree only through a
    descriptor opened at mount and `openat2` with `RESOLVE_BENEATH`,
    `RESOLVE_NO_SYMLINKS` and `RESOLVE_NO_MAGICLINKS` (Linux 5.6+), and
    checks that every path still leads to the node's backing inode
    (`ESTALE` otherwise). A symlink anywhere in a path, whatever the
    workload did to the tree, is never followed. Path resolution is ordered
    against renames (a rename holds a lock for writing; every other
    operation, including `getattr`, `readlink` and the extended-attribute
    calls, holds it for reading), so an ancestor rename never makes a
    `stat` of an existing file fail.
- **Hermes.** Its tool containers run as root while files are owned by UID
  10000. git's "dubious ownership" check then needs `safe.directory` in those
  containers. That is a consumer setting (§16), not an agentfs one.

## 7. Large files and LFS (phase 2)

Built ahead of the rest of phase 2 (`agentfs_core::lfs`). Large content can be stored as Git LFS
objects, with pointer blobs in history that an ordinary `git clone` plus
`git lfs pull` turns back into content.

**Which store.** Decided when a file is recorded, on the frozen copy's
size, and saved in the journal with the change (`Change::Write.store`;
older journals read as blob), so the blob-size gate stays at record time
and a replay is deterministic:

1. **Always a blob:** empty files; git's special files (`.gitattributes`,
   `.gitmodules`, `.gitignore`, `.mailmap`, in any spelling git treats as
   one) and `.lfsconfig`; a path no `.gitattributes` line can name exactly
   (one holding a tab, CR or LF, or whose line would reach git's 2048-byte
   limit). Symlinks are never LFS.
2. **`filter=lfs`:** LFS. `agentfs.store=blob` may not override it.
   Operator `info/attributes` always counts; a tracked `.gitattributes`
   only with `respectTreeRules = true` (§4.1). Otherwise a tree
   `filter=lfs` path is stored as a blob, which git-lfs's smudge passes
   through unchanged, so clones still show the content (but `git status`
   in such a clone may report it modified).
3. **`agentfs.store=lfs` or `blob`** (operator attributes only).
4. **`agentfs.lfsThreshold`:** content strictly larger is LFS. **Unset by
   default**, so size-based LFS is off until an operator sets it: phase 1
   remotes have no LFS server, and pushes would stall.
5. Otherwise a blob.

The blob size limit (`agentfsMaxBlobSize`, §3.4) does not apply to
content stored as LFS: history holds only the pointer. Content stored as
LFS has its own limit instead, `remote.<name>.agentfsMaxLfsObjectSize`
(per object, matching hosts' per-file LFS caps such as GitHub's 2 to
5 GiB): the smallest across valid remotes applies when a file is
recorded, because one history goes to every remote, and over it the file
is refused (`PathRefused`, reason `lfs_object_too_large`), never stored
as a blob instead, which would contradict `filter=lfs` and the blob
limit. Before the freeze the store is decided on the size on disk and
that store's limit applies, so a file far over either limit is never
copied; the commit-time checks are the backstop for records journaled
before a limit was lowered. At push time each remote checks its own limit
on the objects it would upload and fails the attempt as `too_large`
(stage `lfs`) before any request, with no ref pushed.

**Commit.** Each LFS write is hashed (SHA-256) into the local object
cache and committed as a canonical pointer blob (git-lfs spec v1:
`version`, `oid sha256:…`, `size …`, nothing else) with the file's mode,
so the executable bit is kept. Hashing happens at commit time, outside the
synchronous step (§3.2). The cache object is durable before the branch
moves; a crash in between leaves a reusable object.

**History must be readable by ordinary git.** Every commit rebuilds a
block at the end of the root `.gitattributes`, **in history only** (the
backing tree is never written):

```text
<the workload's own .gitattributes, byte for byte>
# agentfs-lfs-begin: maintained by agentfs (Git LFS paths); edits are replaced
/big.bin filter=lfs diff=lfs merge=lfs -text
/dir[[:space:]]with[[:space:]]space/x.psd filter=lfs diff=lfs merge=lfs -text
# agentfs-lfs-end
```

- One line per path currently stored as LFS, anchored with a leading `/`
  and escaped so it matches that path alone (`\`, `*`, `?`, `[` get a
  backslash; a space is `[[:space:]]`, as `git lfs track` writes it), with
  exactly the attributes `git lfs track` writes. Tested against
  `git check-attr` and gix-attributes. `[[:space:]]` also matches a tab,
  so `a b`'s line also covers `a<TAB>b` (harmless: that path cannot be
  LFS itself).
- At the end of the file, so its lines win over the workload's (the last
  match wins). A deeper `.gitattributes` that unsets `filter` for a
  managed path would still make clones show pointer text; agentfs does
  not detect that.
- A path leaves the block when it is removed, rewritten as a blob (for
  example shrunk below the threshold) or replaced by a symlink or a
  directory. The block lists the head's managed paths plus this commit's
  LFS writes; the head's block is trusted because agentfs wrote it.
- The workload's `.gitattributes` content is kept; a block it wrote (a
  line exactly the BEGIN line through the next line exactly the END line,
  with no other BEGIN between, a trailing CR ignored) is removed. Every
  other line is kept byte for byte, including one that only starts like a
  marker (`# agentfs-lfs-begin notes`) or an unterminated BEGIN. A root
  `.gitattributes` holding only the block is created and removed by
  agentfs.
- **A pointer is only committed with its line.** The block must have a
  place and pass the checks of §3.4 with the configured limits (the
  composed file is a blob like any other: `gitattributesLarge`, the
  `agentfsMaxBlobSize`, the line length). A directory named
  `.gitattributes` (or `.gitmodules`) is refused at record time
  (`gitattributesBlob`, `gitmodulesBlob`, §3.4), so normally it does. When
  it does not (history from before that check; a `.gitattributes` that
  fits the limits alone but not with the block; very many LFS paths, at
  about 100 bytes a path; limits lowered below the head's own file), the
  commit falls back instead of failing, each step only if the one before
  is not enough:
  1. that commit's LFS writes are stored as blobs (the blob limit applies
     to them, and one over it is dropped with `PathRefused`, stage
     `commit`);
  2. its writes at or beneath the root `.gitattributes` are dropped with
     `PathRefused` (stage `commit`, the composed file's reason), and the
     block is rebuilt on the head's file;
  3. the head's root `.gitattributes` is kept as it is (a path that
     stopped being LFS in that commit keeps its line until it is written
     again; harmless, since git-lfs passes non-pointer content through).

  Later commits are unaffected: once the block fits again, LFS writes
  are pointers again. The `agentfs.commit` span carries
  `agentfs.lfs_as_blobs` when step 1 ran.

**Endpoints** come only from operator config (the state area's config and
its includes; a tracked `.lfsconfig` is content, never read), with
git-lfs's precedence:

| Uploads (push) | Downloads (restore) |
|---|---|
| `lfs.pushurl` | |
| `lfs.url` | `lfs.url` |
| `remote.<name>.lfspushurl` | |
| `remote.<name>.lfsurl` | `remote.<name>.lfsurl` |
| derived from `pushurl`, else `url` | derived from `url` |

- An explicit `http(s)` URL is the endpoint as written. A remote URL
  derives `<url>/info/lfs` when it ends in `.git`, else
  `<url>.git/info/lfs`, as git-lfs does (GitHub, GitLab and Gitea serve
  LFS there).
- A local path or `file://` URL, explicit or derived, uses git-lfs's
  standalone-file layout: `<git dir>/lfs/objects/aa/bb/<oid>` of that
  repository, which `git lfs pull` in a clone of it reads.
- An SSH remote without an explicit endpoint uses the one
  `git-lfs-authenticate` names (2026-10): agentfs runs
  `git-lfs-authenticate <path> <upload|download>` on the git server over
  its own SSH transport (§11.1), bound to the push URL's host and port,
  with the remote's known_hosts, key or agent and user, and the host
  checked before any key is offered. Uploads use the push URL's path,
  downloads `url`'s, which must be on the same host and port (else the
  remote needs `lfsurl`). The path is sent as git-lfs sends it: the URL's
  path without a leading `/`, unquoted; one that would need quoting is
  refused (`local`). The answer's `href` (http, or https with the `https`
  feature) is the endpoint and its `header` goes to batch requests; it is
  reused until 5 s before `expires_in` or `expires_at` (RFC 3339), or,
  without either, for one push attempt or fetch. A refused host is
  `untrusted_host`, a refused key `auth`, no connection `unreachable`, and
  a non-zero exit (git-shell's "unrecognized command") or JSON agentfs
  cannot read `refused`, with a hint to set `lfsurl`; there is no fallback
  to a guessed https URL. git-lfs's pure SSH protocol (`git-lfs-transfer`)
  is not built: servers offering only it need `lfsurl`.
- `git://` remotes derive nothing and need an explicit `lfsurl`.
- An explicit endpoint that is not `http(s)`, `file://` or a path, or is
  `https` in a build without the `https` feature, makes the remote
  misconfigured (§11.1); a bad global `lfs.url` misconfigures every remote.

**Credentials** are bound to the endpoint's origin (scheme, host and
port) and never sent anywhere else:

1. `remote.<name>.agentfsLfsCredentials` (an absolute path, the same file
   format as `agentfsCredentials`), if set;
2. else, when the endpoint has the push URL's origin (as GitHub's derived
   endpoint does), `remote.<name>.agentfsCredentials`;
3. else a password in the endpoint URL;
4. else none.

So a separate LFS host never sees the git host's credentials, and an
`http` endpoint never gets those of an `https` push URL. For an endpoint
`git-lfs-authenticate` names, its `Authorization` header is the
credential (sent to batch requests, and to action hrefs on its origin
that carry none); without one, `agentfsLfsCredentials` applies, bound to
that endpoint's origin; else requests are anonymous. The files are
re-read when they change, like other credentials, and `mount` refuses
one under the backing tree or mount point. agentfs's `Authorization`
goes to batch requests, and to an action's `href` only when it has the
endpoint's origin and the server gave the action no `Authorization` of
its own; the headers a server gives an action are sent as given.
Redirects are never followed. LFS requests go through the remote's proxy
(`remote.<name>.proxy`, else `http.proxy`, §11.1) when one is set, with
`CONNECT` for `http` and `https` alike, and proxy environment variables
are ignored.

**Client.** Neither libgit2 nor gitoxide implements LFS. agentfs's own
client (`ureq` 3, pure Rust; TLS through native-tls, OpenSSL and the
system trust store, under the `https` feature, as libgit2 uses) speaks
the batch API with the `basic` transfer only: batches of 100 objects,
in sequence, a `PUT` of each object the server lacks, then `verify` when
the server asks for it; a `GET` for downloads, hashed while it is written
and kept only if it matches. Within a batch, up to
`lfs.concurrenttransfers` objects (git-lfs's key; default 8, 1 to 64, a
bad value misconfigures every remote) move at once, each remote with its
own pool of scoped threads and as many idle connections. An upload starts
no new object after its first failure, lets those in flight finish, and
reports that failure; a download keeps going past an object one server
lacks (404) and stops at a failure of the server itself. Timeouts: 30 s
to connect, 5 minutes for a batch or verify request, and for an object 5
minutes plus its size (what is left of it, when resuming) at 256 KiB/s.

**Resuming.** Downloads resume by byte: an object is written into
`<state>/lfs/incomplete/<oid>` (0600) under an exclusive `flock` on that
file, so one download of an object runs at a time and the others then
find it cached. The partial survives failures, commands and restarts; the
next download hashes what it holds and sends `Range: bytes=<n>-`. A `206`
must start at `n` (only the start is checked: lfs-test-server reports the
remaining length as the total), a `200` starts again from the first byte,
and a `416` or a `206` starting elsewhere drops the partial and asks once
for the whole object. A partial longer than the object is emptied first;
a finished one is checked (size and SHA-256), fsynced and renamed into
`objects/`, and a mismatch deletes it (`refused`). An object is tried up
to 3 times when the server cannot be reached (`unreachable`), 1 s and then
2 s apart, each try resuming. Uploads resume by object: an attempt that
fails keeps what it finished on the server, and the batch API skips those
objects at the next attempt, which the push backoff schedules; an object
is never retried within an attempt. Byte-level upload resume (git-lfs's
experimental `tus` adapter) is not built: no LFS server agentfs can test
against implements it (lfs-test-server, GitHub, GitLab and Gitea do not).

**Upload before push.** Before every push attempt, each remote uploads
the LFS objects the commits it lacks reference (`head ^ last_pushed`, or
the whole branch when the remote has nothing or its last pushed commit is
unknown): blobs a commit adds or modifies, compared with its first parent
without rename detection, at paths in that commit's managed block, that
are canonical pointers. Pointer text a workload wrote at an unmanaged path
is ordinary content. Every object must be in the local cache. Only when
every upload succeeds does the git push run; any failure fails the attempt
with stage `lfs` (§13) and pushes no ref, with the usual alarm and backoff
(§11.2). A remote whose backlog references no LFS object never resolves or
contacts an endpoint, so a `git://` remote without `lfsurl` keeps pushing
until LFS content appears. Failures reuse the push failure kinds:
`unreachable` (connections, timeouts, HTTP 429 and 5xx), `auth` (401, 403,
a missing or rejected credential), `untrusted_host` (a TLS certificate
that fails verification, recognised from OpenSSL's message), `refused`
(any other status or per-object error, a redirect) and `local` (a missing
cache object, no endpoint, no `https` feature).

**Cache.** `<state>/lfs/objects/aa/bb/<oid>`, git-lfs's layout, with
`<state>/lfs/tmp/` for objects being written: written to `tmp/`, fsynced,
renamed into place, directories fsynced. `agentfs.state.bytes{part="lfs"}`
measures it. `agentfs lfs fetch` fills it from the remotes: the objects
the branch head references, those of given revisions, or every object of
the branch (`--all`), each remote in name order taking what the earlier
ones lack; `--dry-run` reports what is missing without contacting a
server. Readers that write objects into the tree (restores, reverts,
patch applies) and `lfs fetch` hold `<state>/lfs/lock` shared while they
use the cache.

**Restore and revert** (§15) are LFS-aware: a pointer at a managed path is
checked out as its object's content, from the cache or else downloaded
from the remotes in name order, and every object a plan writes is fetched
during the check, before anything is written: all of them at once first
(concurrently, resuming partial downloads), then each checked so a
missing one names its path and refuses the restore (`lfs_object`). The
restore holds the cache lock (`<state>/lfs/lock`) shared from the check
through its writes, so `agentfs lfs prune` cannot remove an object in
between. The tree's file is compared by size and SHA-256.
The root `.gitattributes` is compared and written without the managed
block, and one holding only the block is absent from the tree. A dry run
(`--dry-run`) downloads and writes nothing: it looks only in the cache and
reports how many objects (and bytes) a real run would download, without
checking that a remote has them.

**Retention.** LFS objects cannot be pruned from most LFS servers
(GitHub's needs the repository deleted), and pruned pointers break
clones. agentfs never deletes anything on a server, and never prunes on
its own. The local cache is pruned only when an operator runs `agentfs
lfs prune` ([runbook](runbooks/lfs-storage.md)), and only of objects
confirmed on every valid remote's upload endpoint and on its download
endpoint where that is another one: an upload batch that asks for no
upload of the object and reports no error for it, a download batch that
offers a download and no error, or for a file remote the file with the
object's size. A per-object `404` keeps the object; any other failure
aborts the prune. A misconfigured remote, or one whose endpoint cannot be
resolved, refuses the whole prune (nothing could be confirmed there),
and with no remote nothing is pruned. `--older-than` keeps recently
cached objects; `--dry-run` deletes nothing. Confirmation takes no lock,
since servers never forget; the deletions run under `<state>/lfs/lock`
taken exclusive (waiting `--wait-lock`, else exit 75), which restores,
reverts, patch applies and `lfs fetch` hold shared while they use the
cache, so no object goes between a restore's check and its write.
Partial downloads nothing holds are deleted too. Pushes need a cached
copy only of objects their server lacks, so a pruned object costs a
push nothing; a remote added after a prune that lacks old objects fails
its push as `local` (stage `lfs`) with an `agentfs lfs fetch --all` hint
until they are fetched back.
Growth is also limited at the source, with operator rules, the `fsync`
class's coalescing interval and `agentfs.maxSize` per path (§8).

**Not built:** byte-level resumable uploads (git-lfs's experimental `tus`
adapter, which no LFS server agentfs can test against implements),
git-lfs's pure SSH protocol (`git-lfs-transfer`), proxy authentication,
and `https://` or SOCKS proxies. Members of an `fsync`-class group (§8)
follow the same store rules as any path.

## 8. Files changed in place: the `fsync` class (phase 2)

Databases and similar files are held open and changed in place. Recording them
only on `close` could mean never. Built in `agentfs_core::fsync_class` (the
policy), `agentfs_core::volume` (atomic group records), `agentfs_fuse::groups`
and `agentfs_fuse::ops` (the mount) and `agentfs_core::repack`.

**Opting in.** Only the operator's `info/attributes` (§4.2) puts paths in
the class; a tracked `.gitattributes` cannot, whatever
`respectTreeRules` says. The usual line for SQLite is

```text
*.db* agentfs.commit=fsync agentfs.group=sqlite
```

with `*.db*` so the pattern covers the `-wal` and `-journal` files too. A
bad `agentfs.commit`, `agentfs.coalesce` or `agentfs.maxSize` value in
`info/attributes` fails the volume at open (`RulesError::BadAttribute`,
naming the line), rather than being ignored as git ignores unknown values.

**Groups.**

- A group is the set of paths in one directory with the same
  `agentfs.group` value *and the same stem*: `state.db`, `state.db-wal`
  and `state.db-journal` (stem `state.db`). Every member must be in the
  class; a member the attributes leave out is not part of the group.
- A class path with no `agentfs.group` is a group of one: only itself.
- The group's interval is the smallest `agentfs.coalesce` of its members.
- `-shm` files are never recorded (`SkipReason::SharedMemory`): SQLite
  rebuilds the WAL index from the WAL when a database is opened.
- Two databases in one directory are two groups.

**Triggers.**

- An `fsync` (or `fdatasync`) on any member, if the group changed and no
  capture was attempted in the last interval (trigger `fsync`). An `fsync`
  inside the interval is passed through and records nothing; a capture that
  fails does not fail the `fsync`.
- **A trailing timer** (thread `agentfs-capture`, trigger `interval`).
  SQLite with `synchronous=NORMAL` does not `fsync` on commit, so a group
  with unrecorded changes is captured one interval after its first change
  since the last capture, and never sooner than one interval after the
  last attempt.
- The close (FLUSH) of the group's last open handle, if the group changed
  (trigger `close`; a failure fails the `close` with `EIO`, as for any
  file), and the release of the group's last handle (trigger `release`).
  Closing one member while another stays open records nothing: SQLite in
  DELETE mode creates and closes its `-journal` on every transaction while
  the database stays open, and capturing each of those closes would defeat
  the interval.
- Unmount: every group with changes is captured after the sweep thread
  stops (`unmount: N database group(s) captured`).
- A rename, unlink, link or size change of a member is not recorded on
  its own, which could split the group: it marks the group changed, and
  the next capture of the whole group records it, at most one interval
  later.
- An operator operation through the mount (`agentfs restore`, `revert`,
  §15): a group the operation's process changes is held back from every
  trigger above while the operation runs, and captured once, whole, when
  it ends (trigger `restore`, the operator as author), before the
  operation's commit. A restore that writes the database and then its
  `-wal` therefore never records the mix of the two versions. If that
  capture fails the operation reports an error.
- `agentfs record <path>` (§17.1) records a member's whole group in one
  record. It is meant for a stopped workload: it does not hold writes.

**Consistent capture.**

- Handles on class paths are never passthrough (§2.1), so the daemon serves
  every write to them. Each write, truncation, `fallocate`,
  `copy_file_range`, `O_TRUNC` open, unlink, link or `mknod` of a member
  holds the group's gate around its system call. A capture *freezes* the
  gate: it waits for the holds in progress, blocks new ones, and copies
  every member into `pending/` while nothing can change any of them, so
  the copies are one point in time, as a crash would leave them, which
  SQLite recovers from. Renames are kept out by the mount's name lock,
  which a capture holds for reading. The gate prefers the capture, so a
  busy writer cannot starve it; writes wait for the length of one copy.
- **The cost of a capture is mount-wide for namespace changes.** The name
  lock is the mount's, not the group's: while a group is being copied,
  every rename, unlink, link, `mkdir`, `rmdir` and create *anywhere on the
  mount* waits for the copy to finish. Reads and writes of other files do
  not wait. A 1 GiB database copied at 500 MB/s holds renames for about
  two seconds once per interval; keep large databases on volumes whose
  workload is not rename-heavy, or raise `agentfs.coalesce`.
- A member is read at capture time by its current name. A member that is
  gone (`ENOENT`, `ENOTDIR`) is recorded as removed; any other error
  (`EACCES`, `EIO`, …) fails the capture, which raises `RecordFailed` and
  leaves the group changed, rather than recording a removal that did not
  happen.
- A handle's group is found again after any rename on the mount, so a
  database whose directory was renamed while it was open is captured
  under its new path.
- The writeback cache is never enabled (§2.1), so the daemon sees writes
  as they happen. Writes through a shared mapping reach it when the kernel
  writes the pages back, and are captured from then on.
- **The group is recorded whole or not at all.** A capture is one record
  marked atomic in the journal. If any member is refused (a check of §3.4,
  `agentfs.maxSize`, a path rule), at record time the others are skipped
  (`SkipReason::GroupRefused`), and at commit time the whole record is
  dropped. A member that a path rule skips (`info/exclude` matching
  `*-wal`, say) refuses the group too (`PathRefused` with reason
  `group_member_skipped`) rather than dropping out of it, since a database
  recorded without its WAL is not one point in time. `-shm` is the one
  member always left out. `agentfs.maxSize` is enforced when a capture is
  recorded (on the size on disk and on the frozen copy), not again at
  commit time. A restored database plus its `-wal` or `-journal` therefore
  always comes from one capture.
- A capture that fails leaves the group changed, so the timer tries again
  one interval later. A refused capture counts as done, as a refused close
  does for an ordinary file. Captures of an unchanged group commit nothing
  (the committer skips commits whose tree does not change).

**Storage.**

- Members are stored by the rules of §7 like any path: LFS when the
  operator says so (`agentfs.store=lfs`, `filter=lfs` or the threshold),
  ordinary objects otherwise.
- **`agentfs.maxSize`** (operator attribute, any path, `k`/`m`/`g` in
  powers of 1024) refuses content larger than it, whatever the store,
  checked on the size on disk and again on the frozen copy
  (`PathRefused` with reason `max_size`). For a group it refuses the
  whole capture.
- **Repack.** libgit2 writes each version as a full compressed loose
  object and has no repack. Every `agentfs.repackInterval` (default 1h,
  `0` or `off` disables, at least 1m) the commit loop runs
  `git repack -d -q --geometric=2` between commits, so versions of a
  database are delta-compressed into packs. It never prunes. It takes
  every remote's push lock without waiting and skips the round if a push
  is running; a failure raises `RepackFailed` (§13) and is tried again an
  interval later. The container image ships the `git` CLI for this;
  without it, every round fails with `RepackFailed`.
  - *Bounded.* A round with no loose objects is skipped without starting
    git. git runs with one thread, `pack.windowMemory=64m`,
    `pack.deltaCacheSize=16m`, `core.bigFileThreshold=64m` (larger blobs
    are stored whole, not delta-searched) and `core.packedGitLimit=128m`,
    bitmaps off. Its memory is roughly 256 MiB plus the largest object it
    must read whole; budget that on top of the daemon in the container's
    memory limit.
  - *Isolated.* git's environment is cleared: only `PATH`, `LC_ALL=C`,
    `HOME` (the history directory), `GIT_DIR`, `GIT_CONFIG_NOSYSTEM=1`,
    `GIT_CONFIG_GLOBAL=/dev/null` and `GIT_TERMINAL_PROMPT=0` are set, so
    no `GIT_*` variable of the daemon's environment and no system or
    global config changes what it does.
  - *Cancellable.* It runs on the commit thread, which polls it: a stop
    (shutdown, unmount) kills it, and the round counts as stopped, not
    failed. A killed repack leaves at most temporary `tmp_pack_*` files,
    which git ignores (nothing removes them). Commits wait while a repack runs.

**Guarantee.** For this class "before return" weakens to "at most one interval
late": a change to a member is in a journaled record no later than one
`agentfs.coalesce` after it was made (plus the time of the capture), or
at the group's last close or unmount, whichever comes first, and it reaches a
commit `commitWindow` later. This is deliberate. An `fsync` returning does
not mean its data is recorded.

**What a crash loses.** Which groups changed since their last capture is
kept in the daemon's memory only. If the daemon or node dies, the changes
made since each group's last capture (up to one interval of them) are in
the files, which SQLite recovers as usual, but not in history; the journal
holds everything that was captured. The restarted daemon does not compare
the files with history, so those changes are recorded only when the
database next changes (its next capture records the whole current state),
and a database that is never written again keeps them out of history.
Run `agentfs record <path>` on such a database (with the workload
stopped) to record it.

**Not built, and known limits.**

- `agentfs restore` and `agentfs revert` expand fsync-class groups (§15):
  restoring or reverting `app.db` restores its `-wal` or `-journal` from
  the same commit, in one operation and one commit. A member the plan
  cannot account for (untracked but present) refuses the operation.
  Restoring a live database still needs the workload stopped
  ([runbook](runbooks/recovery-and-undo.md#7-restoring-a-database)),
  since SQLite does not expect its files to change under it.
- A hard link to a member under a name outside the class is an ordinary
  file.
- The class is per mount: a passthrough handle on the inode opened under a
  non-class name (a hard link) is not held by the gate.
- No per-group storage budget beyond `agentfs.maxSize`, and no pruning of
  old versions. Repack runs on the commit thread, so commits wait for it.

## 9. Commit metadata

**Committer: the replica that made the change.** Every pod mounting a volume has
its own committer identity. Operators set it with templates; the defaults are
below.

```ini
[agentfs "identity"]
    committerName  = {replica}
    committerEmail = {podUid}@{volume}.agentfs.invalid
    authorName     = uid-{uid}
    authorEmail    = uid-{uid}@{volume}.agentfs.invalid
```

**Template variables:**

- `{volume}`, `{namespace}`, `{pod}`, `{podUid}`, `{node}`;
- `{replica}`: the StatefulSet pod name, which is stable across restarts;
  otherwise the pod name;
- `{uid}`, `{gid}` (author templates only).

**Rules.**

- Rendered identities must pass git's fsck rules (§3.4). A template that does
  not is a configuration error at mount.
- The default `.invalid` domain can never belong to a real account, so commits
  are never linked to a person on a hosted remote.

**Author: the writer process** as FUSE reports it. PIDs and UIDs are as seen
from the mount's own PID namespace. Identity beyond UID needs the workload's
cooperation (§1.3, later phases).

**Trailers: pod metadata.**

```
Agentfs-Seq: 18342
Agentfs-Trigger: close
Agentfs-Namespace: agent
Agentfs-Pod: hermes-7c9d8-x2k4q
Agentfs-Pod-Uid: 0b6f…
Agentfs-Node: hawking
Agentfs-Uid: 10000
Agentfs-Pid: 4711
```

- **Phase 1** gets pod metadata from the downward API.
- **The CSI driver** gets it from `podInfoOnMount`.
- **Fronts of a history service** (§14): each record a service receives
  from a front is journaled with that front's `origin` (namespace, pod,
  pod UID, node, replica), so a replay after a crash commits it with the
  same committer. The service renders the volume's templates once per
  origin; a commit never mixes origins, and its committer and pod
  trailers are that pod's. A front whose pod does not render is refused
  when it connects. If the templates change across a restart of the
  service so that a journaled origin no longer renders, its records are
  committed with the service's own committer and keep the pod's trailers,
  with an `IdentityFallback` warning (§13), so the commit loop never
  stalls. Single-process journals carry no `origin` and are unchanged.
- **Grouped commits** carry one `Agentfs-Uid` and `Agentfs-Pid` line per
  distinct writer. A pid of 0 (an operator outside the daemon's pid
  namespace, §15) is unknown and gets no `Agentfs-Pid` line.
- **Restores and reverts** (§15) are commits of their own, never grouped
  with workload changes. Their subject is `restore <path> from <short id>`
  or `revert <short id>: <subject>`, they carry `Agentfs-Trigger: restore`
  and `Agentfs-Restore: <commit>` or `Agentfs-Revert: <commit>` (the full
  id), and their author, `Agentfs-Uid` and `Agentfs-Pid` are the
  operator's: the `agentfs` command's own ids, or, through a running mount,
  those of the process that asked (§15). Each journal record carries the
  operation it belongs to; older journals without the field still read.
- **Patches** (§12) are commits of their own too, authored by the patch's
  author (name, email and time) and committed by the replica. Their
  message is the patch's, and their last paragraph carries
  `Agentfs-Trigger: patch`, `Agentfs-Patch: <patch commit>` and
  `Agentfs-Inbox: <n>`, which is how the queue knows a patch is applied:
  only that paragraph counts, so a path or a patch message cannot forge
  it.
- **A fork's finish commit** (§11.5) is a marker commit whose last
  paragraph carries `Agentfs-Seq`, `Agentfs-Trigger: finish`,
  `Agentfs-Fork: <fork>`, `Agentfs-Source: <source>`, `Agentfs-Base:
  <commit>`, `Agentfs-Change: <commit>` (absent when nothing changed),
  `Agentfs-Fork-Remote: <remote>` and `Agentfs-Unrecorded: <n>`. Only that
  paragraph counts, each field checked against its grammar (names as
  volume names, full commit ids, a decimal count), each at most once. The
  change commit carries `Agentfs-Fork`, `Agentfs-Source` and
  `Agentfs-Base` in a final trailer paragraph, which the patch queue drops
  when it applies the change (§12).

## 10. Secrets (not in scope)

agentfs does not scan content for secrets and does not hold pushes.

- **Keeping secrets out** is done with ordinary operator rules (§4). Each
  consumer excludes its own secret files (§16).
- **A secret that gets recorded anyway** is permanent in history, locally and on
  every remote it reaches. Removing it means rewriting history: a documented
  operator procedure (§15,
  [history-rewrite.md](runbooks/history-rewrite.md)) for the state area
  and each remote, run with the
  workload stopped. It is never automatic.

## 11. Remotes and pushing

### 11.1 Remotes

- **Any git remotes,** over HTTPS or SSH, configured by the operator. GitHub,
  Gitea or a bare repository on a server all work the same way. Local paths
  and `file://` URLs work too, which is how pushing is tested.
- **Several push remotes** can be configured. Each has its own push state and
  alarms.
- **Configuration** is the standard `[remote "<name>"]` sections (`url`, and
  `pushurl`, which wins as in git) plus agentfs's own keys (§4.3), read only
  from the history repository's own config file. libgit2's system, XDG,
  global and ProgramData config search paths and its home directory are
  cleared once per process, and pushes use anonymous remotes with explicit
  `<oid>:<ref>` refspecs, so `remote.<name>.push` and `fetch`, any global
  `url.<base>.insteadOf` or `http.*` setting, and `~/.ssh/known_hosts` are
  never consulted.
- **An invalid remote section never stops the volume.** A remote section is
  left out (never pushed, and its limit does not apply) when:
  - its name is not a valid agentfs name (§11.4, e.g. `team/backup`, which
    git accepts);
  - `agentfsCredentials`, `agentfsSshKey`, `agentfsSshKeyPassphrase`,
    `agentfsSshAgent` or `agentfsKnownHosts` is not an absolute path, or `agentfsMaxBlobSize`, `agentfsMaxPushSize` or `agentfsMaxLfsObjectSize` is not a
    positive size;
  - its push URL is SSH and `agentfsKnownHosts` is missing, not exactly
    one of `agentfsSshKey` and `agentfsSshAgent` is set,
    `agentfsSshKeyPassphrase` is set without `agentfsSshKey`,
    `agentfsCredentials` is set, the URL carries a password, or
    the URL has no host or a bad port;
  - its push URL is not SSH and `agentfsSshKey`,
    `agentfsSshKeyPassphrase`, `agentfsSshAgent` or `agentfsKnownHosts` is
    set;
  - its push URL is SSH and the binary was built without the `ssh` cargo
    feature;
  - an LFS endpoint (`lfs.url`, `lfs.pushurl`, `remote.<name>.lfsurl` or
    `lfspushurl`) is not `http(s)`, `file://` or a path, or is `https` in
    a build without the `https` feature (§7); a bad global key makes every
    remote misconfigured;
  - `lfs.concurrenttransfers` is not 1 to 64 (every remote, §7);
  - `remote.<name>.proxy` is not a proxy agentfs can use, or
    `http.proxy` is not (every remote; see Proxies below);
  - `agentfsLfsCredentials` is not an absolute path.

  Each one raises `RemoteMisconfigured` (§13) when the volume opens;
  `agentfs status` lists it and `agentfs push` reports it and exits 1.
  Recording, committing and the other remotes carry on. Files that are
  missing or unreadable at push time are push failures instead (`auth` for
  a credential or key, `untrusted_host` for known_hosts), so a Secret
  mounted late or being rotated keeps working once it appears.
- **Credentials** come from files the recorder reads, and are re-read when they
  change, so short-lived tokens and keys can be rotated by whatever writes
  the file.
  - `remote.<name>.agentfsCredentials` (HTTP and HTTPS remotes) is an
    absolute path to a file holding either git-credential `key=value` lines
    (`username=`, `password=`; other keys are ignored) or a bare token, used
    as the password with the username from the URL, else `x-access-token`.
  - Every file is re-read when its stat stamp (device, inode, size, mtime,
    ctime) changes. The stamp follows symlinks, so kubelet's atomic `..data`
    swap for Secret volumes and a rename over the file are both seen. An
    in-place rewrite of the same size within the filesystem's timestamp
    granularity would be missed.
  - A credential or key is bound to the host (and port) of the remote's push
    URL and is never given to any other host.
  - It gets one attempt per connection: a rejected credential or key is an
    authentication failure, not a retry loop.
  - HTTPS needs libgit2 built with TLS: the `https` cargo feature (OpenSSL).
  - `remote.<name>.agentfsLfsCredentials` (same format) authenticates an
    LFS endpoint on another origin than the push URL; an endpoint on the
    push URL's own origin reuses `agentfsCredentials` (§7). LFS traffic
    uses agentfs's own HTTP client (`ureq`), not libgit2, with the same
    rules: bound to its origin, no redirects followed, the remote's proxy
    if one is set, TLS through OpenSSL and the system trust store with the
    `https` feature.
- **Proxies** (2026-10) come from git's own keys in the state area's
  config only: `http.proxy` for every remote, and `remote.<name>.proxy`,
  which overrides it for one remote (an empty value: no proxy for that
  remote, which takes `NO_PROXY`'s place). Proxy environment variables
  (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`) are never read. A
  proxy is a plain HTTP proxy, `http://host[:port]` (a bare `host[:port]`
  too; port 1080 by default, as in git). It carries the remote's libgit2
  `https` traffic (the push, the listing that measures a limited push,
  the patch fetch; libgit2's `GIT_PROXY_SPECIFIED`, `CONNECT`) and all of
  its LFS requests (explicit, derived and SSH-authenticated endpoints, and
  their action hrefs, `http` or `https`) through `ureq`'s `CONNECT`, so
  TLS is still verified end to end. SSH git traffic never goes through
  it, and neither does a plain `http://` git remote's: libgit2 1.9 writes
  the proxy's absolute-form request line but opens the connection to the
  server itself (found by the CI test against tinyproxy), so agentfs
  gives libgit2 no proxy for `http://` URLs, and `agentfs status` says
  `LFS only` for such a remote. Refused, so no proxy
  secret ever sits in config: a user or password (proxy authentication is
  not built), `https://` and SOCKS proxies, a path. A bad `http.proxy`
  misconfigures every remote; a bad `remote.<name>.proxy` that remote. A
  proxy that cannot be reached, or refuses, fails the push as
  `unreachable`. `agentfs status` prints each remote's proxy and its key.
- **SSH remotes** (`ssh://`, `ssh+git://`, `git+ssh://`, and scp-like
  `[user@]host:path`, which needs a `:` before any `/`) need the `ssh`
  cargo feature. libgit2 is built without libssh2: agentfs registers its
  own smart subtransport for these URLs (`ssh_transport.rs`), which runs
  `git-upload-pack '<path>'` or `git-receive-pack '<path>'` over a
  [russh](https://crates.io/crates/russh) session, with ring for its
  cryptography (no OpenSSL, no libssh2). The remote's settings reach the
  transport through a scope held on the pushing thread around each push
  or fetch; a connection with no scope, or to another host or port than
  the push URL's, fails closed. The same transport runs
  `git-lfs-authenticate` for the remote's LFS endpoint (§7), with the same
  host check, key or agent, user and timeouts.
  - An SSH remote authenticates with exactly one of a key file and an
    ssh-agent (2026-10).
  - `remote.<name>.agentfsSshKey` is an absolute path to a private key,
    OpenSSH (`BEGIN OPENSSH PRIVATE KEY`) or PEM (`RSA`, `EC`, PKCS#8; not
    DSA), decoded by agentfs, so no public key file is needed. `.pub`
    files are refused with an `auth` failure that says why. The key is
    read and decoded before connecting: a key that cannot be is an `auth`
    failure with nothing sent. It is offered once per connection; a
    refusal is an `auth` failure naming its SHA256 fingerprint and the
    user.
  - An encrypted key (OpenSSH, PKCS#8 `ENCRYPTED PRIVATE KEY`, or legacy
    RSA PEM with AES-128-CBC, the formats russh decrypts) needs
    `remote.<name>.agentfsSshKeyPassphrase`, an absolute path to a file
    holding the passphrase (one trailing newline dropped; 64 KiB at most;
    bound to the push URL's host and port like the key; re-read when it
    changes). It is decrypted before connecting: no passphrase file, or a
    wrong passphrase (`cannot decrypt <key> with <file>`), is an `auth`
    failure and the key is never offered. A passphrase file set for an
    unencrypted key is not read. Other legacy PEM encryption is refused
    with a hint to rewrite the key in OpenSSH format. The decrypted key is
    kept in memory while both files are unchanged, as decryption is
    deliberately slow.
  - `remote.<name>.agentfsSshAgent` is an absolute path to an ssh-agent's
    Unix socket, used instead of a key file. The environment's
    `SSH_AUTH_SOCK` is never read: an agent is per remote and explicit.
    The agent is asked for its identities after the host key check; each
    (a key or a certificate) is offered once, in the agent's order, until
    the server accepts one, as OpenSSH does, so sshd's `MaxAuthTries`
    (default 6) bounds how many are useful. A socket that cannot be
    reached, an empty agent, or every identity refused is an `auth`
    failure naming the socket and the fingerprints offered.
  - The user is the URL's, else `git`.
  - Connecting times out after 30 s, and the handshake, authentication
    and every read or write of the git command after 5 min without
    progress, as libgit2's own timeouts (§11.2) do for other transports
    (they do not apply to custom ones). A command that fails (a missing
    repository, a refused path) is reported with its exit status and its
    stderr.
  - The host key is verified by agentfs, in the transport, before any
    authentication, against
    `remote.<name>.agentfsKnownHosts`: an absolute path to an OpenSSH
    known_hosts file, required for every SSH remote. The host is looked up
    as the push URL's host, or `[host]:port` on a port other than 22, and
    the host libgit2 connected to must be that host. Plain and hashed
    (`|1|…`) names, comma lists, `*` and `?` wildcards, `!` negation and
    `@revoked` are honoured. An unknown host, a changed key, a key type the file does
    not list for the host and a revoked key are all refused, so the key is
    never offered to that host, and the push fails as `untrusted_host` with
    the file, the name looked up, the key type, its SHA256 fingerprint and
    the `ssh-keyscan` command that lists the server's keys. Nothing is ever
    accepted on first use or written back.
  - Host certificates (2026-10): an `@cert-authority` line trusts the
    host certificates its CA signed for the hosts the line matches. When a
    `@cert-authority` line matches the host as looked up, the transport
    asks for host certificates first (the certificate types of every key
    type russh supports), then plain keys, so a plain key line for the
    host still works with a server that has no certificate. A presented
    certificate is trusted only if all of these hold, as for OpenSSH: it
    is a host certificate; its CA is on a matching `@cert-authority` line
    and is not `@revoked`; the certified key is not `@revoked`; the CA's
    signature verifies; now is within its validity; its principals
    include the push URL's host, without the port, compared
    case-insensitively (an empty list, valid for any host, is refused);
    and it has no critical options. A plain key line never trusts a
    certificate, and an `@cert-authority` line never trusts a plain key.
    A refusal is `untrusted_host` before any authentication, naming the
    CA's SHA256 fingerprint and suggesting `ssh-keygen -L -f <cert>`.
  - The host key types known_hosts lists for the host are asked for
    first, in file order, then every other type russh supports, as
    OpenSSH does. So a server with several host keys presents one the
    file lists, and known_hosts needs only one of its keys; a server
    whose types the file does not list still fails with the "lists only"
    message.
  - libgit2 sends a local path containing a `:` after a `/`
    (`/srv/a:b.git`) to the SSH transport. agentfs treats such a path as
    local, as git does, so with the `ssh` feature its push fails, closed:
    neither a credential nor a key is offered and no host key is trusted.
- **Tested end to end in CI:** local, `file://`, `http://` and `https://`
  remotes in `ci/e2e-remotes.sh` (nginx and `git http-backend`), and SSH
  remotes in `ci/e2e-ssh.sh` (OpenSSH's sshd with a `git-shell` user and
  key-only authentication): Ed25519 OpenSSH and RSA PEM keys, plain and
  hashed known_hosts, a wrong key, unknown and changed host keys (the key
  is never offered: the sshd log shows no attempt with it), a key rotated
  under a running `agentfs run`, a divergence over SSH, host certificates
  (accepted from a CA-only known_hosts; an unknown or revoked CA, an
  expired certificate and a wrong principal refused before any key is
  offered), ssh-agent identities (accepted, all refused, a dead agent),
  and passphrase-protected OpenSSH, PKCS#8 and RSA PEM keys (missing,
  wrong and rotated passphrases). The transport's own tests
  (`ssh_transport/tests.rs`) run the same cases, plus scp-like URLs, a patch-queue fetch, a missing repository, a
  stalled server and the fail-closed cases, against an in-process russh
  server running the real `git upload-pack` and `git receive-pack`.
  Git LFS (§7) in `ci/e2e-lfs.sh` and `ci/e2e-lfs-transfers.sh` against
  git-lfs's `lfs-test-server` behind the same nginx (with nginx rules that
  throttle or fail chosen objects, and tinyproxy for proxies), and
  `git-lfs-authenticate` in `ci/e2e-lfs-ssh.sh` against OpenSSH's
  git-shell with a custom command.

### 11.2 Interval push

- Pushes run every `pushInterval`, independent of the workload's lifecycle.
  Nothing waits for a pod to stop. The first push of each remote happens as
  soon as the service starts.
- **At shutdown**, after the final commit, every long-running mode
  (`mount`, `run`, `serve-history`) gives each remote that lacks commits
  one more push attempt, all within `--push-grace` (default 5 s) from the
  stop: stopping a pod waits at most that long for a push, never retries,
  and never waits longer. An attempt is only started before the deadline,
  and a remote still inside an attempt then is left (`abandoned`); what
  stays unpushed goes out on the next start. A remote that started no
  attempt is reported as skipped or up to date, never abandoned (shutdown
  waits up to 2 s past the deadline for that local decision).
  `--push-grace 0s` skips the final push. Each remote's result is printed
  (`final push: <remote>: ...`). Without it the
  last commit before a stop would wait for the next start, and under the
  CSI driver, which stops the history service after the last pod (§14.2),
  for the next pod.
- Each remote is pushed from its own thread, so a slow or hung remote cannot
  delay another. libgit2 has no way to cancel a push in progress, so
  shutdown waits for pushes only up to the grace period, then leaves them.
- **Network timeouts.** libgit2 is set process-wide to give up on a
  connection after 30 s and on a stalled read or write after 5 min, and
  the SSH transport (§11.1) applies the same limits itself. A remote
  that accepts the connection and then stops responding ends as an
  `unreachable` failure with the usual alarm and backoff, rather than
  hanging its thread and silencing its alarms; `agentfs push`, which pushes
  remotes in turn, moves on to the next remote.
- Unpushed commits stay safe in the state area and go out on the next
  successful push.
- Pushes are fast-forward only.
- **Push state** is kept per remote in `meta/push/<remote>.json` (§2.2): the
  last pushed commit, the last attempt and success times, consecutive
  failures, the last error and any divergence. It is bookkeeping: a missing
  or unreadable file is treated as empty.
- **A per-remote lock** (`flock` on `meta/push/<remote>.lock`) keeps two
  push attempts on one remote from overlapping. Since the state-area lock
  (§2.3) a direct `agentfs push` can no longer run beside the service (it
  asks the running daemon instead), so this guards only pushes within one
  process.
- **Backoff.** After a failure the next attempt waits `pushInterval` doubled
  for each consecutive failure after the first, up to
  `max(pushInterval, 15m)`. A success resets it.
- **Push now.** `agentfs push` against a running daemon (§2.3) commits
  what is recorded and wakes each chosen remote's loop at once, past its
  interval and any backoff; the attempt then counts like any other (a
  failure doubles the backoff, a success resets it).

### 11.3 Limits and rejected pushes

- **Limits are configured per remote.** `remote.<name>.agentfsMaxBlobSize`:
  commits are checked against it before they are made (§3.4); because one
  history goes to every remote, the smallest limit applies. The same goes
  for `remote.<name>.agentfsMaxLfsObjectSize`, a limit per LFS object
  (§7), which each remote also checks on the objects it would upload
  before its push. There is no per-push budget for LFS bytes: objects are
  uploaded one by one, outside the git pack, and an interrupted upload
  resumes per object (§7), so a push-size limit would have nothing to
  step. git's fsck
  rules are always enforced, not per remote: history may later go to a
  stricter remote, and a commit no remote accepts can never be pushed.
- **A maximum push size** (`remote.<name>.agentfsMaxPushSize`, a positive
  size, decided 2026-10-13) keeps each push under a host's limit:
  - Before pushing, the attempt lists the remote's branch, packs the
    commits since it with libgit2's pack builder and counts the bytes,
    stopping once the limit is passed (`agentfs_core::push_size`). Only
    the git pack counts; LFS uploads (§7) go to their own servers.
  - A backlog that fits is pushed as usual.
  - A backlog that does not fit (one that grew during an outage would
    otherwise never go out) is pushed in **steps**: the longest prefix of
    the first-parent chain from the last pushed commit that fits, found by
    bisection. A step pushes the branch alone, uploads the LFS objects of
    its commits only, holds the diverged refs and the patch queue's
    `applied/<n>` refs back for the full push, raises `PushLimited` (a
    warning, §13) with the commits left, and makes the next attempt
    follow at once instead of after `pushInterval`. `agentfs push` keeps
    stepping until the remote is up to date. A divergence found during a
    step saves the step's commit, not the head.
  - A single commit over the limit cannot be stepped, nor can a backlog
    whose last pushed commit is not on the head's first-parent chain:
    the attempt fails as `too_large` (`PushFailed`, an error, with
    backoff) and nothing is sent. The operator raises the limit, or sends
    large files to LFS from then on; a commit already made can only go
    out by rewriting history (§15).
  - The pack libgit2 sends can differ slightly from the measured one
    (libgit2 negotiates against every ref the remote advertises).
    Measuring costs one more connection (the listing) and a second pack
    computation per attempt, and a few more while stepping; remotes
    without a limit measure
    nothing.
  - The branch the remote advertises is the base when this repository
    has it and it is the head or one of its ancestors, and it is recorded
    as the last pushed commit: the push state can lag the remote (deleted
    by the history-rewrite runbook, a renamed remote, a crash between a
    push and the state save), and stepping from behind the remote would
    push commits it already has over newer ones, a false divergence at
    every attempt. A remote without the branch counts everything; one
    that cannot be listed, or holds a commit unknown here or not an
    ancestor, falls back to the push state, and the push itself reports
    the failure or divergence.
- **A rejected push never affects the filesystem.** Rejected means refused,
  non-fast-forward, authentication failure, an untrusted host (an SSH host
  key or TLS certificate that fails verification), an unreachable remote,
  or any of these while uploading LFS objects before the git push (stage
  `lfs`, §7: no ref is pushed then).
  Recording, committing and other remotes carry on. The recorder:
  - raises an alarm (`PushFailed`, §13) with the reason and the commit;
  - keeps retrying with backoff (§11.2);
  - keeps the remote's last successful commit, so the backlog is visible.
- **A worktree creates or fast-forwards its own branch, as the remote shows
  it** (one branch per worktree, F8 rule 2, 2026-10-16). In libgit2's push
  negotiation, from the old value the remote reports for the branch in the
  very connection that sends the update, `push::claim` decides: no such
  branch is a create (sent with a zero old value), the head itself is up
  to date, a commit known here and an ancestor of the head is a
  fast-forward (leased on that value by the smart protocol's old-value
  check); anything else, a commit unknown here or not an ancestor, is
  someone else's, and the negotiation stops the push before any ref or
  object is sent. The push record is never an input: it stays
  bookkeeping (§11.2). libgit2's local and `file://` transports update an
  existing ref without compare-and-swap, so a writer racing between the
  negotiation and the update there can still be overwritten; smart HTTP
  and SSH servers compare and swap.
- **Non-fast-forward on the branch means someone else wrote it.**
  - The recorder pushes its head to
    `refs/agentfs/<volume>/diverged/<UTC timestamp, no colons>`
    (`YYYYMMDDTHHMMSSZ`) once per divergence, raising `Diverged`, then keeps
    raising `PushFailed` (`non_fast_forward`) at every attempt, with the same
    backoff as other failures.
  - The name is chosen and saved in the remote's push state as pending
    before that push, and confirmed after it. A crash in between reuses the
    pending name at the next attempt (the same head, or a fast-forward of
    it), so a divergence is saved under one name; only `Diverged` may be
    raised again after such a crash.
  - It never force-pushes.
  - An operator resolves it (§15,
    [divergence.md](runbooks/divergence.md)). The divergence clears when a
    branch push next succeeds.
  - The branch push carries only the branch and the `diverged/*` and
    `accepted/*` refs, both create-only (they never move). A
    non-fast-forward on one is reported like any other and saves
    nothing.
- **The start-branch alarm** (F8 R9, 2026-10-16). A volume started from
  another worktree's branch (a start class, §14.2) keeps its start point
  in its identity (`agentfs.start.remote`, `.branch`, `.commit`). At
  every push attempt to the start remote it lists the start branch first,
  and `push::start_branch_moved` decides: the branch holding commits this
  volume does not have (the tip unknown here, or not an ancestor of the
  head or of one of its `refs/agentfs/<volume>/accepted/*` refs) raises
  `StartBranchMoved` (an error, like `Diverged`, §13), naming the branch,
  the pinned commit and the tip, at every such attempt while it holds; it
  is kept in that remote's push state for `agentfs status` (`start branch
  moved: <pinned>..<tip>`) and the gauge `agentfs.start.moved`. It never
  stops a push: the new volume writes only its own branch. This is how an
  old node's late commits, which its release scan pushes to the old
  branch after a move, are saved and alarmed, never overwritten: the new
  volume raises the alarm; the old node needs no knowledge of the move.
  The operator takes the commits in (a patch, or by hand) or decides
  against them, then `agentfs start accept --tip <tip>`, which fetches the
  start branch, checks the tip is on it and creates
  `refs/agentfs/<volume>/accepted/<tip>` (never moved), pushed
  create-only with the branch: the alarm stops for every commit the tip
  reaches, comes back if the old branch moves again, and the old commits
  are saved under the new volume's name too
  ([runbook](runbooks/start-branch-moved.md)). Not covered: the alarm
  waits for the new volume's next push and watches only the start remote;
  nothing is fetched to raise it.
- **The patch queue has its own push** (§12), to the patch remote only,
  after every branch push attempt, failed ones included (an LFS upload
  failure, a refused or unreachable branch), under the same lock, so the
  status reaches the remote even while the branch cannot:
  1. the status ref, forced (the only ref agentfs ever force-pushes: it is
     a snapshot), and, only when the branch push succeeded, every
     `applied/<n>` the remote lacks, never forced;
  2. then, once that succeeded and only when the branch push did, the
     deletion of each remote `inbox/<n>` whose number is applied (the
     applied patch, or a stale replacement the queue ignores), leased on
     the oid listed and checked again against libgit2's push negotiation,
     so a ref a producer wrote meanwhile stays.

  libgit2 rejects a whole push when one ref is refused, which is why the
  two are apart: a queue push failure raises `PushFailed` with stage
  `queue`, is kept as the remote's `queue_error` and makes `agentfs push`
  exit 1, but never counts toward the branch's failures, backoff or
  divergence. It is skipped while the status pushed last is still the
  local one.
- libgit2's local transport updates an existing remote ref without
  compare-and-swap, so another writer moving a local or `file://` remote
  between libgit2's ref listing and its update can be overwritten. Smart
  HTTP and SSH servers compare and swap.

### 11.4 Shared repositories

Several volumes may share one repository. Each volume has a name (`{volume}`,
set by the operator) and owns a branch and a ref namespace in that repository:

| Ref | Purpose |
|---|---|
| `refs/heads/agentfs/<volume>` | The volume's history (configurable: `[agentfs] branch`) |
| `refs/agentfs/<volume>/inbox/<n>` | Patch queue (§12): `<n>` is ten zero-padded digits from `0000000001`; written by producers |
| `refs/agentfs/<volume>/applied/<n>` | The patch commit applied as `<n>`; pushed before `inbox/<n>` is deleted on the remote |
| `refs/agentfs/<volume>/status` | The queue's status for iac (§12): a parentless commit with `status.json`, force-pushed |
| `refs/agentfs/<volume>/diverged/<timestamp>` | Divergence (§11.3) |
| `refs/heads/agentfs/<fork>` | A fork's history (§11.5), on its fork remote; fixed at create |
| `refs/agentfs/<fork>/change` | A fork's change, one commit on base; pushed create-only, never moved |
| `refs/agentfs/<fork>/finish` | A fork's finish commit; pushed create-only, never moved |

**Pushing.** A recorder pushes only its own volume's refs, with explicit
refspecs, so pushes from different volumes never contend: the branch and
`diverged/*`, and to the patch remote the status and `applied/*`, then
deletions of `inbox/*` (§11.3). It never pushes an inbox ref.

**Fetching.** At mount it fetches only its own volume's `inbox/*` from the
patch remote (§12), and deletes local inbox refs the remote no longer has.

**Single writer.** Each recorder is the only intended writer of its volume's
refs. Remotes cannot always enforce that:

- GitHub can restrict pushers only on organisation-owned repositories.
- Tokens are scoped to repositories, not branches.
- Every volume sharing a repository can therefore write every other volume's
  refs.

agentfs **detects** other writers (§11.3) rather than relying on enforcement.

**Volume names** must be unique within a repository. Two recorders configured
with the same name would see each other as divergence.

**Names.** Volume and remote names must match `[A-Za-z0-9][A-Za-z0-9._-]*`,
without `..` and not ending in `.` or `.lock`. They become ref components and
state file names; a `/` in a volume name would let one volume's
`refs/agentfs/<volume>/` namespace contain another's.

### 11.5 Forks

A **fork** is a volume started from another volume's latest local commit:
its **source**, at its **base** (§19.1 "Forked volumes"). It records,
pushes and recovers like any volume. When it **finishes** (its claim is
deleted under CSI, or `agentfs fork finish`), it publishes one **change**,
a commit whose parent is base and whose tree is the fork's final tree,
and whatever receives the change does all the gating: agentfs never
reviews, accepts or merges.

**Identity.** `agentfs.fork.source` and `agentfs.fork.base` sit in the
fork's own state-area config beside `agentfs.volume`, written once at
create; no include may set them (or `agentfs.fork.pushTarget`), and a
fork's config may not set `agentfs.branch`. `config::Settings` is the one
decider of a fork's refs and remotes. Every other setting comes from the
source's settings files, which the fork includes and reads at every open.

**The fork remote.** Where forks push: the one valid remote section with
`remote.<name>.agentfsForkRemote = true`, else the source's patch remote
(§12; §19.1 "Fork remote", gate answer b). A flagged section is never one
of the source's own push remotes, never sets its limits (§3.4) and never
becomes its patch remote, so a source never pushes there. Two flagged
sections, or one that is invalid, leave no fork remote: forks are refused
rather than fall back. While `lfs.url` or `lfs.pushurl` (the source's LFS
store) is set, a flagged section must name its own `lfsurl` or
`lfspushurl`, which it reads before the global keys; without one it is
invalid, so a fork's LFS objects never reach the source's store. The
default fork remote, the source's own, is unaffected. A fork pushes only to its fork remote, at
its push target, and has no patch queue.

**The push target** is where a fork's history is kept and pushed, fixed
at create in its identity (`agentfs.fork.pushTarget`, §19.1 "Hidden
forks"): `branch` (the default, unset) is `refs/heads/agentfs/<fork>`,
where users' CI and pull requests can act on it; `hidden` is
`refs/agentfs/<fork>/history`, never under `refs/heads/` on any remote,
as forges keep a pull request's head under `refs/pull/`. Create gives a
new fork the source's `agentfs.forkRefs` (default `branch`); changing it
moves no fork that exists, since a fork reads its target only from its
own identity. Every consumer of the fork's history ref (pushes, restore,
status, the finisher, `fork list` and `fork deliver`) takes either name.
Whether a forge runs CI or webhooks on a push to `refs/agentfs/*` is the
forge's own behaviour, not verified here (§19.2).

**Create** (`fork::create`) reads the source's settings and branch head
without its lock and with no credentials, and refuses a source with no
history, one that is a fork (forks of forks are deferred) and one with no
fork remote. It then writes the fork's state area: its identity, the
source's settings files as includes, the source's `info/exclude` and
`info/attributes`, every object reachable from base packed into the
fork's own object store (no alternates, so a fork outlives its source),
the fork's branch at base, and base's tree written into the backing tree
through the restore engine's confined writer (no symlink followed, nothing
recorded), with LFS objects copied from the source's cache. Each step is
idempotent per name: a retry resumes, and a second create of the same
fork returns the base it was created with. A CLI source must keep its
settings in included files (`SourceSettingsNotIncluded` otherwise), since
the fork reads only those; a CSI source's are its profile's.

**Finish** (`fork::finish::Finisher`, only under the fork's state lock)
commits what is journaled, records from the tree the paths every repair
queue left (`meta/repairs`, and each front slot's
`meta/fronts/slot-<n>.repairs` with a crashed rewrite's temporary file,
which under CSI no next front would drain; each entry as the pod that
queued it, and each one that fails, or a queue that cannot be read,
counted and alarmed as `RecordFailed`), then
writes the change (none when the final tree is base's) with
`refs/agentfs/<fork>/change`, the finish commit (§9) as the branch head,
and `refs/agentfs/<fork>/finish`. The finish commit is the single home of
base, change and fork remote. A second finish returns the same record; a
crash between any two writes converges. With no fork remote, finish
writes nothing and the fork stays active.

**Publish** retries with §11.2 backoff: the history push to the fork
remote (a branch a forge moved is §11.3 divergence: the head is saved
under `diverged/` and alarmed, and it never blocks publishing), then the
change and finish refs, **create-only**: never `+`, a ref the remote holds
at another id is another writer's and is never moved, and a listing after
the push confirms both refs at exactly their ids. Only then is the
published mark written to `meta/fork.json`. Before every attempt the
settings and the mark are read again.

**Refused.** One function, `fork::finish::refused_reason`, decides what is
permanent (§19.1 "Refused publish"): a create-only ref the remote holds
at another id (`another_writer`); the create-only push refused by the
remote, such as a pre-receive hook on `refs/agentfs/*` (`rejected`); one
commit over `agentfsMaxPushSize`, or one LFS object over
`agentfsMaxLfsObjectSize`, in either push (`too_large`, §11.3); and a
finish naming a remote that is no longer the fork's valid fork remote
(`remote_gone`). Everything else (unreachable, auth, an untrusted host, a
busy remote, a refused branch push, a moved branch) is pending and retried
with backoff. A refusal writes the `refused` mark (finish, remote, reason,
message, time) and raises `ForkRefused` once; the fork is `refused`, a
publish of it returns the refusal without pushing, and the node plugin's
release scan never starts a finisher for it, nor pushes it. `agentfs fork finish --retry`
clears the mark and publishes again.

**Abandoned.** `agentfs fork abandon --reason TEXT`
(`fork::finish::abandon`, under the state lock, exit 75 while it is held)
records the operator's discard in `meta/fork.json`: the reason, the time,
the operator's uid and the state it was in. It is refused for a published
fork (nothing to discard) and without a reason. After it, `Finisher::open`,
`finish` and every publish attempt read the mark first and stop with
`Abandoned`, so no finisher pushes the fork again; DeleteVolume applies
`onDelete`. A push already running finishes or times out (libgit2 cannot
cancel one, §11.2), and a fork remote may still receive it. Abandon does
not stop a fork that is mounted again: its interval pushes carry on as for
any volume.

**Status** (`fork::finish::status`, `agentfs fork status [--json]`, no
lock, no credentials, no network): `abandoned` wins, then `published` and
then `refused` when their marks name the branch head's finish commit, then
`finished` after the finish commit and `active` before it. A mark that
cannot be read, or settings that cannot be loaded, are errors, never a
state, so DeleteVolume waits. Whether a volume is a fork is read first
from its own `git/config` (the identity keys, which no include may set),
so an ordinary volume whose settings fail to load has no fork status and
its DeleteVolume never reads them.

**Settings that fail closed.** A fork whose included settings file cannot
be read does not open: `mount`, `fork finish`, `fork status` and
DeleteVolume fail naming the file (`IncludeUnreadable`). A fork with no
valid fork remote opens (recording keeps working, principle 5), raises
`RemoteMisconfigured` for the flagged section, and `fork finish` writes
nothing (`NoForkRemote`).

**Read-only after finish.** A fork whose branch head is its finish commit
takes no changes: a `Recorder` opened on it refuses every record, move and
operation (`ForkFinished`), and `mount`, `run`, `record`, `restore`,
`revert` and `patch apply` refuse it before they change anything; the
finisher and status still open it. Under CSI its history service still
starts; its fronts FUSE-mount it read-only (`ro`), so the kernel refuses
every write with `EROFS` through any bind of it, and NodePublish
bind-mounts it read-only whatever it asked (§14.2). The bind alone is not
enough: a container runtime may bind the target into the container
read-write again.

**Listing** (`fork::listing`, `agentfs fork list`) runs in a receiver's
repository with its own git and credentials, like `patch submit`, and
never reads a state area: one `ls-remote`, one fetch of the finish and
change refs (no ref written but `FETCH_HEAD`). A change is listed only if
its fork name is a valid volume name, its finish commit's last paragraph
parses with `Agentfs-Fork` equal to it, the change ref is the
`Agentfs-Change` it names, the change has exactly one parent, `Agentfs-Base`,
and its tree is the finish commit's; anything else is skipped with why.
A listing is only as trustworthy as the remote's writers: whoever may
write `refs/agentfs/*` there can publish a change that passes.

**Delivery (library and CLI).** `fork::deliver` and `agentfs fork
deliver <fork> --to <method>` hand a published change to a receiver
(§19.1 "Fork delivery"). They run in the receiver's repository with its
own git and credentials, on the change `fork::listing::read` checked and
fetched (a fork with none, or one failing a check, is "not published",
exit 1). One contract, `Delivery::deliver(change)`, has four
single-responsibility methods, and one factory, `fork::deliver::delivery`,
is the only place a method is built and validated (a volume name, a
non-empty argv, an absolute credential file, an `http(s)` URL, a prefix
that makes valid branch names, no `onto` with the history shape):

| Method | Delivers | Idempotency |
|---|---|---|
| `inbox` | `patch submit` of the change commit into the inbox of the volume the runner names (`--volume`, never the change's `Agentfs-Source`), on `--remote` | At most once: the remote already queuing or having applied the commit is `AlreadyDelivered` (`AlreadySubmitted`, §12), and the source's duplicate rule is the guarantee |
| `branch` | A pull request's head, `refs/heads/<prefix><fork>` (default prefix `agentfs-review/`), created only if absent (`--force-with-lease=<ref>:`, never a force). `squash`: one commit, the change's tree on base, or with `--onto B` the three-way merge (against base) of B's head and the change on B's head, a conflict refused with its paths. `history`: the fork's history as published, its finish commit | Content-based: the squash commit is deterministic (the change commit's author and committer, the message `change of fork <fork>` with `Agentfs-Change: <change>`), so the ref already at exactly the commit this delivery creates is `AlreadyDelivered`; a ref at any other commit, even one carrying the trailer, is refused and never moved |
| `command` | `argv` run without a shell, the payload as JSON on stdin, an environment of `PATH` alone, in its own process group, killed with it after `--timeout` (default 60s) | At least once: exit 0 accepted, 75 unavailable, anything else or a signal refused, a timeout unavailable |
| `webhook` | One POST of the payload as `application/json` with `Idempotency-Key: <change>` and, with `--credential-file`, `Authorization: Bearer <its contents>`; no redirect followed, never a proxy from the environment, `--timeout` | At least once: 2xx accepted, 409 already delivered, 429, 5xx, a timeout or a connection failure unavailable, any other status refused |

The **payload**, schema 1 (`DeliveryPayload`): `schema`, `fork`,
`source`, `base`, `change` (receivers dedupe on it), `finish`, and
`fetch` (`remote`, as the runner named it, and the `change`, `finish` and
`history` refs). A receiver that dedupes on `change` makes the
at-least-once methods at most once. `fork deliver` exits 0 delivered or
already delivered, 1 refused, and 75 unavailable (a receiver, or a remote
unreachable, or an inbox number lost to racing submitters, all
`DeliveryError::is_retryable`): run it again.

**The CSI driver never delivers** (gate answer "2 a for CSI and b for
the library"): it publishes, and delivery runs only where an implementer
runs it, with that runner's credentials and settings; the target is never
read from the change. A test greps the CSI sources for any use. **The
exposure:** a delivery pushes a sandbox's unreviewed output into a target
repository (a branch on `--remote`, or a volume's inbox), where that
repository's own CI and hooks act on it; the branch method only creates
refs under its prefix and the inbox method only queues, but what runs on a
new branch is the repository's affair (§19.1 "Fork content").

**Operations.** A refused publish has its runbook,
[fork-refused.md](runbooks/fork-refused.md), one section per reason;
`fork finish` and `fork status` link the section. A history rewrite of a
source covers its forks ([§15](#15-recovery-and-undo)). The finisher's
spans, metric and events are in §13.

## 12. Patch queue for iac (phase 3, built)

Producers outside the volume (iac, an operator, CI) change a volume's tree
through history: they queue patches, and the recorder applies them, in
order, before anything can see the tree. Runbook:
[patch-queue.md](runbooks/patch-queue.md).

**What a patch is.** A single commit with exactly one parent, the branch
commit it was written against (the head or any ancestor of it), pushed to `refs/agentfs/<volume>/inbox/<n>` on the volume's
patch remote. `<n>` is ten zero-padded digits from `0000000001`; any other
name under `inbox/` is ignored and reported. A patch may change at most
4096 paths, only regular files and symlinks.

**Submitting** (`agentfs patch submit`, or plain git; §19.1). The producer
runs it in its own git repository, and it drives the producer's `git`, so
the repository's remote, credential helpers and SSH setup apply:

1. `git ls-remote` lists the branch, `inbox/*` and `applied/*`;
2. `git fetch` brings the head (`--depth` for shallow producers);
3. the patch is a given commit (its parent must be on the branch), or one
   built on the remote head from `--put PATH=FILE` and `--delete PATH` in a
   temporary index, with `-m` and `--author`;
4. its number is one past the highest inbox or applied number listed;
5. `git push --force-with-lease=<ref>: <oid>:<ref>` creates the ref only if
   it does not exist. A submitter that loses the race lists again and tries
   the next number (`--attempts`, default 10).

A given commit taking a new number that is already at an `inbox/*` or
`applied/*` ref, or is the status ref's `last_applied` patch, is refused
before anything is pushed (`AlreadySubmitted`: a change lands at most
once). That is an early refusal only: two submits at once both pass it,
and the duplicate rule below is the guarantee.

`--replace N` swaps a queued patch, leased on the oid listed;
`--skip N` replaces it with an empty commit on the head, so the queue moves
on. Both also fill a **gap**: a number below the highest one listed that
the remote has neither as `inbox/N` nor as `applied/N` is pushed
create-only (an empty lease), which is how a `gap` block is cleared. Both
refuse a number the volume has applied: the remote's `applied/N`, or
`last_applied` in its status ref, which reaches the remote even while the
applied refs are held back (a replacement would only be ignored as
`stale`). Applied refs reach the remote before inbox refs are deleted
there, so the highest number listed never goes down and a number is never
reused.

**When it runs.** At mount, after journal replay and before the service
and the FUSE layer start; `agentfs patch apply` does the same with the
volume stopped. `agentfs run` does not. In order:

1. commit what is journaled, so the head is the tree's state;
2. fetch `inbox/*` from the patch remote: forced, this volume's only, local
   inbox refs the remote no longer has are pruned. Best effort, within
   `agentfs.patchFetchTimeout` (default 30s; 0 or `off` skips it); a failure raises
   `PatchFetchFailed` and the queue runs with what is already fetched;
3. scan the branch for applied patches (below);
4. apply the queue, in order, from the last applied number plus one;
5. move the refs and write the status.

**Applying a patch.**

- **Merge:** libgit2's cherry-pick of the patch onto the head, a three-way
  merge against the patch's parent.
- **Write:** the merged tree is written into the backing tree by the
  restore engine (§15): plan the paths whose head and merged entries
  differ, check them (an unrecorded change or something in the way
  refuses), write them, LFS content from the cache or the remotes (§7).
- **Record:** one atomic record of an operation of kind `patch` (§3.4, §8),
  so it becomes one commit, never grouped with workload changes. Its author
  is the patch's author (name, email, time); its committer is the replica's
  identity (§9). It carries `Agentfs-Trigger: patch`, `Agentfs-Patch: <patch
  oid>` and `Agentfs-Inbox: <n>`; its message is the patch's, without a
  final trailer paragraph.
- **A patch that changes nothing** (its content is already there) gets a
  marker commit with the head's tree and the same trailers (§3.4's
  exceptions).
- **A duplicate** (§19.1 "Patch duplicates"): when anything is queued, the
  run scans the whole first-parent history once; before applying inbox
  `n`, if an earlier number applied the same patch commit, `n` is consumed
  with a marker commit with the head's tree carrying `Agentfs-Inbox: <n>`
  and the subject `duplicate of patch <m>: <summary>`, its refs move, and
  `PatchDuplicate` is raised; nothing is merged or written, so a change the
  workload reverted since stays reverted. `status.json` lists these under
  `duplicate_now`. The branch's trailers still decide, so every crash
  point converges.
- **Refused at record or commit time** (a commit-time check drops the
  atomic record), the patch's writes are put back and the queue blocks.
  **A write that fails part way** is recorded as it stands, as an
  operation without an inbox number, so the tree matches history again.

**The first patch that cannot be applied blocks the queue.** It and every
later patch stay queued, `PatchBlocked` is raised, and the volume mounts
anyway. Reasons:

| Reason | When |
|---|---|
| `conflict` | The merge conflicts; the paths are listed |
| `gap` | The next number is missing while later ones are queued |
| `parent_not_in_history` | The parent is not the head or one of its ancestors |
| `merge_commit` | More than one parent |
| `unsupported` | A root commit, a gitlink, a path agentfs does not record (§5) |
| `too_large` | More than 4096 paths |
| `unrecorded` | The tree differs from history where the patch writes (an unrecorded change, a pending repair, something in the way) |
| `failed` | Writing or recording failed, or an LFS object is missing |

The producer fixes it by replacing the patch (`--replace N`, rebased onto
the head) or skipping it (`--skip N`); a `gap` is filled the same way
(`--replace N` with the missing patch, or `--skip N`). An `unrecorded`
block is the operator's: record the paths named (`agentfs record`), or
finish the repair or restore that left them, since the mount commits only
what is journaled. The next mount, or `agentfs patch apply`, carries on.

**A queue that cannot run at all** (the history or the state area cannot
be read, the tree or the LFS cache cannot be opened, a ref cannot be
written) raises `PatchBlocked` with reason `failed` and inbox 0, and is
recorded as blocked in `meta/patch.json` and, when it can be written, a new
status commit, so `agentfs.patch.blocked` and `agentfs status` show it
rather than the previous run's state. The volume mounts anyway.

**Idempotency.** Whether patch `<n>` is applied is decided by the branch:
the newest first-parent commit whose last message paragraph carries
`Agentfs-Trigger: patch`, `Agentfs-Patch` and `Agentfs-Inbox: <n>`.
`meta/patch.json` caches the scan (the head it reached), never
authoritatively. Refs move only after the commit, so every crash converges
on exactly one commit per patch:

| Crash after | Next run |
|---|---|
| the tree write, before the record | the check finds the patch's content already there and records it without writing |
| the tree write, before the record, and the producer then replaced, skipped or withdrew the patch | the patch being written is noted in `meta/patch.json` (`in_flight`: number, patch, head, paths) before the write; the run puts those paths back to the head's state (or, if it cannot, records them as they stand, as a `patch` operation without an inbox number) before applying anything |
| the record, before the commit | journal replay commits it; the scan finds it |
| the commit, before the ref moves | the scan finds it; the refs are moved |

An inbox entry at or below the last applied number is bookkeeping when it
names the applied patch. Otherwise it is `stale`: reported under
`ignored`, raised as the warning `PatchIgnored`, never applied, and deleted
on the remote by the next queue push.

**Refs.** `applied/<n>` is the patch commit itself; `inbox/<n>` is deleted
locally once `applied/<n>` is written, and on the remote by the queue push
(§11.3) once `applied/<n>` is there, whatever patch it names then (leased
on the oid listed).

**Status.** `refs/agentfs/<volume>/status` is a parentless commit holding
`status.json`, rewritten at every run and force-pushed: a snapshot, not
history. Schema version 1 (fields in the runbook): the volume, branch and
head, `updated`, `last_applied` (inbox, patch, commit), `applied_now`,
`duplicate_now` (inbox, patch, applied_as; absent when empty), `queue` (inbox, patch; the blocked one first), `blocked` (inbox, patch,
reason, message, paths), `ignored` and `fetch` (remote, ok, error).
`agentfs status` prints a `patches:` line from `meta/patch.json`, and
`agentfs patch status [--json]` the whole status.

**Telemetry** (§13): the gauges `agentfs.patch.queue_depth` and
`agentfs.patch.blocked`, an `agentfs.patch` span per patch, and the events
`PatchApplied`, `PatchBlocked`, `PatchIgnored` and `PatchFetchFailed`.

**The patch remote** is `agentfs.patchRemote`: a remote's name, or `none`.
By default it is the only remote when exactly one is configured, else
none: no fetch and no queue push, while patches already in the local
inbox still apply. A name that matches no valid remote also means none.

## 13. Observability (OpenTelemetry)

The recorder exports metrics, traces and logs over OTLP.
`agentfs_core::telemetry` instruments the engine against the OpenTelemetry
API only; the binary picks the SDK and the exporter.

**Metrics:**

| Metric | Type | Use |
|---|---|---|
| `agentfs.record.duration` | histogram | Cost of the synchronous step, by trigger |
| `agentfs.record.failures` | counter | Recording errors (§3.5), by trigger |
| `agentfs.journal.backlog` | gauge | Entries recorded but not committed |
| `agentfs.commit.count` | counter | Commits made |
| `agentfs.commit.failures` | counter | Commit passes that failed; the records stay journaled |
| `agentfs.repack.failures` | counter | Periodic repacks that failed (§8) |
| `agentfs.push.unpushed_commits` | gauge, per remote | Backlog |
| `agentfs.push.oldest_unpushed_age` | gauge, per remote | Backlog age, the main push alarm |
| `agentfs.push.failures` | counter, per remote, reason and stage | Rejected, non-fast-forward, authentication, untrusted host, network, too large (`too_large`, §11.3); stage `git`, `lfs` for the LFS upload before the git push (§7), or `queue` for the patch remote's queue refs (§11.3, §12) |
| `agentfs.push.diverged` | gauge, per remote | 1 while diverged |
| `agentfs.start.moved` | gauge, per remote (the start remote of a started volume) | 1 while the start-branch alarm holds, as the last push to that remote saw it; 0 before the first push and after one that does not raise it (§11.3) |
| `agentfs.push.limited` | counter, per remote | Pushes that sent only part of the backlog because it was over `agentfsMaxPushSize` (§11.3) |
| `agentfs.remote.misconfigured` | gauge, per remote | 1 for each invalid `[remote]` section (§11.1) |
| `agentfs.precheck.rejected_paths` | counter | Paths refused by fsck or limit checks, by stage and reason |
| `agentfs.patch.queue_depth` | gauge | Patches queued after the last applied one, the blocked one included (§12) |
| `agentfs.patch.blocked` | gauge | 1 while the patch queue is blocked (§12) |
| `agentfs.state.bytes` | gauge | Size of the state area by part |
| `agentfs.telemetry.export_failures` | counter, per signal | Telemetry exports (traces, metrics, logs) that failed; counted by the binary, exported once metrics get through |
| `agentfs.front.round_trip` | histogram, by request | A front's round trip to the history service (`record`, `tracked`, `ping`), in seconds (§14) |
| `agentfs.front.service_failures` | counter, by stage | A front's failed requests to the history service: `connect`, `timeout`, `request` (connection lost or malformed reply), `refused`, `service` (§3.5, §14) |
| `agentfs.history_service.fronts` | gauge | Fronts connected to this history service (§14) |
| `agentfs.history_service.superseded` | counter | Front writes the history service skipped because a newer state of the path was already journaled (freeze stamps, §14.1) |
| `agentfs.history_service.resyncs` | counter | Paths the history service recorded again as the backing tree has them, after a front's change it could not order (identical stamps from two slots, a late removal or symlink, §14.1) |
| `agentfs.fuse.create_as_caller_fallbacks` | counter | Objects a root daemon created as itself and then gave to the caller, because creating as the caller was refused (a device node, or a caller whose groups are unknown) or could not be set up (§6) |
| `agentfs.fuse.racy_rechecks` | counter, per outcome | Boundaries that would have recorded a file again only because its last record was racy: `skipped` when its bytes were that record's frozen copy, still the last state journaled at its path, else `recorded` (§3.1) |
| `agentfs.reconcile.records` | counter | Records made reconciling moved directories (§3.1) |
| `agentfs.reconcile.failures` | counter | Moved directories given up on after 5 failed reconciliations over at least 30 s (`reconcile_failed`, §3.1) |
| `agentfs.nested.gitdir_heuristic` | counter | Absolute `gitdir:` or `.git` symlink targets under no known view of the tree, found by trying their suffixes (§5); set `--workload-mount` (CSI: `workloadMounts`) |
| `agentfs.fork.publish.duration` | histogram, per remote | Seconds from a fork's finish commit to its fork remote holding the change and finish refs (§11.5): one point per fork, when the published mark is written |

Metric attributes are short and low-cardinality: `trigger`, `remote`,
`reason` (a refusal label or a push failure kind), `stage` (`record` or
`commit` for refused paths, `git`, `lfs` or `queue` for push failures) and `part`
(`git`, `journal`, `pending`, `lfs`, `meta`). Paths never become metric
attributes. Span and log attributes are namespaced `agentfs.*`.

`agentfs.state.bytes` is the apparent size (the sum of file lengths) of each
part, not allocated blocks: reflinks make both misleading, and apparent size
is deterministic. The journal backlog and the push gauges are read when
metrics are collected: the backlog from the open journal; unpushed commits,
oldest unpushed age (seconds since the oldest unpushed commit's committer
time, 0 when none) and divergence from each remote's push state plus a walk
of the branch, shared by the three push gauges in one collection.

**Traces.** Spans `agentfs.record` (trigger, inputs, caller uid and pid, the
sequence number when journaled, paths skipped), `agentfs.commit` (first and
last sequence numbers, records, paths refused, the commit id or
`agentfs.commit.empty`) and `agentfs.push` (remote, previous and new head,
outcome, on failure the reason, the stage and any diverged ref, and when
the backlog references LFS objects `agentfs.lfs.objects`,
`agentfs.lfs.uploaded` and `agentfs.lfs.bytes`). Each span is
current while its operation runs, so alarm logs raised inside it carry its
trace. Record and commit spans are not linked; they correlate through the
sequence numbers. `agentfs.patch` covers one patch at mount (inbox number,
patch oid, outcome, the commit or the block reason, §12); a push to the
patch remote carries the queue push's result as `agentfs.push.queue`.
`agentfs.fork.finish` covers writing a fork's change and finish commit,
once (`agentfs.fork`, `agentfs.fork.change` when there is one,
`agentfs.fork.unrecorded`), and `agentfs.fork.publish` one publish call
(`agentfs.fork`, `agentfs.remote`, `agentfs.fork.publish.outcome`:
`published`, `pending` or `refused`, and `agentfs.fork.refused.reason`),
with its `agentfs.push` spans inside it (§11.5). `agentfs fork finish`
exports like `push`, so the CSI node plugin's finisher children export
too.

**Logs.** A structured log record for every alarm condition, and at INFO
for a fork's lifecycle (`ForkFinished`, `ForkPublished`, `ForkAbandoned`:
`Event::is_logged`): the event name
(`agentfs.path_refused`, `agentfs.push_failed`, ...), WARN for a refused
path, a failed patch fetch, an ignored patch, an identity fallback and a
limited push, and ERROR for every other alarm, the event's message as the body and its fields
(path, reason, remote, kind, heads, ...) as attributes; a push failure
carries its stage as `agentfs.push.stage`.

**Events.** The engine reports alarm conditions as events to a sink
(`agentfs_core::events::EventSink`). The telemetry layer wraps the
process's sink once (`Telemetry::sink`), turning events into the counters
and logs above; the CLI also prints alarms to stderr.

| Event | Raised when |
|---|---|
| `PathRefused` | A path fails the checks of §3.4 or §5, at record or commit time, with the reason's label (for example `blob_too_large`, `lfs_object_too_large`, `max_size`) |
| `RecordFailed` | The synchronous step fails (§3.5) |
| `CommitFailed` | Turning records into a commit fails; the records stay journaled and are retried |
| `ReconcileFailed` | Reconciling a moved directory (§3.1) failed 5 times over at least 30 s, with both paths and the error: the move is committed as journaled, and files beneath the destination that recording the tree would have added, dropped or recorded again differ from the tree until each changes again or is recorded with `agentfs record` |
| `PushFailed` | A push attempt fails, with the remote, the kind (`unreachable`, `auth`, `untrusted_host`, `non_fast_forward`, `refused`, `local`, `too_large`), the stage (`git`, or `lfs` when uploading LFS objects failed and no ref was pushed, §7; `too_large` at stage `lfs` is an object over the remote's `agentfsMaxLfsObjectSize`), the head, the last pushed commit, the consecutive failure count and the next retry; stage `queue` for the patch remote's status, applied and inbox refs (§11.3), which never counts against the branch |
| `Diverged` | The local head was saved under a diverged ref (§11.3) |
| `StartBranchMoved` | A started volume's start branch holds commits it does not have: the start remote, the branch, the pinned commit and the tip, at every push to the start remote while it holds; an error, like `Diverged`; log event `agentfs.start_branch_moved` with `agentfs.remote`, `agentfs.start.ref`, `agentfs.start.pinned`, `agentfs.start.tip` (§11.3) |
| `PushLimited` | A push sent only part of the backlog because it was over the remote's `agentfsMaxPushSize`: the commit pushed, the head, the limit and the first-parent commits left; the next step follows at once (§11.3); a warning |
| `RemoteMisconfigured` | A `[remote "<name>"]` section is invalid, so that remote is left out (§11.1); raised when a volume opens and by `agentfs push` |
| `RepackFailed` | The periodic `git repack` (§8) failed, or refreshing the object database after it; tried again an interval later |
| `PushSucceeded` | A push attempt succeeded, including one with nothing new (not an alarm; sinks filter) |
| `PatchBlocked` | A patch could not be applied at mount: the inbox number, the patch, the reason (§12) and the queue depth; inbox 0 with reason `failed` when the queue could not run at all; the volume mounts anyway |
| `PatchFetchFailed` | Fetching the inbox from the patch remote failed or timed out; the queue runs with what was already fetched (§12) |
| `PatchIgnored` | An inbox entry the queue never applies: `stale` (at or below the last applied number, with another commit; deleted on the remote by the queue push) or `malformed` (§12); a warning |
| `PatchApplied` | A patch was applied as a commit (not an alarm; sinks filter) |
| `PatchDuplicate` | A queued patch commit was already applied under an earlier inbox number, so it was consumed with a marker commit and not applied again (§12; not an alarm) |
| `ForkFinished` | A fork wrote its finish commit, with the fork and its change (none when nothing changed); once (§11.5; not an alarm, logged at INFO) |
| `ForkPublished` | A fork's fork remote holds its change and finish refs at exactly their ids: the fork and the remote; once, when the published mark is written (§11.5; not an alarm, logged at INFO) |
| `ForkRefused` | A fork's fork remote refuses its publish for good: the fork, the remote, the reason (`another_writer`, `too_large`, `rejected`, `remote_gone`) and the remote's message; once, when the refusal is recorded; nothing retries it until `agentfs fork finish --retry` ([fork-refused.md](runbooks/fork-refused.md), §11.5) |
| `ForkAbandoned` | An operator abandoned a fork on record, with the reason (§11.5; not an alarm, logged at INFO) |
| `HistoryServiceUnavailable` | A front's request to the history service failed (unreachable, timed out, lost or refused) after it last succeeded: raised once per outage; boundaries fail with `EIO` until it is back (§3.5, §14) |
| `HistoryServiceRestored` | A front's request succeeded again after `HistoryServiceUnavailable` (not an alarm; sinks filter, the CLI prints it) |
| `IdentityFallback` | The history service could not render a journaled front origin's committer (the identity templates changed across a restart); that pod's records are committed with the service's own committer, keeping the pod trailers (§9); once per pod |

A sink is called from the synchronous step, on the FUSE threads, so it must
never block. Backlog gauges (`unpushed_commits`, `oldest_unpushed_age`) come
from each remote's push state, not from events.

**Export.** The binary reads the standard `OTEL_*` variables. A signal is
exported only when an OTLP endpoint is set for it
(`OTEL_EXPORTER_OTLP_<SIGNAL>_ENDPOINT`, else `OTEL_EXPORTER_OTLP_ENDPOINT`);
with none, with `OTEL_SDK_DISABLED=true`, or with
`OTEL_<SIGNAL>_EXPORTER=none`, telemetry is a no-op and nothing is built.
Each signal goes over the protocol `OTEL_EXPORTER_OTLP_<SIGNAL>_PROTOCOL`,
else `OTEL_EXPORTER_OTLP_PROTOCOL`, names: `http/protobuf` (the default),
`http/json`, or `grpc`. The HTTP exporters use a blocking client on the
SDK's batch threads; the gRPC exporter (tonic) needs a tokio runtime, so a
one-worker runtime (`agentfs-otlp`) is started only when a signal uses gRPC,
and the batch threads drive each export to completion on it. Headers
(`OTEL_EXPORTER_OTLP_HEADERS`) go out as HTTP headers or gRPC metadata, so
a bearer token is `Authorization=Bearer%20<token>`.

**TLS to the collector** (`https://` endpoints) is the `otlp-tls` cargo
feature, built into the image: rustls on ring for both transports (no
OpenSSL, no aws-lc; `ci/check-deps.sh` checks each feature set). A CA bundle
(`OTEL_EXPORTER_OTLP_CERTIFICATE`, per signal or general) is the only trust
when set, so a private collector CA is pinned; unset, the system store is
used. A client certificate and key (`OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE`,
`..._CLIENT_KEY`), set together, give mutual TLS; one without the other is a
misconfiguration. Every file is read and parsed when the exporter is built,
so a bad file turns its signal off with a warning naming it at start rather
than failing every export; rotating a certificate takes a restart (§19.2
item 7). For the
HTTP exporters agentfs builds its own blocking client with that rustls
configuration and so applies `OTEL_EXPORTER_OTLP_TIMEOUT` itself; the gRPC
exporter gets tonic's TLS settings from the same files. Without the feature
an `https` endpoint is reported as needing it.

An unknown protocol, an `https` endpoint in a build without `otlp-tls`, an
`OTEL_EXPORTER_OTLP_TIMEOUT` that is not a number of milliseconds (checked
for every endpoint), or TLS settings that cannot be used are reported
with a warning and that
signal stays off: a
telemetry misconfiguration never stops a command, in keeping with telemetry
never affecting the filesystem. Telemetry is flushed at exit, waiting at
most 5 seconds.

**Failed exports are visible.** Each exporter is wrapped: an export that
fails (the collector is down, refuses with an HTTP error, or times out)
prints `warning: telemetry: <signal> export failed: <error>` on stderr at
once, then at most once a minute with the number of failures in a row,
and `telemetry: <signal> export recovered after N failure(s)` when one
succeeds again. Each failure also counts in
`agentfs.telemetry.export_failures{signal}`, so a collector that drops
only traces or logs shows up in metrics. A failure never stops a command.

**Alarms** are alert rules on these metrics in the consumer's monitoring stack.
agentfs emits the signals; it does not page anyone itself. Example rules
ship in `deploy/alerts`, as a Prometheus rules file and a generated
`PrometheusRule`, each alert linking its runbook
([deployment.md](deployment.md#alerts)). They name the metrics as
Prometheus's default OTLP translation does (`agentfs.record.failures` is
`agentfs_record_failures_total`, `service.instance.id` is `instance`), and
each counter rule also matches a counter's first sample, since a counter
appears at its first count and `increase()` alone needs two. Under the
CSI driver one `instance` (the node plugin's pod) runs every volume on
its node, so there `agentfs.volume` must be promoted to the label
`agentfs_volume`, and the rules join a volume's series on it. The
`alerts` workflow runs promtool's checks and unit tests on them and
checks them against the binary's metrics in a real Prometheus. The CSI driver
reports failed calls through the kubelet's events on the pod (FailedMount
with the call's error); it emits no Events of its own, having no API
access (§14.2).

## 14. Volumes mounted by several pods (CSI, phase 4)

A volume can be mounted by several pods at once on one node. It is used
from one node at a time: several nodes writing one volume are not
supported (§14.2, §19.1, decided 2026-10-15).

**Shape.**

- **Backing tree:** on storage the volume's node can reach: local, or a
  shared filesystem (NFS 4.1 or later) when the volume is to move between
  nodes (§14.2).
- **A front per pod:** a FUSE mount that does the synchronous step of §3.2 for
  that pod's changes.
  - It freezes content into the volume's shared `pending/`.
  - It hands a journal entry to the volume's history service, and returns only
    once the service has made the entry durable.
- **One history service per volume:** the single writer of §2.3. It orders
  entries from all fronts, commits and pushes. Each front's entries carry that
  pod's committer identity and pod metadata (§9).

**Concurrency semantics.** Pods share one directory with the backing
filesystem's semantics. Two pods writing the same file is last-writer-wins, as
it would be without agentfs. History records each pod's change boundaries in
the order the history service received them, and a commit never holds two
pods' changes: within one pod, `commitWindow` grouping (§3.4) is unchanged.

### 14.1 Built (single node)

The service and the fronts exist for fronts on the same node as the state
area: `agentfs serve-history --root <backing>` is the service, and
`agentfs mount --history-service <addr>` (and, for scripts and tests,
`agentfs record --history-service <addr>`) a front. A plain `agentfs mount`
stays single-process and is unchanged, journal bytes included. The CSI
driver (§14.2) starts these per volume and per pod.

**Protocol.** gRPC (tonic and prost; the contract,
`crates/agentfs-core/proto/agentfs/history/v2/history.proto`, is compiled
with protox, so no `protoc`), package `agentfs.history.v2`, protocol
version 2, over a Unix stream socket in the state area,
`meta/history/history.sock` (directory 0700, socket 0600), reached through
`/proc/self/fd/<dir>/history.sock` so long paths fit. Addresses are
`unix:<path>` or an absolute path; any other scheme (`tcp:` and the like)
is refused, since a front reaches only the service on its own node.
Messages are at most 64 MiB each way (the journal's bound on one record,
not tonic's 4 MiB default). `history_service::wire` is the one place wire
messages meet agentfs types; every enum and field is converted with
validation, and an unknown or unspecified value is refused
(`INVALID_ARGUMENT`). The calls:

- `Hello {protocol_version, volume, front: {origin, slot, instance, pid,
  version, views}}`, the first call on every connection (`views`, where
  the front's workloads see the tree, §5). The service checks the peer's
  credentials (root or the service's own euid, as for the control socket,
  else `PERMISSION_DENIED`), the protocol version, the volume name, that
  the slot is in 1..=1023 and leased (below), and that the volume's
  identity templates (§9) render for the front's origin (each
  `FAILED_PRECONDITION`), and that it admits fewer than 2048 connections
  (`RESOURCE_EXHAUSTED`). It replies `Welcome {protocol_version, volume,
  branch, service_pid, features}` and binds the front to the connection:
  every later call on it is that front's, and a call before `Hello` is
  refused (`FAILED_PRECONDITION`, `send Hello first`). The front checks the
  other way too: the socket's peer must be root or the front's own euid
  (`SO_PEERCRED`), so a squatter on a shared listen directory cannot
  acknowledge records it never journaled, and the `Welcome` must name
  protocol 2, its volume and its branch. Mixed versions refuse each other
  here, with a message to run the same agentfs version; a front of
  protocol 1 (JSON frames) does not speak HTTP/2 and fails to connect.
- `Tracked {paths}` → `{tracked}`: which paths are in the tracked set. No
  `fsync`.
- `Record {draft, stamps}` → `{seq, skipped}`, answered only once the
  record is durable; a failure to journal it is `ABORTED`.
- `StillLast {path, copy}` → `{last}`: whether the last change journaled
  at `path` is still the write of the front's frozen copy `copy`, so a
  RELEASE may skip re-recording it (§3.1). No `fsync`; feature
  `still_last`, and a front never asks a service without it.
- A `Record` may carry one `dir_move` change (a directory rename, §3.1);
  feature `move`. A front whose service lacks it records the tree
  instead, and a move is never part of an fsync-class group.
- `Listen`, a bidirectional stream on which the service calls the front
  (below).
- Liveness is `grpc.health.v1` for the service `agentfs.history.v2.History`
  on the same socket (`SERVING` while it runs, `NOT_SERVING` once it shuts
  down).

`Welcome.features` names what the service supports (`stamps`,
`repair_origin`, `listen`, `still_last`, `move`), so a feature added later
needs no new protocol version.

A front keeps two HTTP/2 connections to the service, each opened with
`Hello`. On the calls connection HTTP/2 multiplexes its concurrent
boundaries, its `Tracked` and `StillLast` questions and the health probe;
the listening stream has a connection of its own. Fronts and service are
synchronous code that own small tokio runtimes
(`history_service::bridge`, Tokio's documented pattern for bridging
synchronous code), and FUSE threads block on them per call. The calls
connection runs on a current-thread runtime with no thread of its own:
the calling threads drive it, so a call costs no hop to a runtime thread
and back (in a local bench the two hops were about 40 of an 80 µs health
round trip), and between calls nothing polls it. The listening connection
needs a thread that is always polling, since the service's calls arrive
unasked, so it has a one-thread runtime. The service serves at most 2048
admitted connections (two per front process), and runs every call that
touches the volume off the runtime's threads (`spawn_blocking`), so
concurrent boundaries from all fronts still share the journal's group
commit. A connection is bounded before its `Hello` too: one from a peer
uid that may not use the service is closed at accept, at most 64 wait for
their `Hello` at once, and one not admitted within 10 s is closed. At
shutdown the service waits for the requests in flight, not for idle
connections to close. Every call carries a deadline (`--history-timeout`,
10 s, sent as `grpc-timeout` and also enforced by the front); a front
waits up to `--history-wait` (30 s) for the service at start, then exits
1 with nothing mounted.

**The synchronous step, split** (§3.2): the front decides the path rules,
runs every record-time check (size on disk, `maxSize`, the blob limit, the
content scans) and freezes, because only it holds the inode's descriptor.
A write whose decision depends on the tracked set (an ignored or nested
path) first costs one `Tracked` round trip, so an ignored file is still
never copied. Removals are always sent and decided by the service. The
service takes the path claims, decides again against its tracked set,
skips a write or symlink whose path is no longer in the backing tree as
sent (below), checks that every frozen copy exists and is in the sender's
slot, stamps the front's origin (§9), journals with group commit and
`fsync`, updates the tracked set and replies. The front returns only then.

**Pending slots.** A pending id's high 16 bits are a slot. Slot 0 is the
single process or the service; a front leases a free slot N in 1..=1023
with an exclusive `flock` on `meta/fronts/slot-N.lock` for its whole life,
which the kernel releases when it dies. Ids never collide and the journal
format is unchanged. The service's prune of unreferenced copies at start
spares live slots. A copy frozen but never journaled (its front died
between freeze and send, or the service died after reading the request
and before journaling it) is removed when a front instance first
connects to the service: the service removes that slot's copies older
than the slot's lease file (rewritten at every lease) that no journaled
record references, holding off every front record meanwhile. A front
that keeps running keeps such copies until a front next leases its slot.

**Failure** (§3.5): an unreachable, hung or refusing service fails the
boundary with `EIO`, `RecordFailed` and, once per outage,
`HistoryServiceUnavailable`. The front reconnects by itself once the
service is back. A draft is never resent (a request that timed out may
have been journaled; records hold absolute state, so that is harmless,
while a resend could journal one frozen copy twice); frozen copies are
removed on the front only when the request was certainly not sent. The
one exception proves it: a request that hyper hands back unsent because
the connection had closed (the service restarted) goes out once on a new
connection, with its `Hello`, within the same deadline, so a restart does
not fail the next boundary. How a front reads the service's answers:

| gRPC status | Front's error | Meaning |
|---|---|---|
| `PERMISSION_DENIED`, `FAILED_PRECONDITION`, `INVALID_ARGUMENT` at `Hello` | refused | the service will not admit this front |
| `RESOURCE_EXHAUSTED` at `Hello` | busy | the service is at its connection limit; an `EIO` and `RecordFailed`, but no outage |
| `ABORTED`, `INVALID_ARGUMENT` on a call | service error | the service failed or rejected the request; nothing was journaled |
| `FAILED_PRECONDITION` on a call | protocol error | a call before `Hello` |
| `DEADLINE_EXCEEDED`, `CANCELLED`, the front's own deadline | timeout, maybe journaled | never resent |
| a transport error after the request left | lost, maybe journaled | never resent |
| unsent twice, or health `NOT_SERVING` | unreachable | nothing was sent |
Repairs go to the slot's `meta/fronts/slot-N.repairs`, which the front's
sweep retries, and which the next front leasing that slot loads at start.
Each entry keeps the pod that failed to record it (§3.5), so a repair a
front inherits from a dead predecessor in its slot is committed as the
predecessor's pod: the front sends it as a `sweep` record naming that
origin, and the service journals the named origin for a sweep record if
the volume's identity templates render for it, and the front's own
otherwise (with a warning), as it does for any other trigger. Files
written by older fronts have no origins, and their entries are committed
as the loading front's pod, as before.

**Recovery.** A front that crashes leaves a stale mount, handled as for
the phase 1 sidecar (§17.1); its lease is released by the kernel. A service
that crashes is restarted: it recovers its journal as `run` does (the
origin is journaled, so replayed commits keep each pod's committer), the
stale socket is replaced, and fronts reconnect at their next request.
Boundaries during the outage failed with `EIO`; their final states are
recorded by the fronts' repairs, RELEASE and the sweep, but intermediate
states are lost.

**Listening.** Each front also keeps one `Listen` stream open on its
connection (feature `listen`; `history_service::listener`): the service
sends calls (`FrontCall`) on it and the front answers them
(`FrontAnswer`, matched by id), one at a time, on a thread of its own.
The stream's response headers tell the front it is listening. `Settle` runs the
front's sweep and returns once its records are journaled (with how many
paths still wait for a repair), `OpenForWriting` says which of a list of
paths the front holds open for writing, and `Invalidate` makes the front
drop its kernel's cached entries, attributes and pages of a list of paths
(FUSE notifications, sent from the listener thread so never from a
request handler); `Ping` checks the link. The service keeps one link per slot
(`history_service::fronts`), replacing an older one; the listener
reconnects with a backoff of 100 ms to 5 s when the service restarts or
the link breaks. These calls let the service serve `restore` and `revert`
while fronts are live (§15).

**What fronts do not do.** They serve no control socket: `restore`,
`revert` and `push` are served by the service. The patch queue (§12) is applied by `serve-history` before it
listens, and only when no front lease is live, because patches write the
backing tree that live fronts serve. POSIX and BSD locks stay in each FUSE
mount's kernel and are not shared between fronts, so a SQLite or other
lock-coordinated database must be written from one pod. A front does not
know what other fronts write: a late record of its own (RELEASE, or the
sweep while it holds the file open) of a path another pod is writing at
that moment captures that pod's state, for example the empty file
between its `O_TRUNC` and its write, under its own committer. Freeze
stamps keep the order right and the other pod's close then records its
final state; only the intermediate commit's attribution is wrong.

**Freeze stamps** (`agentfs_core::stamps`). Each front freezes under its
own path claim but the service journals under its own, so two pods
writing one path at nearly the same instant can reach the service in
the opposite order to their freezes. Each write therefore carries the
stat stamp (device, inode, size, mtime, ctime) of its backing file,
taken just before the freeze, so never newer than the content frozen.
The service keeps the stamp of the last write it journaled at each path,
with the slot it came from (five minutes, far past any request timeout;
an older entry no longer counts) and, under the path claim, skips a
write whose stamp has an older `ctime` (`superseded by a newer state
another front already recorded`, counted by
`agentfs.history_service.superseded`); its copy is removed. `ctime`
moves on every write, chmod and rename of an inode, from one clock per
filesystem, so it orders a replacement against its predecessor too.
The backing file's stamp now is consulted as well:

- a write whose stamp is the file's current one is never skipped, and
  replaces the noted stamp: it froze the file as it still is. `ctime`
  follows the wall clock, so after a backward step (NTP, a VM resume)
  every later write of a recently journaled path would otherwise look
  older than the noted one;
- stamps with the same `ctime` that differ otherwise are settled by the
  file: if it still has the journaled stamp, the incoming write is
  older;
- identical stamps from one slot are that front's own re-record (the
  racy-timestamp rule records a file twice on purpose, §3.1), ordered by
  its own claim, and are journaled;
- identical stamps from two slots (or a front and the service) cannot be
  ordered: two same-size writes within one tick of a coarse timestamp
  clock (kernels before 6.13, or filesystems without multigrain
  timestamps, tick every few milliseconds). The write is journaled, and
  then the service **resyncs** the path: once the record is journaled
  and its claims released, it records the path as the backing tree has
  it now, as a `sweep` record of its own under the path's claim (frozen
  again, reached with `openat2` beneath the tree root, never through a
  symlink and never opening a FIFO or device, §3.1; nothing is committed
  if the content is what was just journaled), counted by
  `agentfs.history_service.resyncs`. HEAD then ends on the tree's state
  whichever record arrived last.

The service's own writes (a live restore, §15; patches; resyncs) note
their stamps too. Atomic groups (§8) are recorded whole and never
judged.

**Renames and removals across fronts.** Each front learns of another front's renames and removals only at its
next lookup (entries are cached 1 s); a name a front knew that no longer
leads to the file it looks up (moved away, or its inode number reused by
the filesystem) is dropped then, so opens use the current name. A record
from an open descriptor (close, RELEASE, the sweep) never trusts a cached
name: each of the node's names is opened and compared (device and inode)
with the descriptor, and the names that no longer lead to it are dropped.
With none left, a file still linked is recorded at the path the kernel
gives for the descriptor (`/proc/self/fd`, which follows renames by any
process), if that leads to it; a removed file is recorded nowhere, since
the front that removed it records the removal. This costs one `O_PATH`
open and `fstat` per name per record, also in single-process mode.
A removal can still race a record in flight: front A checks the name,
front B removes the file and sends the removal, and A's write reaches
the service. The service therefore checks every front write or symlink
(outside an atomic fsync-class group) against the backing tree under the
path claim, and skips it (`no longer in the backing tree`) if the path is
gone or no longer a file (or symlink) under real directories. B's removal
is sent only after the tree changed and journals under the same claim, so
it either sees A's write journaled first or A's write sees the path gone.
A path that another front replaced with a new file (a rename over it)
between A's check and the service's is caught by A's freeze stamp: the
file at the path is another inode, so A's write is skipped the same way
(`no longer in the backing tree`), and the rename's own record, from the
front that made it, journals the new file.
The other way round, a late removal or symlink can arrive after another
front made the path again: front B unlinks `lock.json` and is
descheduled, front A creates, writes and records it, then B's removal
arrives. Removals are not judged by stamps (they have none), so the
service journals a removal of a tracked path, and if the path is a file
or symlink in the backing tree again it resyncs it (above); likewise a
symlink whose target is not the one the tree has now. HEAD then ends on
the tree's state.
A daemon running as root creates each new object as its caller (§6), so
another front's open of a name just created, as the same caller, never
finds a root-owned file and fails with `EACCES` (the e2e's concurrent
burst and same-name creates check this, and the CSI e2e does with pods
whose primary group is not their `fsGroup`). This needs the caller's
supplementary groups, which FUSE does not give for a caller outside the
daemon's pid namespace (pid 0): the CSI node plugin therefore runs with
`hostPID: true`, and without it the window and the refusals of §6
("Callers whose groups are unknown") remain.

**Measured cost per boundary** (§19.2 item 2, single node). These
numbers are indicative: the CI runners are shared. One threshold is
asserted: the `e2e-multipod` jobs run the bench below five times
(`ci/round-trip-bench.sh`) and fail when a front's single-thread record
p50 exceeds 1.6 times the single-process one at the median of the five
ratios (§19.1).

- CI, gRPC (protocol 2), calls driven by the calling threads: the
  `e2e-multipod` jobs of run 38819854746 for commit 2526397 (GitHub
  `ubuntu-24.04`, x86_64, kernel 6.17.0-1022-azure, backing tree and
  state area on ext4, which has no reflink, so every freeze is a copy),
  microseconds:

  | Measurement | Passthrough | p50 | p90 | p99 |
  |---|---|---|---|---|
  | close through a single-process mount (`boundary-latency.py`, n=1000) | auto | 996 | 1238 | 2007 |
  | close through a front | auto | 1234 | 1532 | 3590 |
  | close through a single-process mount | off | 961 | 1205 | 2304 |
  | close through a front | off | 1253 | 1493 | 2783 |
  | `grpc.health.v1` round trip (bench, no FUSE, n=2000) | auto / off | 147.9 / 90.4 | 151.7 / 137.7 | 164.5 / 146.5 |
  | single-process record, 1 thread (bench) | auto / off | 827 / 858 | 1067 / 1039 | 2167 / 1904 |
  | front record, 1 thread (bench) | auto / off | 1099 / 1134 | 1340 / 1340 | 3422 / 3038 |
  | single-process record, 8 threads (bench) | auto / off | 2083 / 2028 | 2816 / 2872 | 5694 / 21208 |
  | front record, 8 threads (bench) | auto / off | 2516 / 2470 | 3264 / 3436 | 5168 / 21528 |

  The bench is `cargo test --release -p agentfs-core --test
  history_service -- --ignored --nocapture round_trip_cost`, each a
  `record` of a 4 KiB file. A front's single-thread record costs 1.33
  and 1.32 times the single-process one at the median in the two jobs
  (1.30 and 1.23 in reruns of the same commit), and a front adds 238 and
  292 µs to the median close. The round trip itself varies with the
  runner from 61 to 148 µs. With one connection per front, driven by a
  runtime thread (commits 9d8a536 and 33b89b3), the round trip was
  49–134 µs and the ratio 1.24–1.39 (runs 38804442698 and 38806366270);
  with protocol 1's JSON frames it was 18–27 µs and the ratio 1.16–1.18
  (runs 38796484778 and 38799428170). On shared runners these vary more
  between runs than between designs; a local bench on one machine (an
  aarch64 VM, interleaved runs) measured the calls driven by the calling
  threads at about 40 µs less per call (health round trip 45–57 against
  78–92 µs) and a third less at 8 threads. p99 on shared runners is
  noise in both directions.
- Protocol 1, kept for comparison: a local bench (the same test; no
  FUSE; x86_64, 16 CPUs, kernel 6.18.48-talos, overlayfs) measured a
  29 µs `Ping` and a front adding about 6 % to the median single-thread
  record (3148 against 2976 µs).

### 14.2 CSI driver (built)

`agentfs-csi` (crate `crates/agentfs-csi`, §17.2) is a CSI v1.13 driver,
`agentfs.mcurcio.github.io`, in the agentfs image. The operator's guide is
[csi.md](csi.md). Each claim is a **worktree**: its files and its own
branch `agentfs/<volume>`, on the disk of the node that created it, for
good (F8 "one branch per worktree", §19.1 2026-10-16). Any number of pods
on that node share it; every multi-node access mode is refused (below,
§19.1). Continuing a volume elsewhere is a new volume started from the
old branch; which branch is current is the operator's choice, never
fenced.

**Shape** (§19.1 2026-10-16, R1, R8). There is no controller Deployment
and no attach step. One DaemonSet runs on every node: `agentfs-csi`,
which serves the Identity, Controller and Node services, beside
csi-provisioner in `--node-deployment` mode (it provisions the claims
whose pods are scheduled to its node, and deletes the PVs of its node's
worktrees), node-driver-registrar and livenessprobe. The driver
advertises `VOLUME_ACCESSIBILITY_CONSTRAINTS`; NodeGetInfo and
CreateVolume answer the node's topology,
`topology.agentfs.mcurcio.github.io/node = <node>`, so the PV's node
affinity keeps every pod of the claim on that node (`WaitForFirstConsumer`
picks the node first; prior art: local-path, TopoLVM).
`CSIDriver.attachRequired` is `false`; ControllerPublish and
ControllerUnpublish are not offered.

**Lifecycle mapping.**

| CSI call | Where | agentfs |
|---|---|---|
| CreateVolume | the node's plugin | `<basePath>/<pv>/{backing,state,csi.json}` on this node, which the request's topology must include (`RESOURCE_EXHAUSTED` otherwise); the state area initialised in-process (`init_with`, or `start_from` for a start class) with the profile's config included and its rules installed; the answer carries the node's topology |
| NodeStage | node plugin | checks the worktree (it never creates one: missing is `NOT_FOUND` naming the node, and a static PV is adopted only over an existing worktree of its volume, `reopen_with`), starts `agentfs serve-history` for the volume (which waits up to 30 s for the state lock a release-scan push or DeleteVolume's listing may hold), waits a bounded time for it to listen |
| NodePublish | node plugin | starts a front, `agentfs mount --history-service`, with the pod's identity, mounted at a private source; bind-mounts it onto the kubelet's target |
| NodeUnpublish, NodeUnstage | node plugin | stop the front, the service (final commit, and one more push of it within `--push-grace`) |
| DeleteVolume | the node's plugin | asks the release decider (below), never pushes: delete renames the directory to `.deleting.<handle>` under the state lock and removes it after; keep renames `csi.json` to `csi.retained.json`; wait answers `UNAVAILABLE` and wakes the release scan |

The node plugin spawns the `agentfs` binary for every long-running piece
rather than linking FUSE into the gRPC server: a crashed front or service
cannot take the server down, and the processes, flags and logs are the
ones the single-node tests exercise (§14.1).

**Parameters and profiles.** Everything about a volume is the operator's:
a StorageClass (or static PV) names a `profile`, whose
`<profile>.config` (a git-config fragment, §4.3), `.exclude` and
`.attributes` (§4.1, §4.2) live in a ConfigMap in the driver's namespace,
and whose credentials are files of a Secret there, both mounted into the
driver's pods. Pod authors choose only the class. Other parameters:
`volumeName` (a template over the claim's namespace and name and the PV
name, by default all three; it must contain `{pvName}`, so a re-made
claim is a new worktree with a new branch, F8 rule 1; a literal name in
a static PV, whose unknown attributes are refused), `onDelete`, and
`startFrom` (below). There is no `branch`: a worktree's branch is always
`agentfs/<volume>`, named once in its identity, which no include, static
PV, stage or start can change. Inline ephemeral volumes are refused
(`volumeLifecycleModes: [Persistent]`), since their parameters would be
the pod author's.

**One branch per worktree** (F8 rules 1 and 2). The invariant: no path
writes a branch its worktree did not create. A worktree writes its
branch only when the remote has none or holds a commit already on its
own history, decided in the push's own negotiation (§11.3). Another
worktree's tip reaches a worktree only by a start or a fork, both of
which name a new branch; a CLI or sidecar volume reusing a name is an
unrelated history, saved as a divergence. Not covered: a hand-copied
state area, local and `file://` remotes (libgit2 updates them without
compare-and-swap), and anyone holding the remote's credentials.

**Start classes** (R2, R3). A StorageClass with `startFrom:
"<remote>:<branch>@<commit>"` makes every claim on it a new worktree
whose history begins at that commit of another worktree's branch:
CreateVolume (`fork::start::start_from`) fetches the branch once from
that remote, a push section of the profile, with its credentials;
checks the commit is the branch's tip or an ancestor of it; writes the
start point into the new volume's own config (`agentfs.start.*`, never
from an include, never changed); points the new branch at the commit;
and writes its tree through the fork's confined tree writer, with LFS
content from the start remote's endpoint. It writes nothing to any
remote, and never the start branch. A start point names a branch only:
a `refs/...` name, such as a `diverged/*` ref (which never moves, so
nothing could alarm on it), is refused (`INVALID_ARGUMENT`, "not a
branch"), and so is the new volume's own branch. Codes:
`INVALID_ARGUMENT` (shape, remote not in the profile, own branch),
`FAILED_PRECONDITION` (no such branch, commit not on it), `UNAVAILABLE`
(the fetch or an LFS download failed; retried), `ALREADY_EXISTS` (the
name exists with another start point). Moving a volume is a runbook
([moving](runbooks/moving.md)); its old node's late commits raise the
start-branch alarm on the new volume (§11.3).

**Identity.** `podInfoOnMount` gives NodePublish the pod's name, namespace
and uid, which the front passes on as its origin (§9): each pod commits
as itself, and the service journals the origin so replays keep it.

**Little network work in a call.** CreateVolume of a start class
fetches once; DeleteVolume lists each remote once; NodeUnstage waits on
the service's final push (bounded by `--push-grace`). No call pushes.
A volume otherwise starts empty, except a fork (decided 2026-10-16,
§19.1 "Content sources"; §11.5): CreateVolume from a fork class clones
its `dataSource` claim locally, copying the source's history up to its
latest commit and writing that commit's tree, with no credentials and no
network. The source must be on the fork's node (`FAILED_PRECONDITION`
otherwise, naming the remedy: pod affinity to the source's pods).

**The release decider** (F8 rule 3, D1). `agentfs_core::release::decide`
is the only deletion decider. It is pure; DeleteVolume gathers its
facts: whether the branch exists, the declared remotes (`Declared::of`),
what each pushed remote advertises for the branch in a listing made for
that call (`RemotePusher::branch_claim`, judged by `claim`; the push
record is not asked), the class's `onDelete`, and a fork's state. It
weighs, in order, no history, then the fork's state, then the remotes:

- **Delete**: every declared remote holds the head (or there is no
  history), and `onDelete: delete`;
- **Keep**: the volume is released and the worktree stays on its node:
  `onDelete: retain` once every remote holds the head, or a profile with
  no remote declared (nothing elsewhere can hold it);
- **Wait**: `UNAVAILABLE`, naming the first remote that does not hold the
  head and why (lacking, unreachable, diverged), every remote section
  invalid, or a fork not yet published or abandoned.

Every class of the driver has `reclaimPolicy: Delete`, enforced by the
chart's admission policy (`agentfs-class`), so DeleteVolume always runs
and the decider always answers; data safety is the decider's, not
Kubernetes': with `onDelete: retain` (the default) the worktree stays on
disk after its PV goes. DeleteVolume refuses (`FAILED_PRECONDITION`) a
worktree staged on its node, one whose state lock is held, and one whose
settings cannot be read (nothing deleted or released).

**The release scan** (D1). The node plugin's release scan
(`release_scan.rs`) runs at start, every `--release-scan-interval`
(30 s) and whenever DeleteVolume waits. For every `csi.json` or
`csi.retained.json` on its node that no pod stages, it runs `agentfs
push` (the one push path, with the plugin's identity) unless the last
push of it exited 0 at its current head (kept in memory: a restarted
plugin pushes each once more), backing off per worktree after a failure
from the interval, doubling up to `--backoff-max`; exit 75 is retried at
the next scan. Each failed attempt raises `PushFailed` through the
child's own alarms and OpenTelemetry export: that is what alarms while
unpushed commits sit in an unstaged worktree, whether its claim is gone,
it is kept (a static PV with `Retain`), or it is only between pods. A
fork asked to finish gets `agentfs fork finish` instead (below); a
refused or abandoned fork is never pushed.

**Forks** (§11.5). A fork class is a StorageClass with `fork: "true"`;
it must not set `profile`, `branch` or `startFrom` (the fork has its
source's profile, its own branch, and starts from its source's head),
its `volumeName` must name `{pvName}` like every class's, and its
`onDelete` defaults to `delete` (§19.1 "Fork onDelete"). The driver
advertises `CLONE_VOLUME` and clones only into a fork class; any other
content source, and every snapshot, is `INVALID_ARGUMENT`. DeleteVolume
of a fork records `finish_requested` in `csi.json` and answers
`UNAVAILABLE` until the fork is published or abandoned (`agentfs fork
abandon`, run in the node plugin: [csi.md](csi.md) §14), then applies
`onDelete`; a fork status that cannot be read is `FAILED_PRECONDITION`,
nothing deleted. The release scan runs one `agentfs fork finish
--timeout 300s` per fork asked to finish that no node stages, no
finisher of its own already serves, and that is neither refused nor
abandoned; it is the only writer of a finished fork, and holds the fork
remote's credentials through the profile. The timeout frees the fork's
lock between scans, for an operator's `fork abandon --wait-lock`. A
finished fork staged again (through a static PV of its retained
directory) starts its history service, its front mounts it read-only
(`ro`, so the kernel answers `EROFS`), and NodePublish bind-mounts it
read-only (`MS_RDONLY`) whatever the request asked. The service pushes on
its interval (§11.2), and before it listens fetches the patch inbox
(bounded by `agentfs.patchFetchTimeout`, §12) and applies queued
patches. NodeStage only waits for it, at most `--stage-wait` (60 s), and
answers `UNAVAILABLE` past that while the service keeps starting; the
kubelet retries.

**Recovery, specified and tested** (§18 test 16):

- *Records.* The node plugin keeps a staged record per volume and a
  published record per target (pod, slot, front pid) on a hostPath that
  survives its restarts.
- *A crashed front* is restarted with backoff (1 s doubling to 30 s,
  reset after a minute up) with `--history-slot`, so it leases its
  predecessor's slot (`FrontLease::acquire_preferring`) and drains that
  slot's repairs as the same pod (§3.5). The stale source is detached
  (`MNT_DETACH`); the new mount is bound **over** the stale target, which
  cannot be unmounted under a running container (stacking). The target is
  made shared after the bind, so the stacked mount propagates to
  containers that mount with `HostToContainer`; others see it after a
  container restart.
- *A crashed service* is restarted with the same backoff; it recovers its
  journal (§3.3), and fronts reconnect at their next request.
- *A plugin restart* stops every child (final commits) and leaves the
  records. The new plugin reconciles: it starts the staged volumes'
  services, and recovers each recorded target whose mount is still there
  (live or stale) as for a crashed front; targets the kubelet removed are
  forgotten. The DaemonSet is `OnDelete`: an upgrade interrupts the node's
  mounts for seconds, so it is rolled node by node.
- *Order.* Per-volume locks serialise stage, publish, unpublish, unstage
  and restarts; unstage is refused (`FailedPrecondition`) while a target
  is published, and publish before stage likewise. Waits happen outside
  the lock (a front mounting, a service starting, a restart's backoff), so
  one pod's slow start never delays another's recovery; a child that
  exits before the call that started it sees it ready is that call's
  failure. A republish of a target with other flags is `ALREADY_EXISTS`.
- *Failed restarts.* A restart whose spawn fails (fork under pressure, a
  full disk) is tried again with backoff; entries and records stay, at a
  restart as at reconcile.
- *Unpushed history.* NodeUnstage's service pushes its final commit once
  more within `--push-grace`; then the release scan pushes the unstaged
  worktree until a remote holds its head, and the release decider keeps
  DeleteVolume waiting meanwhile: history no remote holds is never
  deleted.
- *A lost node, a wiped disk.* A worktree whose node is gone is not
  recovered elsewhere: the operator starts a new volume from its branch
  ([moving](runbooks/moving.md)). A worktree whose directory is gone
  makes NodeStage answer `NOT_FOUND`, and the pod waits, its events
  naming the node.

**One node per volume.** Several pods on one node share a volume, each
committing as itself: `ReadWriteOnce` (`SINGLE_NODE_WRITER` or
`SINGLE_NODE_MULTI_WRITER`) and `ReadWriteOncePod`
(`SINGLE_NODE_SINGLE_WRITER`); `SINGLE_NODE_READER_ONLY` is a read-only
mount. Every multi-node access mode (`MULTI_NODE_READER_ONLY`,
`MULTI_NODE_SINGLE_WRITER`, `MULTI_NODE_MULTI_WRITER`, so
`ReadWriteMany` and `ReadOnlyMany`) is refused with one message naming
the decision (§19.1), at CreateVolume, ValidateVolumeCapabilities,
NodeStage and NodePublish, and a `csi.json` with fields the driver does
not know (one written by an earlier build) is refused rather than half
read. The PV's node affinity keeps the worktree's pods on its node; pods
on other nodes read its history from the remote it pushes to.

**Upgrading the node plugin.** A live hand-over of a node's FUSE
sessions to a new plugin is not built: the fronts are the plugin's
children, a FUSE session cannot be passed between processes without
re-running INIT, and the kernel's node ids, lookup counts and
passthrough backing ids cannot be transferred. The supported procedure
is per node: cordon, evict that node's agentfs pods, delete its plugin
pod (the DaemonSet stays `OnDelete`), wait until it is Ready, uncordon
([csi.md](csi.md#9-operations)).

**RBAC inventory.** csi-provisioner, the only container with a token
(projected into it alone; the node plugin's account is not
automounted): PVs (get, list, watch, create, patch, delete), PVCs (get,
list, watch, update), StorageClasses, CSINodes and Nodes (get, list,
watch), Events (list, watch, create, update, patch), bound to the node
plugin's account. No attacher, no leases, no Secrets, pods or snapshots
anywhere: credentials are mounted files, never API reads. Each node's
provisioner holds PV and claim rights cluster-wide. The table with the
reasons is in [csi.md](csi.md#10-security); `ci/e2e-csi.sh` checks it with
`kubectl auth can-i`.

**Pod Security inventory.** The driver's namespace enforces `privileged`
(warn and audit `baseline`): the node plugin is privileged and root, and
shares the host's pid namespace (`hostPID`, so its fronts see callers'
real pids and can read their supplementary groups, §6) (FUSE
mounts, `Bidirectional` propagation into `/var/lib/kubelet`, passthrough's
`CAP_SYS_ADMIN`, `CAP_CHOWN`/`CAP_FOWNER` for ownership); the sidecars
run as root without capabilities and with a read-only root filesystem
(to reach the root-owned 0660 socket). Workload namespaces can enforce
`restricted`: a pod only mounts a claim. Hardened node images (Talos) are
untested.

**Where worktrees live** (R1, "B"). `--base-path` is any directory on the
node's own disk (the chart's `basePath.path`, a hostPath); sizes are not
enforced, so a full disk makes the kubelet evict every pod on the node,
and the docs recommend a disk of its own. `agentfs-csi` refuses a base
path on a network filesystem (nfs, nfs4, cifs, smb3, smbfs, ceph,
fuse.glusterfs, glusterfs, lustre, gpfs, afs; `shared_fs::network_filesystem`)
before it serves anything, naming its type. The optional shared storage
is an ordinary git remote (a server on a claim of any StorageClass, NFS
4.0 or iSCSI included), never the base path. The state lock's NFS checks
(§2.3) still guard CLI and sidecar state areas, which may live on NFS 4.1
or later (F7 decision).

Tested on NFS (`ci/e2e-nfs.sh`, CI job `e2e-nfs`, passthrough `auto` and
`off`): a kernel NFS server on the runner exports one directory to two
NFSv4.2 clients (the second in network and UTS namespaces of its own, so
a separate client id, lease and set of locks). It checks that the state
lock and its holder line hold across the clients, that an NFS 4.0 mount
is refused at start, mmap and SQLite single-writer through a front on the
NFS tree, that NFS silly renames never reach history, and that
`agentfs-csi` refuses a base path on the export (4.2 and 4.0). The
`local_lock` refusal is NOT RUN and cannot run on Linux: the kernel
clears `local_lock` and `nolock` on every NFSv4 mount
(`nfs_fs_context_validate` in `fs/nfs/fs_context.c`), and NFSv2/3, which
keep them, are refused for their version first.

Moving between nodes is tested on a two-worker kind cluster
(`ci/e2e-csi-move.sh`, the f8 workflow's `e2e (CSI move between nodes)`
job): the steps of [moving](runbooks/moving.md), the old node's late
commit reaching the old branch through its release scan, the start-branch
alarm and its acceptance, a second writer of an old name diverged, a
start class on a `diverged/*` ref refused, and a missing worktree making
a pod wait.

## 15. Recovery and undo

Recovery may be manual, but it must be supported and documented. Phase 1
delivers two commands, `agentfs restore` and `agentfs revert`, and
operator runbooks ([runbooks](README.md#runbooks)):

| Runbook | For |
|---|---|
| [recovery-and-undo.md](runbooks/recovery-and-undo.md) | Finding a change, pausing the workload, restore and revert, refusals, plain-git alternatives |
| [divergence.md](runbooks/divergence.md) | Resolving `refs/agentfs/<volume>/diverged/*` (§11.3) |
| [history-rewrite.md](runbooks/history-rewrite.md) | Removing a recorded secret from the state area and each remote (§10) |
| [stuck-pushes.md](runbooks/stuck-pushes.md) | Pushes that keep failing |
| [crashed-sidecar.md](runbooks/crashed-sidecar.md) | A sidecar whose daemon died: automatic recovery by stacking, when to step in, the pod restart (§17.1) |
| [patch-queue.md](runbooks/patch-queue.md) | Submitting patches (with agentfs or plain git), a blocked queue, replacing or skipping a patch (§12) |
| [fork-refused.md](runbooks/fork-refused.md) | A fork whose publish is refused for good: each reason, how it shows, retry or abandon (§11.5) |
| [moving.md](runbooks/moving.md) | Continuing a CSI volume on another node: a new volume started from the old branch (§14.2) |
| [start-branch-moved.md](runbooks/start-branch-moved.md) | `StartBranchMoved`: taking in or accepting the commits an old branch received after a move (§11.3) |

**The supported path:**

1. **Find the change.** History is ordinary git on the remote, or in the
   state area: `git log --follow <path>`, `git show`, the remote's web
   view. Trailers (§9) say which pod, process and trigger made each
   change.
2. **Pause the workload** when possible. A live restore is supported,
   but a workload writing the same files can overwrite it again (its
   writes are recorded too), and a file the workload opens and writes in
   the moment between a path's last re-check and its write (see Safety)
   has those writes replaced before they are recorded.
3. **Restore or revert:**
   - `agentfs restore <path> <commit>` writes `<path>` (a file, symlink,
     directory, or `.` for the whole tree) as it was in `<commit>`,
     following `git restore --source=<commit> -- <path>`: every entry the
     commit has there is written, every entry history has there now but
     the commit did not is removed, and untracked files are never
     touched.
   - `agentfs revert <commit>` applies the inverse of the commit's diff
     against its first parent (the empty tree for a root commit), without
     rename detection. Merge commits are refused.
   - **Database groups.** A path in the `fsync` class (§8) brings its whole
     group: restoring or reverting `app.db` also restores `app.db-wal` and
     `app.db-journal` (each from the same commit, or removed if the commit
     has none), so the database and its log always come from one capture.
     A member present in the tree but not in history refuses the
     operation. In direct mode each group is recorded as one atomic
     record; through a mount the group is captured once, at the end of
     the operation (§8).
   - `<commit>` is any git revision; `HEAD` means the volume's branch. It
     must be on the branch (the head or an ancestor); other commits,
     including diverged refs, are restored with plain git.
   - With the workload stopped, plain git also works: `git show <commit>:<path> > <file>`
     into the backing tree, then `agentfs record <path>`.
4. **Resume the workload.**

**Two modes, one engine** (`agentfs_core::restore`: plan, check, apply):

- **Through a running mount.** `agentfs mount` (like `run`, which serves
  only `push`, and `serve-history`, below, §2.3) listens on
  `meta/control/agentfs.sock` (§2.2): directory `0700`, socket `0600`,
  and each connection's `SO_PEERCRED` must be root or the daemon's user.
  Both ends address it as `/proc/self/fd/<dir fd>/agentfs.sock`, so a long
  state path never hits the 108-byte `sun_path` limit. The peer's pid is
  0 when it is in a pid namespace the daemon cannot see; it is then
  unknown and named in no `Agentfs-Pid` (§9). The protocol is JSON
  lines, version 1, one request per connection, at most 64 KiB, read
  within 10 s; a restore request must name its path (`.` for the whole
  tree; a missing path is refused, never taken as the root); one restore
  or revert runs at a time and a second gets `busy` (exit 75); a `push`
  request runs beside it. The daemon waits
  until every record made before the request, including one still being
  written to the journal, is committed (at least 10 s, or 20 commit windows; otherwise refused as
  history being behind), checks against its mount point, and writes
  through its own mount from the connection's thread. The writes go
  through the kernel like any workload's, so kernel caches, the inode
  table and dirty detection stay coherent, and the FUSE layer records
  them. The control socket is shut down before the unmount.
- **Through the history service.** `agentfs serve-history` (§14) listens
  on the same control socket and serves restores for the volume's fronts,
  each of which keeps a listening connection to it (§14.1). Every slot
  whose lease is held must have a listening front, waiting up to 5 s for
  one that is reconnecting; otherwise the request is refused as busy
  (exit 75), since a front the service cannot reach could neither settle
  nor drop its stale caches. The service has each front settle (record
  what is dirty in it, journaled before it answers; a front that fails
  to, or does not answer in time, makes the request busy, 75, with
  nothing written), waits until history has caught up as above, and
  checks the backing tree, asking the fronts in batches which paths any
  of them holds open for writing (a path a front failed to answer for
  counts as open). A dry run settles the fronts too, so it checks what a
  real run would; what they record is what their next sweep would have
  recorded anyway. The service then writes the backing tree itself, as
  direct mode does, asking every front again about each path just before
  writing it (one round trip per front and path), so a file a pod opens
  for writing while a long restore runs is refused at its own step unless
  `--force`; a pod that opens it between that question and the write is
  not seen, as in mount mode. It records what it wrote as one operation
  (operator: the socket peer; actor: the service), and has every front
  drop its kernel's cached entries, attributes and pages of every path
  written or removed. A front that fails that call keeps serving and is
  named in a warning: it may show old content until its kernel's cache
  times out. Records a front's settle or sweep makes during the operation
  are its own, not the operation's. When the service is stopped during a
  restore, the restore stops before its next path, records what it wrote
  and fails part way (rerun it); the service waits for that up to
  `--shutdown-grace` (at least 5 s) before it stops serving fronts. Only
  a single path still being written at the end of that grace (a large
  LFS object) is left written and unrecorded.
- **Directly.** With nothing listening, no mount (and so no workload
  through one) can be running. The command takes the state-area lock
  (§2.3), commits what is journaled, writes the backing tree (`--root`)
  through `openat2` descriptors confined like the FUSE layer's (§6), and
  records what it wrote. `run` serves no restores, so while it (or
  `record`, `flush`, `push`) holds the lock, restore exits 75.

**Recording.** The process applying a restore (the CLI's pid, or the
daemon thread's id, which FUSE reports as the caller's pid; the daemon's
pid is registered too) is registered as the operation's **actor** while
it runs. Every record by an actor, including late RELEASE and sweep
records naming it as the last writer, gets trigger `restore`, the operator
(the CLI's ids, or the socket peer's) as its caller, and the operation in
its journal record (an optional field older journals lack). The committer
gives each operation its own commit, never grouped with workload records
and held open while the operation runs, with `Agentfs-Trigger: restore`
and `Agentfs-Restore: <commit>` or `Agentfs-Revert: <commit>` (§9).
Workload records interleaved with a live restore split it into more than
one commit, since commits follow the journal's order.

**Safety.** Every check runs before anything is written:

- everything written must be recordable: the path rules (§4, including
  `-agentfs` and ignores for untracked paths) and every pre-commit check
  (§3.4) on the content;
- the tree must hold what history says at every path the restore covers,
  including paths history already has right; anything else is a change
  not in history, and is never overwritten, even with `--force`. The one
  exception is a tree that already holds the restore's target where
  history does not (a restore stopped between writing and recording, or
  content put back by hand): nothing there would be lost, so direct mode
  records it as part of the operation instead of calling it done, and a
  mount, which records only what it sees written, refuses it as not
  recorded;
- nothing untracked may be in the way: a file where a restored one goes,
  a directory with files where a file goes, or a symlink or file as a
  parent directory (symlinks are never followed);
- `--force` overrides only refusals where nothing is lost: a revert of
  paths changed again since the commit (the later versions stay in
  history), and files a workload holds open for writing through the
  mount.

**Re-checks and failures.** Each path is re-checked just before it
changes: one that no longer holds what the check saw, or (unless
`--force`) that a workload opened for writing through the mount since,
is left alone and the operation stops there. A window remains between
that re-check and the write: a workload that opens and writes the file
in that moment, without closing it, has those writes replaced before
they are recorded, so nothing unrecorded is lost only while the workload
is paused. Through a mount, the check's own read of a file a workload
has written but not closed ends with a FLUSH of a changed inode, a change
boundary (§3.1): that content is recorded before the refusal, so a retry
finds it in history and only the open handle (overridable) remains. A
failure part way records what was written and reports the error. A file
is rewritten in place only after the space its new content needs is
allocated, so a full disk or a file size limit leaves it whole; a write
that fails after that (an I/O error) can leave it part written, and
direct mode then records it as it is, so tree and history agree. A
re-run records what the tree already holds and writes the rest.

**Writing.** Removals run deepest first and prune directories they leave
empty (never above the restored path's parent); writes run shallowest
first. A regular file with one link is rewritten in place, keeping its
inode, owner, other mode bits and extended attributes, with only its exec
bits changed; a hard-linked file, a type change or a new file is a new
inode, so only the named path changes, as in git. New files and
directories take the owner and group of their nearest existing parent
when the process may chown (`CAP_CHOWN`), with mode `0644`/`0755` and
`0755`. Content, the executable bit and symlinks are restored, nothing
else (§6). Content stored as LFS is written from the object cache, or
downloaded from a remote during the checks, before anything is written
(§7); the root `.gitattributes` is written without the managed block.

**Operator history rewrite** removes content that must not stay in
history, such as a recorded secret (§10). It is a documented procedure,
[history-rewrite.md](runbooks/history-rewrite.md), not a command:

- run against the state area's repository and every remote the content
  reached, with the workload stopped, never automatically;
- `git filter-repo --prune-empty never` in a mirror clone of the state
  repository, so every commit and its `Agentfs-Seq` survive and
  filter-repo never edits the state area's config or remotes; the
  rewritten branch is fetched back, the reflog expired and old objects
  collected, and the push state (`meta/push/*.json`) deleted;
- on each remote, a force-push of the volume's branch with a lease, the
  one case where agentfs's no-force-push rule is set aside, by an
  operator, plus deleting the volume's other refs there and the host's
  own purge;
- **forks** (§11.5, §19.1 "Forks in history rewrites"): the affected refs
  are found by reachability, never from a list: in mirror clones of the
  source's remotes and of every fork remote its settings name, every ref
  under `refs/heads/agentfs/` and `refs/agentfs/` that contains the first
  commit that recorded the content (`git for-each-ref --contains`). Only
  forks whose base contains that commit are abandoned, before anything
  is rewritten (`git merge-base --is-ancestor <bad> <base>`); forks based
  earlier keep their ids under filter-repo and publish and land as
  before. The affected refs are deleted with a lease on each remote. A
  change based after the bad commit and not yet landed can no longer land
  (`parent_not_in_history`, §12). agentfs cannot enumerate, and the
  runbook lists for the operator: delivery targets (the branch method's
  repositories, webhook and command receivers), receivers' clones and
  mirrors, users' CI caches, and fork state areas kept with `onDelete:
  retain`.

**Not built yet** (§19.2): a read-only history view for operators, and undo
through the patch queue (§12).

## 16. First consumer: Hermes

Hermes is the first consumer, used to validate the design, not a design
target. What it needs:

- **What is mounted.** Only agent-authored definitions are recorded, for example
  `skills/` and each profile's `SOUL.md`, `config.yaml`, `profile.yaml` and
  `skills/`. The operator rules exclude Hermes' secrets (`.env`, `auth.json`),
  databases, sessions, logs, caches and `checkpoints/`.
- **Two kinds of writer.**
  - The Hermes process writes to the mount directly.
  - Hermes' file and terminal tools run in containers on a docker-in-docker
    sidecar, so the mount must also reach that sidecar, and tool containers need
    it as a volume.
  - In the iac workload that is a deliberate policy change to the `docker_volumes`
    pin, which the owner must approve.
- **Root init.** The Hermes image's start-up runs as root. With a real backing
  tree and `default_permissions`, root reads the mount like any other directory.
- **Restarts.** iac patches (§12) apply only at mount, which for Hermes means a
  pod restart (or `agentfs patch apply` with the sidecar stopped).

The rule files are in `deploy/examples/hermes`, and the pod changes, the
one-time migration of Hermes' existing data and the initial import in
[deployment.md](deployment.md#10-hermes).

## 17. Layers and libraries

### 17.1 Layer 1: core and daemon (Rust)

- **`agentfs-core`, a library:**
  - recording (§3);
  - rules (§4);
  - the commit builder;
  - pre-commit checks (§3.4);
  - push (§11);
  - the background service: the commit loop and one push loop per remote,
    with clean shutdown (§11.2), and the periodic repack on the commit
    thread (§8, `repack`);
  - the `fsync` class policy (§8, `fsync_class`): groups, stems, the
    attributes and their validation; atomic group records in the recorder;
  - alarm events, through a non-blocking sink (§13);
  - Git LFS (§7, `lfs`): store selection, pointers, the managed
    `.gitattributes` block, the object cache, endpoints and credentials,
    the batch client with concurrent and resumable transfers, uploads
    before each push, `git-lfs-authenticate` for SSH remotes, LFS-aware
    restores, `lfs fetch` and `lfs prune` with the cache lock;
  - remote proxies (§11.1, `proxy`);
  - the patch queue (§12);
  - telemetry (§13): implemented against the OpenTelemetry API.
- **`agentfs-fuse`, a library:** the FUSE layer (§2.1, §3.1, §6).
  - `Ops`: every filesystem operation over the backing tree, independent of
    the kernel, so all of its behaviour is unit-tested against real
    directories and state areas; the inode table; dirty detection; the
    change boundaries; the repair queue (§3.5).
  - `groups`: the `fsync`-class groups of the mount (§8): the gate that
    holds writes while a group is captured, and each group's schedule.
  - The fuser adapter, the sweep thread, the capture thread (the `fsync`
    class's trailing timer), the layout preflight, mount and unmount.
  - It is a separate crate so the core stays free of FUSE and the binary
    stays a thin CLI.
- **`agentfs`, a binary:**
  - `init`, `mount`, `probe`, `status`, `flush`, `record`, `push`, `run`,
    `serve-history` (§14), `restore`, `revert`, `patch` (`submit`,
    `apply`, `status`), `lfs` (`fetch`, `prune`), `fork` (`create`,
    `finish`, `status`, `abandon`, `list`, `deliver`, §11.5) and `start`
    (`accept`, §11.3: a started volume accepts a tip of its start branch,
    clearing the start-branch alarm; no lock); a test
    (`crates/agentfs/src/reference_tests.rs`) checks this list against the
    CLI;
  - `fork finish [--retry] [--timeout D]` finishes once and publishes
    (exit 0 published, 1 pending at the timeout or refused, 75 busy);
    `fork status [--json]` reads the fork's state, record and publish
    result with no lock and no network; `fork abandon --reason TEXT`
    discards the fork on record, under the state lock (75 busy); `fork
    list [--remote R] [--source V] [--repo DIR] [--json]` lists the
    checked published changes on a fork remote from a receiver's
    repository, never a state area (§11.5); `fork deliver <fork> --to
    inbox|branch|command|webhook [--from R]` delivers one checked change
    from there by one method (exit 0 delivered or already, 1 refused or
    not published, 75 unavailable; §11.5 "Delivery");
  - `init` is safe to re-run (§2.3): exit 0 for the volume it already
    holds, 1 for another volume or branch; `--include` sets the config's
    `include.path` entries (§4.3), and `--root` creates the backing tree;
  - `probe <mountpoint> [--allow-stale]` reports a live agentfs mount
    (exit 0), a stale one whose daemon died (1, or 0 with
    `--allow-stale`), nothing, or another filesystem (1), from
    `/proc/self/mountinfo` and a `statfs`; no lock, no state area. It is
    the sidecar's startup and readiness probe;
  - `push` pushes once to every remote (or those named with `--remote`) and
    exits 1 unless all succeeded; with a daemon running it asks the daemon
    over the control socket instead, waiting up to `--timeout` (§2.3);
    `run` runs the service in the foreground, serving `push` on the
    control socket, until SIGINT or SIGTERM, makes a final commit, gives each remote that
    lacks commits one more push within `--push-grace` (default 5s, §11.2),
    and exits at once on a second signal. `mount`, `flush`, `record`, `push` and `run` export
    telemetry (§13) and take the pod flags (`AGENTFS_POD`, ...), so every
    command's telemetry resource is the same. `record` records paths of a
    backing tree directly, without a mount; a path in the `fsync` class
    records its whole group as one atomic record (§8);
  - `status` is read-only (§2.3): the head, the last committed sequence,
    the backlog, the state-area lock's holder, the LFS threshold, and each
    remote: its URL (userinfo redacted), last pushed commit, unpushed
    commits, consecutive failures, last error (with its stage) and any
    diverged ref, and its LFS upload endpoint and where that endpoint's
    credentials come from, or why it has none (resolved from config, no
    network);
  - `restore <path> <commit>` and `revert <commit>` (with `--force` and
    `--dry-run`) write content from history back into the tree, recorded
    as one commit (§15): through a running mount over its control socket,
    or directly into `--root` when nothing runs. Exit status 0 done, 1
    refused (a reason and a hint) or failed, 75 busy;
  - `mount`, `run`, `serve-history`, `record`, `flush`, `patch apply`,
    `fork create`, `fork finish`, `fork abandon`, and `init` of an
    existing state area take the state-area lock (§2.3) and exit 75 while
    another process holds it; `push`, `restore` and `revert` take it only
    in direct mode, with no daemon answering; fronts take none;
  - `patch submit` runs on the producer's side, in its git repository,
    through its `git` (§12); `patch apply` fetches and applies the queue
    with the volume stopped (it takes the lock; exit 1 when blocked);
    `patch status` reads `meta/patch.json` and the status ref, like
    `status`, which prints a `patches:` line. `push` prints a `queue:`
    line for the patch remote (§11.3);
  - `lfs fetch [REVISION...] [--all] [--remote NAME]... [--dry-run]`
    downloads into the LFS cache the objects the branch head references
    (or the trees of the revisions given, or every commit with `--all`),
    from the remotes in name order (§7). It takes no state-area lock, so
    it runs beside a daemon, and holds the cache lock shared; exit 1
    unless every object is cached afterwards; `lfs prune [--dry-run]
    [--older-than DURATION]` deletes from the cache the objects every
    remote's endpoints confirm (§7), taking the cache lock exclusive for
    its deletions and exiting 75 while restores or fetches hold it.
- **`agentfs mount <backing> <mountpoint>`** runs the FUSE session, the
  commit loop and the push loops in one process.
  - **Preflight.** Before anything is mounted it refuses a state area,
    backing tree and mount point that are not three separate directories
    (none inside another, after resolving symlinks), any remote section's
    `agentfsCredentials`, `agentfsSshKey`, `agentfsSshKeyPassphrase`,
    `agentfsSshAgent`, `agentfsKnownHosts` or `agentfsLfsCredentials` file
    under
    the backing tree or the mount point, invalid sections included (an
    error, not `RemoteMisconfigured`: carrying on would expose the secret,
    or let the workload rewrite known_hosts to trust any host), a
    `--root` that is not the backing tree, an `include.path` fragment
    under the backing tree or the mount point, and a stale mount point
    (`ENOTCONN`, with the command that clears it). A missing mount point
    is created first (one level, mode 0755).
  - **Order.** Lock, open the volume (journal replay, repairs), the patch
    phase (§12: fetch the inbox with `agentfs.patchFetchTimeout`, apply the
    queue, block on the first failure), then the service and the push
    loops, then the FUSE session. A blocked queue never stops the mount.
  - **A stale mount point with `--on-stale-mount recover`** (a sidecar
    restarted after its daemon died; the shipped sidecar's mode): it
    takes the lock (the kernel released the dead daemon's), opens the
    volume as usual (journal replay, repairs, the patch phase), then
    mounts FUSE at `<state>/meta/stack/mnt` (private, 0700, created with
    its parent on a fresh state area, cleared of stale layers and required
    empty), bind-mounts it over the stale
    mount point, makes the new layer shared (best effort) and lazily
    detaches the private directory. The layout preflight resolves the
    mount point through its parent, since the stale mount answers every
    `stat` with `ENOTCONN`, and refuses (exit 1, naming both) a stale
    mount whose source is not `agentfs:<this volume>`: stacking over
    another volume's dead mount would send that volume's workloads into
    this one's history. It never detaches the stale mount first:
    workload containers hold copies of it, and between a detach and a
    new mount their writes would reach the bare directory. Mount
    propagation carries the new layer into every copy instead (peers of
    the stale mount, such as a kubelet `subPath` bind, and their slaves,
    such as the workload containers' `HostToContainer` mounts), as the
    CSI node plugin's bind stacking does (§14.2). It logs `stacked over a
    stale mount left by a crashed daemon at <path> (N stale layer(s)
    below)`. It needs `CAP_SYS_ADMIN` and is refused for fronts (the CSI
    driver stacks those); refused, it exits 1 and leaves the stale mount.
    With nothing stale, `recover` (like `hold`) is a normal mount.
  - **At every stop**, after its own unmount, a mount detaches the stale
    layers (at most 16) still at its mount point, so a recovered
    sidecar leaves nothing mounted.
  - **A stale mount point with `--on-stale-mount hold`**: it mounts
    nothing and takes no lock, says so, waits for SIGINT or SIGTERM,
    then lazily detaches every stale layer at the mount point, and exits
    1; recovery is then a pod restart, by hand.
  - **A mount point that is not empty** (and not stale) is mounted over
    with a warning: what is in it is hidden and not recorded (a workload
    wrote there while no daemon was mounted).
  - **`--seal-mountpoint`** (`AGENTFS_SEAL_MOUNTPOINT`, set by the
    shipped sidecar) makes the bare mount point root's with mode 0000
    before mounting (needs root; the mount on top is unaffected). What a
    workload reaches while no daemon is mounted is then a directory no
    non-root process, nor root without `CAP_DAC_OVERRIDE`, can write:
    `EACCES` instead of a write nothing records. It is the fence for
    `subPath` consumers, which the read-only `emptyDir` cannot cover. A
    stacked mount leaves the (stale) mount point as it is; the first
    mount of the pod sealed it. Not chosen: `chattr +i` (stops root too,
    but the kubelet could no longer remove the `emptyDir`) and a
    read-only bind "floor" under the FUSE mount (it would outlive the
    pod on the node, and probes would have to tell it from a foreign
    mount).
  - **Kernel options.** fuser, multi-threaded (`--threads`, default the CPU
    count between 2 and 16) with a cloned `/dev/fuse` descriptor per thread;
    `default_permissions`, `allow_other` (unless `--no-allow-other`),
    `nodev`, `nosuid`, `rw`, fsname `agentfs:<volume>`, subtype `agentfs`.
    INIT asks for `FUSE_ATOMIC_O_TRUNC` and `FUSE_PARALLEL_DIROPS`; never for
    the writeback cache (FLUSH must see every write the daemon serves),
    POSIX or BSD locks (they stay in the kernel, which is right for one
    mount) or POSIX ACLs. No `auto_unmount`: a crashed daemon leaves
    `ENOTCONN` rather than exposing the empty mount point to writes nothing
    would record. Entries are cached 1 s, attributes not at all.
  - **Passthrough.** `--passthrough auto` (default) asks for
    `FUSE_PASSTHROUGH` (maximum stack depth 1) when the kernel offers it and
    the daemon may register backing files (`CAP_SYS_ADMIN` in the initial
    user namespace: root, not root inside a rootless container), and keeps
    one backing id per inode while any handle on it is open. If registering
    fails for one inode (a stacked backing filesystem such as overlayfs,
    `EMFILE`), only that inode is served through the daemon while it has a
    handle open; every other inode keeps passthrough, and an inode that
    already has a backing id keeps using it (the kernel refuses to mix the
    two on one inode with `ETXTBSY`). Each fallback is logged ("passthrough
    refused for inode ..."; the first ten). `on` refuses to mount without
    the privilege, fails at INIT without kernel support, and if a
    registration fails later it fails that open with `EIO` and stops the
    mount (exit status 1); `off` never asks. The startup line reports the
    result and the reason. CI's `ubuntu-24.04` kernel negotiates it (the
    `fuse` job runs `auto`, `on` and `off`, and asserts no open fell back).
  - **Restores.** After mounting it serves `restore` and `revert` on its
    control socket (§15).
  - **Shutdown** on SIGINT, SIGTERM or an external unmount: stop the
    control socket (waiting up to `--unmount-grace` for a restore in
    progress, which writes through the mount), unmount (a
    plain unmount, then a lazy detach if busy; `fusermount3 -u`, then
    `-u -z`, when not root), wait up to `--unmount-grace` (10 s) for the
    session to end, run a final sweep, capture every `fsync`-class group
    with changes (§8), make the final commit, push it once more to each remote that lacks
    commits within `--push-grace` (§11.2), flush telemetry, release the lock. A second
    signal exits 130.

  It works anywhere FUSE does, with no Kubernetes dependency.
- **`agentfs serve-history --root <backing>`** is the history service of
  a volume (§14), for fronts on the same node. It takes the state-area
  lock (holder `command=serve-history`), opens the volume (journal
  replay), applies the patch queue only when no front lease is live (else
  it says why not), starts the commit and push loops and the control
  socket (`push` only, §2.3), then listens on
  `--listen` (`AGENTFS_HISTORY_LISTEN`, default
  `unix:<state>/meta/history/history.sock`) and writes the address to
  `meta/history/address` for `status`. On SIGINT or SIGTERM it stops
  accepting, waits up to `--shutdown-grace` (10 s) for fronts' requests in
  progress (those still running fail with `EIO`), then makes the final
  commit and, as `mount` and `run` do, pushes it once more to every remote
  that lacks commits, all within `--push-grace` (§11.2; under the CSI
  driver nothing else would push the last pod's final commit, §14.2).
- **`agentfs mount --history-service <addr>`** is a front (§14): no lock
  and no control socket. Order: the preflight, a lease of a pending slot,
  the connection to the service (waiting up to `--history-wait`, 30 s, and
  exiting 1 with nothing mounted if it never answers), then the FUSE
  session, whose repair queue is the slot's `meta/fronts/slot-N.repairs`.
  Every round trip has `--history-timeout` (10 s). The startup line names
  the service and the slot. Shutdown is as for `mount`, minus the commit
  and the pushes, which are the service's. `agentfs record
  --history-service <addr>` records paths of the backing tree through the
  service the same way (`--root` is the backing tree). `status` adds a
  `history:` line while a service holds the lock, a `fronts:` line per
  live lease, and a `repairs:` line for a slot whose repair file waits for
  a front.
- **Phase 1 deployment** ([deployment.md](deployment.md), manifests in
  `deploy/sidecar`): a privileged native sidecar (an init container with
  `restartPolicy: Always`) in the workload's pod, from the image
  `ghcr.io/mcurcio/agentfs` (§17.3).
  - **Volume.** The backing tree and the state area live on one
    `ReadWriteOncePod` PVC, mounted once at `/data` as `backing/` and
    `state/` (two subPath mounts would break reflinks with `EXDEV`,
    §3.2). The Deployment uses the `Recreate` strategy, so one pod writes
    the volume (§2.3).
  - **Init container** `agentfs init --include /etc/agentfs/config`, as
    root with every capability dropped: creates the state area and the
    backing tree on the first start, accepts its own volume and refuses
    another on every later one.
  - **Mount.** The sidecar mounts the FUSE filesystem at
    `/agentfs/workspace`, a subdirectory of an `emptyDir` it mounts at
    `/agentfs` with Bidirectional propagation; workload containers mount
    the `emptyDir` read-only with HostToContainer propagation (or the
    mount itself with `subPath: workspace` at a fixed path, as Hermes
    needs, §16). A
    subdirectory, not the `emptyDir`'s root, so that a dead daemon leaves
    only the subdirectory answering `ENOTCONN`: a restarted sidecar can
    still be created, and the kubelet can still tear the pod down.
  - **Probes.** Startup: `agentfs probe --allow-stale /agentfs/workspace`
    (workload containers start once the mount is live; 5 minutes for
    journal recovery). Readiness: `agentfs probe /agentfs/workspace` (a
    dead daemon shows as a NotReady pod). No liveness probe on the
    sidecar: the kubelet restarts a daemon that dies, and a probe would
    only add kills of a slow but live one (a long journal recovery). A
    workload container with a `subPath` of the mount gets one instead
    (its mount point must be a live FUSE mount: `stat -f -c %T` prints
    `fuseblk`), so a clean exit of the daemon restarts it onto the new
    mount.
  - **Configuration.** Settings and remotes from a ConfigMap fragment
    (`include.path`, §4.3); the operator rules from the same ConfigMap,
    mounted read-only over `info/exclude` and `info/attributes` in the
    sidecar (a change rolls the pod through the generated name's hash);
    credentials from a Secret; the pod flags from the downward API.
  - **A crashed daemon** recovers with no manual step. The workload's
    calls fail with `ENOTCONN` (nothing is written where it would not be
    recorded). The kubelet restarts the sidecar (`restartPolicy:
    Always`, with its backoff after repeated crashes), which runs with
    `--on-stale-mount recover`: it recovers the journal and stacks a new
    mount over the dead one, and propagation carries it into the
    workload containers and `subPath` binds, which are not restarted.
    The kind test checks this after `kill -9` from the node (§18); a
    write that races the recovery and succeeds must be recorded.
    `--on-stale-mount hold` remains for operators who want to recover
    by hand ([runbook](runbooks/crashed-sidecar.md)).
  - **A clean exit while the pod runs** (SIGTERM to the sidecar, an
    unmount from outside, a `--passthrough on` failure) leaves no stale
    mount: the daemon unmounts, commits and pushes, and the kubelet
    restarts it into a fresh mount. The gap is fenced by the workload's
    read-only `emptyDir` mount (`readOnly: true`, `recursiveReadOnly:
    Disabled`): the FUSE mount propagated into it is a mount of its own
    and stays writable, but while none is mounted, writes to the bare
    directory fail with `EROFS`. A `subPath` consumer cannot be fenced
    that way (a read-only bind of the FUSE root would make the mount
    read-only); it keeps the dead mount (`ENOTCONN`) until its liveness
    probe restarts it, and if it restarts before the sidecar has mounted
    again it binds the bare directory, which `--seal-mountpoint` made
    root's with mode 0000: `EACCES` for a non-root consumer (or root
    without `CAP_DAC_OVERRIDE`). A root consumer with default
    capabilities still writes there unrecorded (hidden by the next
    mount, which warns): give such workloads the read-only `emptyDir`
    instead. SIGTERM
    stays final for the daemon (it never waits for a remount): the pod
    may be terminating, and the kubelet decides whether it comes back.
  - **Limits of in-place recovery.** Descriptors the workload opened
    with kernel passthrough before the crash keep writing to the backing
    file, unrecorded, until closed; since the workload keeps running,
    such a descriptor can outlive the crash longer than under a pod
    restart (`--passthrough off` where every write must be recorded). A
    hung daemon (alive, not answering) is not detected. Each crash adds
    one stale layer until the sidecar stops.
  - **Privilege.** `privileged: true` is what Bidirectional propagation
    requires; its cost and mitigations are in
    [deployment.md](deployment.md#9-security-trade-offs-of-the-privileged-sidecar).
    The CSI driver (§14.2) removes it from workload pods.
- **Docker** ([docker.md](docker.md), examples in `examples/docker`): the
  same pattern without Kubernetes. One agentfs container per volume
  mounts FUSE inside a host directory bound with `rshared` propagation;
  workload containers bind that directory read-only, `rslave`, with
  writable submounts, so the mount (and a mount stacked over it after a
  crash) reaches them and the bare directory is the fence. The backing
  tree and the state area share one Docker volume that only the agentfs
  containers mount. Docker's restart policy plays the kubelet's part in
  crash recovery (`--on-stale-mount recover`). There is no Docker volume
  plugin (§19.1 "Docker").

### 17.2 Layer 2: CSI driver (Rust)

Phase 4 (§14.2). The crate `crates/agentfs-csi` is a library and the
binary `agentfs-csi`, one per node (§14.2). It depends on
`agentfs-core` (`init_with`, `start_from`, `reopen_with`, the release
decider, `branch_claim`, the state lock, `FrontLease::holders`, name
validation) and `agentfs-fuse` (only the mount-state probe), never on the
CLI. Modules:

- `csi`: the CSI v1.13.0 protobuf, vendored under `proto/` and compiled by
  `build.rs` with protox and tonic-prost-build (no `protoc`);
- `params`: StorageClass and PV parameters, the `volumeName` template,
  `podInfoOnMount`; `layout`: the paths under basePath and node-state,
  handle validation, target keys; `records`: `csi.json` and the node's
  staged and published records, written atomically;
- `identity`, `controller`, `node`: the tonic services. The controller's
  calls are serialised and run blocking work off the async threads; `node`
  validates and delegates to `supervisor`;
- `supervisor`: per-volume locks, the children (one service per staged
  volume, one front per target), readiness (the service's address file,
  the front's mount state), restarts with backoff, recovery by bind
  stacking, reconcile at start, shutdown;
- `launcher`: `trait Launcher` and `AgentfsLauncher`, which spawns the
  `agentfs` binary with tokio, forwards its output prefixed with a label,
  keeps its last lines for errors and reports its exit; `mounter`: `trait
  Mounter` and `KernelMounter` (bind, read-only remount, shared
  propagation, `MNT_DETACH`). Both traits are public so tests inject
  fakes;
- `server`: the endpoint, a tonic server on a Unix socket (mode 0660), and
  graceful shutdown on SIGTERM.

**Process model.** The node plugin is `tini` → `agentfs-csi` → its
children, the `agentfs` binary (`serve-history`, `mount
--history-service`), each in its own process so a crash is contained and
restartable (§14.2). The controller runs no children.

### 17.3 Libraries (verified 2026-10-12)

| Need | Choice | Notes |
|---|---|---|
| FUSE | `fuser` 0.18 | Active. Multi-threaded dispatch with cloned fds, passthrough (`BackingId`). Default features are pure Rust: `mount(2)` as root, `fusermount3` otherwise, no libfuse. It sends the attribute inode number as the node id, so `st_ino` is the node id (§6) |
| Git objects, commits, push, credentials | `git2` (libgit2) | Active. Has **no** filter or merge-driver registration, and its fsync option is only reachable through `libgit2-sys` |
| TLS and SSH transports | `git2` feature `https` (OpenSSL) behind agentfs's `https` cargo feature; for `ssh`, agentfs's own libgit2 subtransport over `russh` 0.64 (ring and the pure-Rust `rsa` crate; `tokio` current-thread runtime per connection), libgit2 built without libssh2 | `https` is a C library, so feature-gated: default builds stay free of OpenSSL. `ssh` needs no OpenSSL or libssh2 (checked by `ci/check-deps.sh`), and russh gives the transport the host key before authentication, a host key algorithm preference and host certificates, which libssh2 1.11 cannot negotiate. The `rsa` crate has a timing side-channel advisory on private-key operations (RUSTSEC-2023-0071): Ed25519 keys are recommended |
| SSH host keys | agentfs's own known_hosts parser over `base64` and `sha1_smol` (HMAC-SHA1 for hashed names, written by hand and tested against RFC 2202) | Pure Rust. The SSH transport checks host keys with it before any authentication, and never reads `~/.ssh` (§11.1) |
| Ignore and attribute rules | `gix-ignore`, `gix-attributes` | Pure Rust; chosen over libgit2's rules for the reasons in §4.4. gitoxide has no push, so it does not replace `git2` |
| LFS | none exists: implemented in agentfs (§7) over `ureq` 3 (default features off: pure Rust, no TLS; proxies through its built-in HTTP `CONNECT` proxy, no extra feature) and `sha2` 0.11; concurrency on `std::thread::scope`, `git-lfs-authenticate` over the `ssh` feature's russh transport, no further crates; the `https` feature adds `ureq`'s `native-tls` (OpenSSL, as `git2/https`; its webpki root bundle is compiled in but unused: the platform verifier reads the system store) | ureq's `native-tls-no-default` feature does not compile the native-tls connector in 3.4 (its code is behind the `native-tls` feature), hence `native-tls`. `reqwest` stays in the binary only, for OTLP |
| Telemetry | `opentelemetry` 0.33 (API) in the core; `opentelemetry_sdk` and `opentelemetry-otlp` (`http-proto`, `http-json`, `grpc-tonic`, `reqwest-blocking-client`) in the binary, with `tokio` (one worker, started only for gRPC); the `otlp-tls` feature adds `opentelemetry-otlp/tls-ring`, tonic's `tls-ring` and `tls-native-roots`, `rustls` 0.23 (ring), `rustls-native-certs` and reqwest's `rustls-no-provider` | OTLP http/protobuf, http/json and gRPC export. tonic and prost were already in the workspace for the CSI driver, and the OTLP gRPC code comes pre-generated in `opentelemetry-proto`: no protoc. The default build is verified pure Rust (no OpenSSL, ring, aws-lc, cmake or protoc); `otlp-tls` adds ring only (C and assembly, no cmake), never aws-lc or OpenSSL; `ci/check-deps.sh` checks both. With rustls in reqwest, ring is installed as the process's crypto provider |
| Patch submission | the `git` CLI on the producer's side (`ls-remote`, `fetch`, `commit-tree`, `push --force-with-lease`) | Runs in the producer's repository with its own credentials and config, so agentfs needs none of them; the lease makes a number create-only (§12) |
| Repack | the `git` CLI (`git repack --geometric=2`, git 2.32 or later) | libgit2 has no repack or gc. Run by the commit loop with system and global config off (§8); the image ships `git` |
| Container image | `debian:trixie-slim` with `fuse3`, `libssl3t64`, `ca-certificates`, `git` and `tini`; built on `rust:1.99-trixie`; both pinned by digest. Ships `agentfs` and `agentfs-csi` | Not distroless: the daemon runs `git repack` (§8), and operators follow the runbooks with `git` through `kubectl exec`. `tini` (`-s`, a subreaper: the node plugin shares the host's pid namespace) reaps the CSI node plugin's children's orphans. Runs as root (§17.1). `linux/amd64` only; published to `ghcr.io/mcurcio/agentfs` as the commit sha and `latest`, only after both kind e2e jobs pass |
| CSI gRPC | `tonic` 0.14 and `tonic-prost`, `prost` 0.14, `prost-types`, on `tokio` (in `agentfs-csi`) | Transport and codegen features, no TLS: the socket is local |
| Front/service gRPC (2026-10-15) | `tonic` 0.14 (`codegen`, `router`, `server`; never `channel` or TLS), `tonic-prost`, `tonic-health` for `grpc.health.v1`, `prost` 0.14, and `hyper` 1 (`http2` client) with `hyper-util` and `tokio-stream`, on `tokio` (in `agentfs-core`) | The front's client is hyper's HTTP/2 connection over the Unix socket, so a request hyper hands back unsent is known never to have left (§14.1); tonic's `channel` would hide that. No TLS: the socket is node-local. `ci/check-deps.sh` checks the default build still links no ring or rustls |
| gRPC codegen | `protox` 0.10 and `tonic-prost-build` 0.14 in `build.rs` (both crates) | A pure-Rust protobuf compiler with the well-known types bundled: no `protoc` on build hosts or in the image |
| CSI spec | `csi.proto` v1.13.0, vendored in `crates/agentfs-csi/proto` | Pinned by copy; updating it is a deliberate change |
| CSI sidecars | csi-provisioner v6.4.0 (`--node-deployment`, in the node plugin pod), csi-node-driver-registrar v2.18.0, livenessprobe v2.20.0 from `registry.k8s.io/sig-storage` | Pinned by digest in `deploy/csi/kustomization.yaml` |

## 18. Acceptance tests

1. **No gaps.** Each of these produces the expected recorded state:
   - an editor's temp-file-and-rename save;
   - an append;
   - `truncate`;
   - `mv` of a directory;
   - `rm -r`;
   - a hard link;
   - an executable-bit change;
   - a symlink;
   - unlink of an open file;
   - rename over an open file;
   - log rotation while the writer is open.
2. **Before return.** Killing the daemon right after `close` returns, then
   remounting, shows the change in history.
3. **Crash safety.** Killing the daemon or node at every step of record → commit
   → ref update → push loses nothing and duplicates nothing. Automated
   with failpoints (`agentfs_core::failpoints`, the `failpoints` cargo
   feature, test builds only) in three tiers, each checked by one oracle
   (`crates/agentfs-core/tests/support/crash_oracle.rs`: every promised
   record committed exactly once, first-parent `Agentfs-Seq` strictly
   increasing, no empty commits, `git fsck`, nothing left in `pending/`,
   remotes at or behind the head with any diverged head saved once): the
   engine killed or failed at every point a workload reaches
   (`crash_points.rs`, also a failure then a kill), the processes
   (`record`, `run`, `push`, `serve-history` and a front, `patch apply`;
   `crash_daemon.rs`), and a FUSE mount (`crash_mount.rs`). Power loss is
   tested apart: `dm-log-writes` records every write and flush of a
   workload on ext4 and XFS, and xfstests' `replay-log` replays the log to
   every FUA (or flush) point, where the same promises are checked on a
   snapshot (`ci/power-loss.sh`).
4. **Any user.** Root and UID 10000 in one pod read and write with normal
   permission semantics. `chmod 600` persists.
5. **Invisible.** No history, journal, state or credential is reachable through
   the mount.
6. **Rules.**
   - Ignores apply only to new paths.
   - A tracked file stays recorded after it is ignored.
   - With the default settings, tracked rule files cannot turn recording off.
7. **Nested repositories.**
   - A clone, a bare clone and `git init` at the root behave as §5 says.
   - Nothing is deleted from history by nesting.
8. **Remotes.**
   - Interval pushes reach two remotes.
   - An unreachable remote, a refused push and a non-fast-forward each raise the
     alarm while the filesystem keeps working.
9. **Pre-commit checks.** A path git's fsck or a remote limit would refuse is not
   recorded, and raises an alarm.
10. **Shared repository.**
    - Two volumes push to one repository on their own branches and ref
      namespaces without contending.
    - A volume's recorder writing into another volume's refs is detected as
      divergence.
    - Committer identities follow the configured templates.
11. **Recovery.** Following §15, a file and a whole commit are restored, and the
    restore is recorded.
12. **Telemetry.** The metrics in §13 are exported and move as expected
    in the tests above. Checked against a real collector
    (`ci/e2e-telemetry.sh`): the record, commit, repack, push, precheck,
    remote, patch, state, journal and export-failure metrics, the
    history service's fronts gauge and a front's round trip and service
    failures, the record, commit and push spans, and the alarm logs;
    without one (`cli_telemetry.rs`): the patch and fork spans and
    `agentfs.fork.publish.duration`. Not checked as exported:
    `agentfs.history_service.superseded` and `.resyncs`,
    `agentfs.fuse.create_as_caller_fallbacks` and `.racy_rechecks`,
    `agentfs.reconcile.records` and `.failures`, and
    `agentfs.nested.gitdir_heuristic`; the behaviour each counts is
    tested, its export is not.
13. **Performance.** Recorded latency and throughput for:
    - a mid-size `git clone` with `nested = working-tree`;
    - 30,000 small files written into an excluded `node_modules/` (the
      shape of an `npm install`; npm itself is not run);
    - `rm -rf` of 100k files;
    - closing a 1 GB file, on XFS (reflink) and ext4 (copy).
    Automated: `.github/workflows/bench.yml` runs `ci/bench.sh` on fresh
    XFS and ext4 loop filesystems on every push to main, with the
    synchronous step without FUSE (`record_step` bench), a 4 KiB close and
    100k creates as well, checks the history after each workload, and
    publishes the results; see [performance.md](performance.md).
14. **Phase 2.** SQLite in WAL mode with `synchronous=NORMAL`, and in DELETE mode:
    every recorded version restores and passes `PRAGMA integrity_check`. LFS
    content is readable through an ordinary `git clone` plus `git lfs pull`.
15. **Phase 3.** Three queued patches where the second conflicts:
    - the first applies;
    - the second and third stay queued;
    - the volume mounts;
    - a crash and replay does not apply the first twice.
16. **Phase 4.** Two pods on one node mount one volume. Each pod's changes
    carry its own committer identity, and history keeps every boundary from
    both. Run with two front mounts on one node (§14,
    `ci/e2e-multipod.sh`: renames and removals across fronts, also of a
    file still open in the other front, a concurrent burst on one path, an
    outage, and a `kill -9` of the service with a journaled, uncommitted
    record), and through the CSI driver on one node (two restricted pods
    on one claim, `ci/e2e-csi.sh`, with the front, service and plugin
    crash recoveries of §14.2). A `ReadWriteMany` claim is refused
    (`ci/e2e-csi.sh`'s negative case, and every multi-node mode at every
    call in `crates/agentfs-csi/tests/grpc.rs`). The state lock, its
    holder line and the mount-option check hold across two NFS clients
    of one export, and a volume moves between two node plugins on it,
    refused on the second while staged on the first (`ci/e2e-nfs.sh`).
    Two nodes writing one volume are out of scope by decision (§19.1).
17. **Forks** (§11.5). A source with a fork-only remote never pushes there
    and keeps its remotes, limits and patch remote; a fork pushes only to
    its fork remote at `agentfs/<fork>` and has no patch queue (unit tests
    in `config.rs`). Create reads the source live, without its lock,
    leaves it byte-identical, writes base's tree as stored without
    following a symlink, and refuses what it cannot fork; finish writes
    the change on base and the finish commit once; publish returns
    published only once the remote holds both refs at exactly their ids,
    never moves another writer's ref, and is not blocked by a forge moving
    the fork's branch; status fails closed (`fork/tests.rs`). The same
    patch commit queued twice applies once, as a marker the second time,
    even after the workload reverted it (`patch/tests.rs`), and end to end
    through the CLI with two concurrent `patch submit` (`cli_fork.rs`).
    On kind, a sandbox's ephemeral claim on a fork class clones a source
    claim in use; its pushes reach only the fork remote; a Retain fork
    class is denied at admission; with the fork remote blocked, deleting
    the sandbox leaves the PV Released and DeleteVolume UNAVAILABLE across
    a node-plugin restart; once reachable, the release scan alone publishes
    the change and finish refs and the PV goes; for a second fork, a
    pre-receive hook refusing `refs/agentfs/*` makes the publish refused
    and no further finisher starts; that finished fork's directory
    mounted read-write through a static PV is read-only (`EROFS`); `fork
    finish --retry` in the node plugin publishes it and its PV goes; the
    first change submitted twice applies once, and a later submit is
    refused early
    (`ci/e2e-csi-forks.sh`). Refused, abandoned and remote-gone publishes,
    the single refusal decider, settings that fail closed and the
    read-only recorder are tested in `fork/tests.rs`; `fork status`,
    `abandon`, `list` (forged change refs skipped), `finish --retry`, the
    read-only commands and submit's early refusal end to end in
    `cli_fork.rs`; the release scan skipping refused and abandoned forks in
    `release_scan.rs`; the read-only publish in `tests/supervisor.rs`;
    DeleteVolume after abandon in `tests/grpc.rs`. `fork deliver`
    (`cli_fork_deliver.rs`): the inbox method queues exactly the checked
    change commit in the named volume's inbox, once; the branch method's
    squash commit has the change's tree on base, a repeat is already
    delivered, a forged ref carrying the trailer is refused and never
    moved, and `--onto` merges a clean change and refuses a conflicting
    one with its paths; the history shape pushes the finish commit; a
    command gets the v1 payload on stdin with its arguments unexpanded and
    `PATH` alone, and is killed at its timeout; a webhook gets the payload
    with `Idempotency-Key` and the bearer token, its status decides, and
    no redirect or proxy from the environment is followed; and no CSI
    source uses delivery. The payload's schema, its serde round trip and
    the factory's validation are unit tests in `fork/deliver.rs`. Hidden
    forks (`cli_fork.rs`): with `agentfs.forkRefs = hidden`, a new fork
    is published at `refs/agentfs/<fork>/history` and nothing of it under
    `refs/heads/`, `fork status`, `list` and `deliver` work on it, and a
    fork created before the setting changed keeps its branch; both keys'
    values and the include rule are unit tests in `config.rs`. Telemetry
    (`cli_telemetry.rs`): a finish and publish export both fork spans with
    their attributes, `ForkFinished` and `ForkPublished` at INFO and one
    `agentfs.fork.publish.duration` point; a refused publish exports
    `ForkRefused` as an alarm and no point. Every refused reason has its
    section in the refused runbook, which `fork finish` links (a unit test
    in `fork_cmd.rs`). The history-rewrite runbook with forks runs as its
    own step of the `e2e-runbooks` job (`ci/e2e-runbooks.sh forks`): on a
    source with a fork based before the secret and one after, both
    pushed, the secret's blobs are gone from the source remote and the
    fork remote, only the later fork is abandoned and its refs deleted,
    and the earlier one still publishes.

**Where they run.** Tests 1, 2, 4, 5, 6, 7 and 11 run through real mounts
in the `fuse` CI job (`crates/agentfs/tests/mount.rs`, as root, with
passthrough `auto`, `on` and `off`), along with SQLite in WAL and DELETE
mode and git (`gc`, `fsck`) on the mount. Test 14's SQLite part runs there
too, with the databases in the `fsync` class (every recorded version
restored with its `-wal` or `-journal` and checked), and as
`ci/e2e-sqlite.sh` in the same job: long-lived Python writers, the
history catching up within one `agentfs.coalesce` of a WAL writer's last
commit while it is still open, captures at least one interval apart (a
DELETE-mode writer's per-transaction `-journal` closes capture nothing),
every version checked with `PRAGMA integrity_check` and whole
transactions, and restores of one member by the runbook, live through
the mount and directly, each bringing its whole group in one commit. Test 11 also runs as `ci/e2e-recovery.sh` in the
same job (the recovery runbook end to end, ending with a push of the
restore commits), and without a mount in `crates/agentfs/tests/cli_restore.rs`;
the divergence and history-rewrite runbooks run in the `e2e-runbooks` job.
Tests 8 and 12 run in the `e2e` jobs against a real git server
and OpenTelemetry Collector (test 12 with real failures for the record,
commit and repack failure counters, and a real `serve-history` with FUSE
fronts for `agentfs.history_service.fronts`, `agentfs.front.round_trip`
and `agentfs.front.service_failures` (`timeout` and `connect`); the
export-failure counter against a dead endpoint; every OTLP transport,
http/protobuf, http/json and gRPC, plain, over TLS with a bearer token and
over mutual TLS (and refused: a wrong token, no client certificate, an
unknown CA), in `ci/e2e-otlp-transports.sh`; and without a collector
binary in `crates/agentfs/tests/cli_telemetry.rs`); test 8 runs over smart HTTP and HTTPS
(`e2e-remotes`) and over SSH (`e2e-ssh`, against OpenSSH's sshd). The
`e2e-kind` job (`ci/e2e-kind.sh`, in the `image` workflow) runs the
phase 1 deployment in a kind cluster with the image just built: test 4
in a pod (root and uid 10000 writers in separate containers, one through
a `subPath` of the FUSE mount), test 5 for the writers (no `/data`,
nothing but the tree under the mount), the operator-rules part of test
6, test 8 over HTTP from a pod with the credential Secret (authors and
pod, namespace and node trailers checked on the remote), test 11 through
`kubectl exec` and the control socket, the final commit and push on pod
deletion, and test 2 in a pod: `kill -9` of the daemon from the node,
`ENOTCONN` for the workload, the kubelet restarting the sidecar, which
stacks over the stale mount with no manual step, both writers carrying
on without a restart (the `subPath` one included), and the pre-crash
and post-crash writes reaching the remote; then `agentfs push` through
the recovered daemon, the clean-exit fence (SIGTERM to the sidecar:
`EROFS` in the gap, nothing of it in history or the backing tree, the
`subPath` writer restarted inside the gap refused with `EACCES` by the
sealed mount point, or recorded if the sidecar was back first, then
restarted onto the new mount by its liveness probe; the seal itself
checked from the node) and a pod deletion
leaving nothing mounted on the node. The ignored root tests in
`crates/agentfs/tests/mount.rs` stack over a crashed mount with a
`subPath`-like bind and a slave mount namespace, refuse without
`CAP_SYS_ADMIN` and for fronts, and detach every stale layer; others
check the sealed mount point and that stopping `agentfs mount` pushes
its final commit (§11.2). Test 14's LFS part runs in the `e2e-lfs` job
(`ci/e2e-lfs.sh`: git-lfs's `lfs-test-server` behind nginx, the real
git-lfs client, `git clone` plus `git lfs pull`, over http and https;
`ci/e2e-lfs-transfers.sh`: concurrent downloads seen overlapping in the
nginx log, uploads resuming by object after a 503 for one object,
downloads resuming by byte with `Range` after a `kill -9`, the per-remote
object size limit, `lfs fetch`, `lfs prune` keeping what a remote lacks,
and proxies through tinyproxy, including https through `CONNECT`) and
in the `e2e-lfs-ssh` job (`ci/e2e-lfs-ssh.sh`: `git-lfs-authenticate`
through OpenSSH's git-shell, interoperating with the real git-lfs in
both directions, expiry per batch, refused headers, a server without the
command, an untrusted host key with no key offered), and without a
server in `crates/agentfs/tests/cli_lfs.rs` (local and `file://`
remotes, `lfs fetch` and `lfs prune`; `git lfs pull` where git-lfs is
installed, as on CI runners); the `lfs-storage` runbook's blocks run in
`ci/e2e-runbooks.sh`. Test 15 runs without a mount in
`agentfs_core::patch` (unit tests, including a crash at each step) and
`crates/agentfs/tests/cli_patch.rs` (racing submitters over `file://`, a
blocked and resolved queue), through a real mount in the `fuse` job
(`patches_apply_before_the_mount_serves`), and as `ci/e2e-patches.sh` in
the `e2e-patches` job: racing producers over smart HTTP with basic auth,
the conflict, the push of the queue refs, a crash between the commit and
the ref move, and a replaced patch. Test 16 runs at single-node level:
without FUSE in `crates/agentfs-core/tests/history_service.rs` (two
fronts and one service in one process: committers, outage and restore, a
hung service, refusals, a crash replay) and
`crates/agentfs/tests/cli_history.rs` (`serve-history` and `record
--history-service`, a kill -9 and restart); through real mounts in the
`fuse` job (`two_fronts_one_service`, with passthrough `auto`, `on` and
`off`); and as `ci/e2e-multipod.sh` in the `e2e-multipod` job (passthrough
`auto` and `off`): two front mounts of one backing tree as pods `pod-a`
and `pod-b`, files from each, a file created in one and renamed and
removed through the other, a directory `mv`, a shared file written in
turn, every commit's committer equal to its `Agentfs-Pod` trailer, `EIO`
and the alarm while the service is down and the catch-up after it
restarts, a `kill -9` of the service right after a close returned, the
push, and the per-close cost through a single-process mount and a front.
Through the CSI driver (§14.2), test 16 runs: without FUSE in
`crates/agentfs-csi/tests/grpc.rs` (the Identity and Controller services
over a real socket), `tests/supervisor.rs` (the supervisor with a fake
launcher and mounter: restarts in the same slot, bind stacking,
reconcile, ordering) and `crates/agentfs/tests/csi_service.rs` (the
supervisor driving a real `serve-history`: two pods' committers, a `kill
-9` with a journaled commit, the final commit at unstage; the release
scan pushing an unstaged worktree with the real `agentfs push` until its
remote holds it); in the `csi-sanity` job (`ci/csi-sanity.sh`: csi-test
v5.6.0 against `agentfs-csi` with real services and FUSE fronts, then
nothing left behind); and in the `e2e-csi` job of the `image` workflow
(`ci/e2e-csi.sh`, kind, the image just built): the RBAC inventory through
`kubectl auth can-i`, two restricted pods (uid 10000 and 10001, one
fsGroup) on one claim with distinct committers, trailers and uid authors
on the runner's git server, `kill -9` of one front (the other pod
unaffected, the front back in its slot and live through
`HostToContainer`), of the service right after a write (committed and
pushed), and deletion of the node plugin pod (reconcile; one pod live,
the other after a container restart), the refusals of inline volumes and
`ReadWriteMany`, and teardown through DeleteVolume.
Test 13 runs in the `bench` workflow (`ci/bench.sh`, on fresh XFS and
ext4 loop filesystems; [performance.md](performance.md)).
Test 3 runs in the `crash` job (`ci/crash-tests.sh`: the engine matrix,
the process tests, and the mount tests as root) and in the `power-loss`
workflow (`.github/workflows/power-loss.yml`, ext4 and XFS, every FUA by
default and every flush on demand); a killed repack is a unit test
(`repack.rs`). Test 7's "nothing deleted by nesting" runs through a mount
and through a front in the `fuse` job (`nothing_is_deleted_by_nesting`).
Test 8's "the filesystem keeps working" runs through a live FUSE mount in
the `e2e-shared-remotes` job (`ci/e2e-remote-failures.sh`: unreachable,
hung, refused, auth, diverged and untrusted remotes at once, close
latency against a healthy baseline, recovery, and the hung push
abandoned at SIGTERM), and test 10's two volumes on one repository there
too (`ci/e2e-shared-remote.sh`: two FUSE mounts pushing at once to one
HTTP and one local repository, overlapping on the server) and as a unit
test (`push.rs`, with a same-named rival). Test 10's identity templates
run in `crates/agentfs/tests/cli_identity.rs` (every variable, the
fallbacks, refusals, fronts, a template change across a service
restart), through a mount and a front in the `fuse` job, and with custom
templates checked exactly in `e2e-kind` and `e2e-csi`.
A volume continued on another node of a two-worker kind cluster, by a
new volume started from its branch, runs in the f8 workflow
(`ci/e2e-csi-move.sh`, §14.2). Not automated yet: the CSI driver on
hardened node images (Talos) and across node reboots.

The `fuse` job also runs the README's from-source quick start as
written, as root (`ci/readme-quickstart.sh`): its remote log after
`kill %1; wait` must show the edited file, which only the final push at
shutdown (§11.2) puts there. The `docs` workflow runs the README's
Docker quick start as written against the published image
(`ci/readme-quickstart.sh docker`): the history it leaves must hold the
workload's file with uid 10000 as its author. The README's Kubernetes
quick starts are not run as written; the manifests and charts they use
are tested by the `image` and `helm` workflows.

Every CI job runs on GitHub's runner image pinned by name,
`ubuntu-24.04` rather than `ubuntu-latest` (which moves to Ubuntu 26 on 2026-10-19):
the kernel, FUSE, kind and nginx the tests rely on change only when the pin
does. `crates/agentfs-core/tests/docs_reference.rs` fails on any other
runner.

## 19. Decisions and open questions

### 19.1 Decisions (2026-10-12)

| Topic | Decision |
|---|---|
| Storage model | Real backing tree with FUSE passthrough; the projection is logical only |
| Several pods on one volume | Supported by the CSI driver, one committer identity per replica (§14, §9); built at single-node level as `serve-history` and front mounts (§14.1), started per volume and per pod by the CSI driver, one node per volume (§14.2) |
| Rejected pushes | Alarm, retry, keep operating (§11.3) |
| LFS object size limit | `remote.<name>.agentfsMaxLfsObjectSize`, per object: the smallest across valid remotes refuses LFS content at record time (`PathRefused`, `lfs_object_too_large`, also before the freeze) with a commit-time backstop, never falling back to a blob; each remote's own limit fails its push as `too_large` at stage `lfs` before any upload; no per-push LFS byte budget (§7, §11.3, 2026-10) |
| Push size limit | `remote.<name>.agentfsMaxPushSize`, measured before connecting with libgit2's pack builder (git pack only, not LFS); a backlog over it pushed in first-parent steps with `PushLimited` and the next step at once, applied and diverged refs held for the full push; a single commit over it fails as `too_large` with backoff, nothing sent; `agentfs push` steps until up to date (§11.3, 2026-10-13) |
| Remotes | Any git remote, several at once (§11.1) |
| Push timing | Intervals, independent of pod lifecycle, plus one bounded push at shutdown in `mount`, `run` and `serve-history`: after the final commit, one attempt per remote that lacks commits, within `--push-grace` (5 s; `0s` skips it), never retried and never extending shutdown past the grace. This sits beside the earlier decision that pushes run at intervals and that a pod's stop never waits on a push: the stop waits at most the grace, and what stays unpushed goes out on the next start (§11.2, 2026-10-13) |
| Recovery | `agentfs restore` and `agentfs revert`, plus operator runbooks; further tooling discovered from use (§15) |
| Telemetry | OpenTelemetry (§13) |
| Secret scanning | Not in scope (§10) |
| Repository layout | Several volumes may share a repository, each with its own branch and ref namespace (§11.4) |
| Identity | Configurable templates with defaults (§9) |
| Backing filesystem | Both supported. XFS (reflink) makes the synchronous step size-independent; ext4 falls back to a copy. Recommendation, from the benchmark (§3.2, §18 test 13): XFS (or btrfs) for volumes holding large files, where ext4 costs about 2.9 s per clean GiB at close; either for small files, where the journal `fsync` dominates on both |
| Performance budget | Advisory, checked by every benchmark run (`ci/bench-budget.json`, [performance.md](performance.md#performance-budget)): about 1.5 times the slowest of the first three CI runs (six jobs on two CPU types), for example 3.5 ms for a small file's step at the median; a result over budget warns and never fails CI (2026-10-13) |
| One process per state area | An exclusive `flock` on `meta/lock`, taken by `mount`, `run`, `serve-history`, `record`, `flush`, `patch apply`, `fork create`, `fork finish`, `fork abandon`, `init` of an existing state area, and `push`, `restore` and `revert` in direct mode (when no daemon answers on the control socket); exit 75 when held, `--wait-lock` to wait; `status` is read-only (§2.3) |
| FUSE layer placement | A separate library crate, `agentfs-fuse`, whose kernel-independent `Ops` carries all behaviour; the fuser adapter only maps replies (§17.1) |
| Dirty detection | Armed inodes compared by stat stamp, with git's racy-timestamp rule; created and `O_TRUNC`-opened files always recorded at their next close. A boundary that would record a file again only because its last record was racy compares it with that record's frozen copy, kept open, and skips the record if the bytes are equal and the copy is still the last state journaled at the path (2026-10-13; §3.1, §19.2 item 4) |
| Directory renames | One `move` change in the synchronous step, then reconciled on the commit thread before the move is committed: excluded paths dropped, newly recorded paths added, and files changed within 60 s of the move whose stamp is not the last journaled recorded again; paths refused at the destination (path rules, `agentfs.maxSize`) left out with `path_refused`; every reconciling read confined with `openat2`, never through a symlink or into a FIFO; failures retried with backoff for at least 30 s; the move and its reconciliation share a commit (2026-10-13; §3.1) |
| Journal compatibility | A newer change kind in an intact journal frame is an error for an older binary, never a torn tail: flush before downgrading (2026-10-13; §3.3) |
| `.git` paths | An exact `.git` component that is a git directory or gitfile is skipped without an alarm; any other `.git`, look-alikes and invalid names raise `PathRefused` (§5) |
| Layout | The state area, backing tree and mount point are separate; credentials are never under the backing tree. `mount` refuses otherwise (§17.1) |
| Passthrough | `auto` by default: used when the kernel and privileges allow, off with the reason reported otherwise, and switched off per inode (not per mount) if registering its backing file fails; `on` fails the mount instead (§17.1) |
| Failed namespace records | `EIO` plus a repair queue the sweep drains from the backing tree's current state, kept in `meta/repairs` across mounts and drained before the next mount serves (§3.5) |
| Privileges | Decided from the effective capabilities, not the uid: `CAP_SETUID` + `CAP_SETGID` (with the two below) to create new objects as their caller, `CAP_CHOWN` + `CAP_FOWNER` to give the rest to their caller, `CAP_SYS_ADMIN` in the initial user namespace for passthrough (§6, §17.1) |
| `st_ino` | The FUSE node id, stable while mounted and shared by hard links, not across remounts (§6) |
| `.gitmodules` checks | git fsck's `gitmodulesParse`, `Name`, `Url`, `Path`, `Update` and `Large` are refused at record and commit time (`gitmodulesParse` too, though git only reports it); a port of git's config parser and URL checks, tested against the `git` binary; `http(s)` URLs must pass both git 2.39's and 2.40+'s checks (§3.4) |
| Synchronous-step bound | Content scans of special files run at record time only up to 1 MiB, the tree rule file bound; larger files are scanned in full at commit time. Size checks always run at record time (§3.4) |
| Nested custom-template gap | At commit time, a write at a path new to history under a directory that has since become a bare git directory is dropped silently; only with the real backing tree known (§5) |
| Telemetry transports | OTLP over http/protobuf (default), http/json and gRPC (a one-worker tokio runtime only when used), per signal; `https` collectors with the `otlp-tls` feature (rustls on ring): a pinned CA bundle or the system store, mutual TLS, bearer tokens through the headers; files read at start, a bad one turns its signal off with a warning (§13) |
| Telemetry export failures | Each OTLP exporter is wrapped: a stderr warning at once, then at most once a minute with a count, a recovery line, and the counter `agentfs.telemetry.export_failures{signal}`; telemetry still never stops a command (§13) |
| Restore modes | Live restore is supported: with a mount running, `restore` and `revert` ask it over the control socket and it writes through its own mount, so caches and dirty detection stay coherent; with nothing running they take the lock and write the backing tree directly; `run` and `serve-history` serve none (exit 75) (§15) |
| Restore semantics | `git restore --source=<commit> -- <path>` for restore, the inverse of the first-parent diff for revert; only commits on the volume's branch; untracked files never touched (§15) |
| Restore safety | Every check before any write; content not in history is never overwritten, even with `--force`; `--force` overrides only what loses nothing (later recorded changes to a reverted path, files open for writing through the mount); each path is re-checked just before it changes; a partial apply is recorded, a tree already holding the target is recorded (direct) or refused (mount), and a re-run finishes the job (§15) |
| Operation commits | The applying process or thread is registered as an actor; its records get trigger `restore`, the operator as author, and the operation in the journal record; each operation is its own commit with `Agentfs-Restore` or `Agentfs-Revert` (§9, §15) |
| Control socket | `meta/control/agentfs.sock`, directory 0700, socket 0600, peer root or the daemon's user, addressed through `/proc/self/fd`; served by `mount`, `run` and `serve-history`; JSON lines, protocol v1, operations `push`, `restore` and `revert` (additive fields; one restore or revert at a time, a push beside it) (§2.2, §2.3, §15) |
| History rewrite | A documented procedure with `git filter-repo --prune-empty never` in a mirror clone, then a force-push with lease per remote by an operator; not a command (§10, §15) |
| SSH remotes | Built with the `ssh` cargo feature: agentfs's own transport on russh (2026-10, replacing libssh2). Each SSH remote names a known_hosts file (`agentfsKnownHosts`) and exactly one of a private key (`agentfsSshKey`, with `agentfsSshKeyPassphrase`, a passphrase file, if it is encrypted) and an ssh-agent socket (`agentfsSshAgent`); every file is re-read when it changes. The key is bound to the push URL's host and port, decrypted before connecting and offered once per connection; an agent's identities are offered once each, in its order (sshd's `MaxAuthTries` applies); as the URL's user, else `git`. `SSH_AUTH_SOCK`, global and `~/.ssh` files are never read (2026-10, §11.1) |
| Host key verification | Done by agentfs against `agentfsKnownHosts`, in the SSH transport, before any authentication; plain and hashed names, `[host]:port`, wildcards, negation and `@revoked` honoured; unknown, changed, unlisted-type and revoked keys refused, never learned; the host key types known_hosts lists are asked for first. Host certificates (2026-10): with a matching `@cert-authority` line, certificates are asked for first and trusted only if host type, CA listed and not revoked, signature, validity, principal (the URL's host) and no critical options all check out, as for OpenSSH (§11.1) |
| Untrusted host reason | SSH host key refusals and TLS certificates libgit2 refuses (`GIT_ECERTIFICATE`: a self-signed or unknown CA, a host name the certificate does not cover) fail the push as `untrusted_host`, apart from `unreachable`, so a possible interception alarms differently from a network problem. A TLS handshake that fails before any certificate check (a server that drops it, as for a certificate host mismatch reached over an IP address in CI) stays `unreachable` (§11.3, §13) |
| Phase 1 pod layout | One `ReadWriteOncePod` PVC mounted once at `/data` (`backing/`, `state/`: reflinks fail across mounts), `Recreate` strategy; the FUSE mount at `/agentfs/workspace`, a subdirectory of a shared `emptyDir` (Bidirectional in the sidecar, HostToContainer in workloads), so a dead daemon does not block the restarted sidecar or the pod's teardown (§17.1) |
| Crashed daemon in a pod | Automatic: the kubelet restarts the sidecar, which runs `mount --on-stale-mount recover` and bind-stacks a new mount over the stale one; propagation reaches the workload containers and `subPath` binds without restarting them, and a stop detaches the stale layers. A clean exit mid-pod is fenced by the workload's read-only `emptyDir` (`EROFS`) and, for `subPath` consumers, by the sealed mount point (`--seal-mountpoint`: root's, mode 0000, `EACCES` for non-root); they restart onto the new mount through a liveness probe. A stale mount of another volume is refused. `hold` (manual pod restart) remains (§17.1) |
| Sidecar probes | Startup `agentfs probe --allow-stale` (gates workloads; passes on a stale mount, so a restarted sidecar can recover), readiness `agentfs probe` (live only); no liveness probe on the sidecar, since the kubelet restarts a dead daemon and a probe would only kill slow live ones; a `subPath` workload gets one (`stat -f -c %T` is `fuseblk`); a shell-free subcommand reading mountinfo and `statfs` (§17.1) |
| Operator config in Kubernetes | Rules: the ConfigMap's `exclude` and `attributes` subPath-mounted read-only over `info/*` in the sidecar; settings and remotes: a git-config fragment in the same ConfigMap named by `include.path`, so `kubectl exec` commands and `git` see them. Includes must be absolute, exist, not set `agentfs.volume`/`agentfs.branch`, and stay outside the backing tree and mount point; `includeIf` is refused (§4.3) |
| `agentfs init` re-runs | Safe at every pod start: exit 0 for its own volume (taking the lock), 1 for another volume or branch with nothing changed; `--include` sets the includes declaratively; `--root` creates the backing tree (§2.3) |
| Image user | Root. The sidecar needs privileged for Bidirectional propagation, `CAP_SYS_ADMIN` for mount(2), `/dev/fuse` and passthrough, `CAP_CHOWN`/`CAP_FOWNER` for ownership, and to own the state area for libgit2. The init container drops every capability; workloads run as non-root (§17.1) |
| Image publishing | `linux/amd64`, published from `main` only after the kind e2e passes, pushing the tested image itself; tags: the full commit sha and `latest` (§17.3) |
| CI runners | Every job on `ubuntu-24.04`, pinned by name and checked by a test, so a runner image change is a deliberate commit (§18, 2026-10-13) |
| Kind e2e git server | The runner's nginx + `git http-backend`, reached from pods through a selector-less Service and an EndpointSlice on the kind network's gateway, so the credential Secret is exercised over HTTP basic auth and the bare repository stays on the runner for assertions; the daemon is killed from the node (PID 1 ignores SIGKILL from inside its namespace) |
| LFS store selection | Decided at record time on the frozen size and journaled with the write (older journals read as blob): always blob for empty files, special files, `.lfsconfig` and unnameable paths; then `filter=lfs` (operator, or tree only with `respectTreeRules`), which `agentfs.store=blob` cannot override; then `agentfs.store`; then `size > agentfs.lfsThreshold`, which is unset by default. The blob limit does not apply to LFS content (§7) |
| LFS `.gitattributes` | A block at the end of the root `.gitattributes`, in history only, rebuilt every commit: one anchored, escaped line per LFS path with `git lfs track`'s attributes; the workload's lines are kept and forged blocks (exact BEGIN through END) replaced; a file holding only the block is agentfs's own. A pointer is never committed without its line: when the block cannot be written, that commit's LFS writes are blobs, then its `.gitattributes` writes are dropped, then the head's file is kept; a commit never fails for it. A directory named `.gitattributes` or `.gitmodules` is refused (`gitattributesBlob`, `gitmodulesBlob`) (§3.4, §7) |
| LFS endpoints and credentials | git-lfs's precedence (`lfs.pushurl`, `lfs.url`, `remote.<n>.lfspushurl`, `remote.<n>.lfsurl`, then derived `<url>.git/info/lfs`), from operator config only; local and `file://` remotes use the standalone-file layout; SSH remotes without `lfsurl` use `git-lfs-authenticate` over agentfs's own SSH transport (the push URL's path for uploads, `url`'s on the same host for downloads; its header is the credential, else `agentfsLfsCredentials`; reused until 5 s before it expires, else per attempt; no fallback; `git-lfs-transfer` not built), `git://` needs `lfsurl`. Credentials: `agentfsLfsCredentials`, else `agentfsCredentials` on the push URL's own origin, else the URL's password; bound to the origin, sent to action hrefs only on that origin and without a server-given `Authorization`; no redirects; the remote's proxy if set (§7, §11.1) |
| Proxies | git's `http.proxy` and `remote.<name>.proxy` (empty: none), from the state area's config only, never the environment; a plain `http://host:port` proxy (1080 by default), no authentication, no `https://` or SOCKS proxies; carries the remote's libgit2 `https` traffic and all its LFS requests (`CONNECT`), never SSH, and not plain `http://` git traffic, which libgit2 1.9 cannot proxy (it connects to the server directly); a bad value misconfigures the remote (every remote for `http.proxy`) (§11.1, 2026-10) |
| LFS uploads | Before every push attempt, for the objects the commits the remote lacks reference at managed paths; a failure is a `PushFailed` with stage `lfs` and no ref pushed. Batch API, `basic` transfer only, batches of 100 in sequence, `verify` when offered (§7) |
| LFS transfers | `lfs.concurrenttransfers` (git-lfs's key, default 8, 1 to 64) objects at once within a batch, per remote, on scoped threads; an upload stops starting objects at its first failure and lets those in flight finish; restores prefetch every object they need concurrently, each remote in name order taking what the earlier ones could not provide. Downloads resume by byte (`Range`, from `<state>/lfs/incomplete/<oid>`, 3 tries on `unreachable`); uploads resume by object (the batch API skips what an earlier attempt finished); `tus` byte-level uploads are not built, since no testable server implements them (§7, §15, 2026-10) |
| LFS cache and retention | `<state>/lfs/objects/aa/bb/<oid>`, written at commit time; nothing is ever deleted on servers, and the local cache is pruned only by `agentfs lfs prune`, run by an operator (§7) |
| LFS fetch and prune | `agentfs lfs fetch` downloads the objects the head, given revisions or every commit (`--all`) reference, from the remotes in name order, without the state-area lock; `agentfs lfs prune` deletes cached objects only when every valid remote's upload and download endpoints confirm them, refusing when a remote is misconfigured or has no endpoint; deletions under `<state>/lfs/lock` taken exclusive, which restores and fetches hold shared (§7, 2026-10) |
| LFS restores | A pointer is checked out from the cache, else downloaded from the remotes in name order, every object before any write; tree content compared by SHA-256; `.gitattributes` without the managed block; a dry run only reports objects missing from the cache, downloading nothing (§7, §15) |
| LFS client and TLS | agentfs's own client over `ureq` 3, pure Rust by default; the `https` feature adds `ureq`'s `native-tls` (OpenSSL, system trust store, as libgit2). An explicit https endpoint without the feature is a misconfigured remote; a derived one fails at push time as `local` (§7, §17.3) |
| `fsync` class: opting in | Operator `info/attributes` only; `agentfs.commit`, `agentfs.coalesce` (at least 1s) and `agentfs.maxSize` values are validated at open, and a bad one fails the volume naming the line (§4.2, §8) |
| `fsync` class: groups | Same directory, same `agentfs.group`, same stem (`db`, `db-wal`, `db-journal`), every member in the class; no group is a group of one; the smallest member interval wins; `-shm` never recorded (§8) |
| `fsync` class: capture | Class handles are never passthrough; every content or name change of a member holds a writer-preferring gate around its syscall, and a capture freezes the gate and copies every member as one atomic record. Triggers: `fsync` (rate-limited, and a capture failure does not fail the `fsync`), a trailing timer at max(first change, last attempt) + interval, the group's last close (`EIO` on failure), last release, unmount. A member's own namespace changes are folded into the next capture; a group an operator operation changes is captured once, at the operation's end. The capture holds the mount's name lock, so namespace changes anywhere on the mount wait for the copy. A member read error other than `ENOENT`/`ENOTDIR` fails the capture (§8) |
| Atomic records | A journal record flag: at record time a refused member, or one a path rule skips (`group_member_skipped`), skips the rest of the group (`GroupRefused`), at commit time a refused change drops the whole record; a refused capture counts as done (§3.4, §8) |
| `agentfs.maxSize` | An operator attribute for any path, refusing larger content (`max_size`) whatever the store, checked on the size on disk and on the frozen copy at record time, not again at commit; a file moved by a directory rename is checked against its new path's limit by the move's reconciliation and removed if over it (§3.1, §4.2, §8) |
| Repack | `git repack -d -q --geometric=2` with bitmaps off, one thread, bounded window, delta cache and big-file threshold (about 256 MiB), a cleared environment and system and global config ignored, skipped when nothing is loose, killed on stop, on the commit thread every `agentfs.repackInterval` (default 1h, `0`/`off` disables, at least 1m); skipped while a push holds its lock; never prunes; failures raise `RepackFailed` (§8, §13) |
| Patch submission | A patch is a single commit pushed to `inbox/<n>` with a create-only lease (`--force-with-lease=<ref>:`); a race loses and takes `n+1`. `agentfs patch submit` wraps the producer's `git`; plain git works the same (§12, [runbook](runbooks/patch-queue.md)) |
| Patch application | At mount only, after journal replay and before the service and FUSE start; a cherry-pick through the restore engine's checks, recorded as one atomic operation of kind `patch` with the patch's author and agentfs as committer; the first failure blocks the queue, raises `PatchBlocked`, and the volume mounts anyway (§12) |
| Patch idempotency | Decided by the branch's trailers (`Agentfs-Inbox`, `Agentfs-Patch`), never by refs or a cache; refs move after the commit, so a crash anywhere re-applies nothing (§12) |
| Queue push | Only to the patch remote, after every branch push attempt (failed ones included) and apart from it: the status ref, `applied/<n>` once the branch holds the commit, and `inbox/<n>` of every applied number deleted, leased on the oid listed; its failures never count against the branch (§11.3) |
| LFS test server | CI uses git-lfs's `lfs-test-server` (pinned by commit, built with `go install`) behind the git server's nginx, which authenticates with separate users per host; rudolfs (no auth or verify) and giftless (JWT setup) were not used (§18) |
| Front/service protocol (2026-10-15) | gRPC (tonic, prost; the `.proto` compiled with protox) over the volume's node-local Unix socket, same path and permissions, peer root or the service's euid checked on both ends; package `agentfs.history.v2`, protocol 2. Two HTTP/2 connections per front, calls and the listening stream (calls driven by the calling threads, measured 2026-10-15: no hop to a runtime thread). Each starts with `Hello`, which admits the front (version, volume, slot lease, identity) and binds it to the connection. `Record` answers only once durable and is never resent; a request goes out again on a new connection only when hyper proves it never left. Every call carries a deadline (`--history-timeout`). The service calls fronts on a bidirectional `Listen` stream. `grpc.health.v1` answers liveness. Mixed versions refuse each other at `Hello`. The owner: "i dont want to invent new protocols where they might already exist"; asked whether gRPC comes (a) in this change, (b) in its own later change or (c) not at all, and whether it lands on a PR or main: "1 a, 2 main" (2026-10-15) (§14.1) |
| Front round-trip limit (2026-10-15) | In the `e2e-multipod` jobs (passthrough `auto` and `off`), a front's single-thread record p50 may be at most 1.6 times the single-process one, judged on the median of the ratios of five runs of the `round_trip_cost` bench, never on one run (single runs measured 1.14 to 1.39); over it the job fails and prints every run's ratio (`ci/round-trip-bench.sh`). The limit was 1.35; judged this way the median measured 1.33 to 1.38 (CI runs 38826803578 and 38827325539, both legs), on the limit itself, and the owner raised it to 1.45. At 1.45 the medians on the same code across runners measured 1.34 to 1.46, and main's run 38828653054 failed at 1.458, so the owner raised it again to 1.6. The cost: the check catches only large slowdowns; the earlier extra hop to a runtime thread measured about 1.39 and would pass. The owner: "Keep the 1.35 limit but judge it on the median of several CI runs instead of a single run.", then: "Raise the limit to 1.45 on the median of 5 runs and land the check."; asked "Raise it to about 1.6. main turns green, but it only catches large slowdowns.": "(3)" (all 2026-10-15) (§14.1) |
| Split of the synchronous step | The front decides the rules, runs the record-time checks and freezes (it holds the descriptor); the service re-decides against the authoritative tracked set, checks the copies exist in the sender's slot, journals and replies. A `Tracked` query (no `fsync`) only for writes whose decision depends on tracking, so ignored files are never copied (§3.2, §14) |
| Pending slots | The high 16 bits of a pending id; slot 0 for the single process and the service, 1..=1023 leased by fronts with an `flock` on `meta/fronts/slot-N.lock`; the service's prune spares live slots (§2.2, §14) |
| Front failures | Service unreachable, hung (`--history-timeout`) or refusing: `EIO` at the boundary, `RecordFailed`, `HistoryServiceUnavailable` once per outage; repairs in the slot's file; a draft is never resent (§3.5, §14) |
| Committer of a front's records | The front's origin (namespace, pod, pod uid, node, replica) is journaled in the record, so replays keep it; the service renders the identity templates per origin and refuses a front whose identity does not render; a commit never mixes origins (§9, §14) |
| Patches with fronts | Applied by `serve-history` before it listens, only when no front lease is live; otherwise skipped with a message (§12, §14) |
| Restores with fronts | Served live by `serve-history`: every live front must listen (else busy, 75); fronts settle (a front that cannot is busy, 75; dry runs settle too), answer which paths are open for writing (in batches for the check, then per path just before each write), and drop their kernel caches of the paths the service wrote to the backing tree; at shutdown a restore stops before its next path and records what it wrote (§14.1, §15) |
| Freeze stamps (2026-10-13) | Each front write carries its backing file's pre-freeze stat stamp; the service, under the path claim, skips a write whose `ctime` is older than the last journaled at that path (5 min horizon) unless it is the file's current stamp (clock steps); same-`ctime` stamps are settled by the file; identical stamps from one slot are journaled (racy re-record) (§14.1) |
| Unorderable front changes (2026-10-13) | Identical stamps from two slots, a removal of a path present again, and a symlink whose target the tree no longer has are journaled, then resynced: the service records the path as the backing tree has it, as its own `sweep` record under the path claim (`agentfs.history_service.resyncs`). Chosen over comparing frozen copies' content, which cannot order two states the file no longer holds (§14.1) |
| Creating as the caller (2026-10-13) | A daemon with `CAP_SETUID`/`CAP_SETGID`/`CAP_CHOWN`/`CAP_FOWNER` creates as the caller by switching the worker thread's fsuid, fsgid and supplementary groups (read from `/proc/<pid>/status`) for the one creating call; refusals fall back to create-then-chown (`agentfs.fuse.create_as_caller_fallbacks`). The CSI node plugin runs with `hostPID: true` so callers' pids, and so their groups, are visible (§6, §14.1, §14.2) |
| Repair origins (2026-10-13) | A repair keeps the pod that failed to record it; the service trusts an origin a front names only on `sweep` records whose committer renders, and journals the front's own otherwise (§3.5, §14.1) |
| Protocol evolution (2026-10-13) | Additions are optional fields and new variants only, and the service lists what it supports in `Welcome.features` (`stamps`, `repair_origin`, `listen`, `still_last`, `move`); the version changes only with the transport (1 to 2 with gRPC, 2026-10-15) (§14.1) |
| Locks across fronts | POSIX and BSD locks are per FUSE mount, so not shared between pods; a lock-coordinated database (SQLite, the `fsync` class) must be written from one pod (§14) |
| CSI driver shape | One binary, `agentfs-csi`, in the agentfs image, run by one DaemonSet on every node beside csi-provisioner in `--node-deployment` mode: every node plugin serves the Identity, Controller and Node services, answers its node's topology (`topology.agentfs.mcurcio.github.io/node`), and provisions and deletes the worktrees on its own disk; classes use `WaitForFirstConsumer`, `attachRequired` is `false`, and there is no controller Deployment, attacher or `--mode` (superseded 2026-10-16 by F8: it was `--mode` controller, node or all, with a controller Deployment). The owner, asked whether a provisioner runs beside the node plugin on every node (F8 gate, Q3): "a" (2026-10-16, R8). tonic over a Unix socket, the spec compiled without `protoc`; the node plugin spawns the `agentfs` binary for the service, each front and the release scan instead of linking FUSE in-process (§14.2, §17.2) |
| CSI volumes | PersistentVolumes only (no inline ephemeral volumes); a worktree `<basePath>/<pv>/{backing,state,csi.json}` on the disk of the node that created it, for good; basePath is any local directory on each node (a hostPath, ideally a disk of its own), a network filesystem refused at start; size recorded, not enforced (§14.2). Superseded 2026-10-16 by F8: it was one basePath filesystem seen by the controller and every node. The owner (R1): "use local disk for working code and an optional backend storage_class (NFS or iscsi or other) for persisting the changes and storing branches and inboxes and such"; on where a node keeps volumes: “B seems like the most straightforward "kubernetes way"”, then "go" (2026-10-16) |
| One node per volume (2026-10-15) | A volume is used from one node, for good. Several pods on that node share it, each committing as itself: `ReadWriteOnce` (`SINGLE_NODE_MULTI_WRITER`) or `ReadWriteOncePod` (`SINGLE_NODE_SINGLE_WRITER`); `SINGLE_NODE_READER_ONLY` is a read-only mount. Every multi-node access mode is refused with one message naming this row, at CreateVolume, ValidateVolumeCapabilities, NodeStage and NodePublish, and a `csi.json` the driver does not understand is refused. A volume never moves: its worktree stays on its node, and continuing elsewhere is a new volume started from its branch (superseded 2026-10-16 by F8 "One branch per worktree", which replaced the "2a" answer below: a volume moved over a shared NFS basePath once detached). Pods on other nodes read its history from the remote it pushes to. The owner, asked whether sandboxes write to the directory or only read it, and whether they can run on Hermes's node: "Write, same node is fine". Asked whether volumes still move between nodes on shared NFS (2a) or stay on one node's disk (2b): "2a". On cross-node support he had said: "It is acceptable for the final answer to be that RWX is not supported in some or all cases. I want to do the diligence to know for sure, but if the complexity is not worth the value, then we can drop it" (all 2026-10-15) (§14.2, §19.2 item 2) |
| Mount options on shared storage (2026-10-15) | The state lock refuses a CLI or sidecar state area on NFS older than 4.1, mounted `nolock`, or with `local_lock` other than `none` (read from `/proc/self/mountinfo`), since the state lock is the only guard against a second writer there. `agentfs-csi` instead refuses a base path on any network filesystem (nfs, nfs4, cifs, smb3, smbfs, ceph, glusterfs, lustre, gpfs, afs), naming its type (superseded 2026-10-16 by F8 R1, "use local disk for working code": it applied the NFS 4.1 check to a shared basePath). Other shared filesystems are neither checked nor tested for state areas. A design choice, not an owner statement (§2.3, §14.2) |
| NFS silly renames (2026-10-15) | Silly-rename names (`.nfs` and 24 hex digits) are never recorded (§4) |
| Node plugin upgrades (2026-10-15, default, owner to confirm) | Drain a node: cordon, evict that node's agentfs pods, delete the plugin pod, wait until it is Ready, uncordon; the DaemonSet is `OnDelete`. A live FUSE hand-over is not built. Since F8 (2026-10-16) the evicted pods wait for their node (their PVs are pinned to it) instead of moving (§14.2, [csi.md](csi.md#9-operations)) |
| CSI parameters | A StorageClass or PV names a `profile` whose config fragment, rules and credentials live in the driver's namespace (a ConfigMap without hash suffix, a Secret), mounted into its pods; no Secret RBAC; pod authors choose only the class (§14.2) |
| CSI identity | `podInfoOnMount`: each pod's front commits under the pod's name, namespace and uid (§9, §14.2) |
| CSI recovery | Crashed fronts and services restarted with backoff (1 s to 30 s); a front in its predecessor's slot (`--history-slot`), its mount bind-stacked over the stale target, made shared so `HostToContainer` containers see it; reconcile from node records after a plugin restart; DaemonSet `OnDelete` (§14.2) |
| CSI RBAC | csi-provisioner's minimal ClusterRole (PVs, PVCs, StorageClasses, Events, CSINodes, Nodes), bound to the node plugin's account, whose token is projected into that sidecar alone; no Secrets, pods or snapshots; the `agentfs-csi` container holds no token. Superseded 2026-10-16 by F8 R8: it was csi-provisioner and csi-attacher in a controller Deployment with leader-election leases, and a node plugin with no API access. Each node's provisioner now holds PV and claim rights cluster-wide, the stated cost of R8 (§14.2, [csi.md](csi.md#10-security)) |
| CSI volume names | Every class's `volumeName` must contain `{pvName}` (`INVALID_ARGUMENT` otherwise; superseded 2026-10-16 by F8 rule 1: a template without it was the operator's choice, documented as unsafe), and there is no `branch` parameter or attribute: each claim is a worktree with its own branch, `agentfs/<volume>`, named once in its identity. The default is `{pvcNamespace}.{pvcName}.{pvName}`. A re-made claim is a new volume: the owner, on "re-made claim = new volume (a)": "Go" (2026-10-16, R3). A static PV's `volumeName` (or `volume`) is literal, its unknown attributes are refused, and it is adopted only over an existing worktree of that volume (§14.2, [csi.md](csi.md#5-parameters)) |
| CSI final push | `serve-history` pushes once more at shutdown, after its final commit, within `--push-grace`, as every long-running mode does (§11.2); NodeUnstage follows the last pod. After it, the node plugin's release scan pushes every unstaged worktree until a push exits 0 at its head, raising `PushFailed` at each failure, and DeleteVolume never pushes (superseded 2026-10-17 by F8 "Release decider": nothing else pushed an unstaged volume, and `onDelete: delete` refused DeleteVolume while a remote lacked commits) (§14.2, [csi.md](csi.md#9-operations)) |
| CSI Pod Security | The driver's namespace is `privileged` (the node plugin needs it, and runs with `hostPID: true` so its fronts can read callers' groups, §6); the controller's agentfs container has only `DAC_OVERRIDE` and `FOWNER`; workloads can run under `restricted` (§14.2) |
| CSI error codes | Unknown or invalid volumes are a success for DeleteVolume, ControllerUnpublish, NodeUnpublish and NodeUnstage (idempotent teardown the kubelet can always finish); NodeStage waits at most `--stage-wait` and answers `UNAVAILABLE` while the service starts; `SINGLE_NODE_READER_ONLY` is accepted as a read-only mount; checked by csi-sanity (§14.2) |
| Crash testing (2026-10-13) | Failpoints in the durable path (`agentfs_core::failpoints`), compiled only with the `failpoints` cargo feature (release builds have none; such a build says `+failpoints` in `--version` and warns when `AGENTFS_FAILPOINTS` is set), configured by `AGENTFS_FAILPOINTS` (`name=kill\|error\|panic\|sleep(ms)[@n]`); three harness tiers with one oracle, run in the `crash` job; power loss by `dm-log-writes` and `replay-log` in the `power-loss` workflow (§18 test 3) |
| Crash findings (2026-10-13) | Found by the harnesses and fixed: objects and `objects/` are synced before the ref names them; the journal keeps appending to the file a failed truncation renamed in; a record whose frozen copies are gone is dropped with `RecordFailed` instead of failing every later commit; a repair queue left in its temporary file is loaded; ref lock files a crash left are removed at open (§3.3, §3.5) |
| Bench off the landing path (2026-10-15) | A change is done when the `ci` and `image` workflows are green on its final commit; `bench` and `power-loss` run after it lands on main and are checked afterwards, and a bench budget warning or a power-loss failure is a follow-up, not a reason to hold the change (AGENTS.md). The bench runs each filesystem as two parallel jobs, `many` (create, mvdir and rm of 100,000 files) and `rest` (record_step, close4k, clone, npm, bigclose), and one report job merges them per filesystem; clone and npm run once instead of three times, bigclose keeps three. The owner, on the bench's 17 to 24 minute jobs: "23 minutes is too long for a CI job. Let's bring that time down"; offered options including sharding the bench (1) and agents not waiting on it (3), he chose "1 and 3" (both 2026-10-15) ([performance.md](performance.md#the-benchmark)) |
| Forked volumes (2026-10-15) | A fork is a volume started from another volume's latest local commit (its source, at its base). It records, pushes and recovers like any volume; when it finishes it publishes one change, a commit whose parent is base, and whatever receives the change does all the gating: agentfs never reviews, accepts or merges. The owner's ask: "I want to gate the acceptance of changes upstream, but still want sandbox agents to interact with the normal filesystem. So with agentfs, I can grant them a worktree forked from the latest head, they edit whatever files they choose, and then when the sandbox ends those edits become an upstream merge patch — either through a PR or some automated flow, configurable/programmable by an implementer." Its scope: "Design it, but leave Hermes out of it. We're building an open source project; my application of it to my cluster stack will come later. Focus on capabilities. Configuration with sensible defaults". Where a change lands, the source volume or another repository as a PR: "A or b. I'm not sure" (both are supported); part of a volume: "Partial would be via a subpath mount rather than a partial clone"; one squashed patch or the full history: "Configurable". On the frame "agentfs forks and publishes; whatever receives the change does the gating": "Go" (all 2026-10-15, in a Claude Code session). Built (§11.5), in the four units of "Fork scope" |
| Fork finish (2026-10-15) | A fork finishes when its claim is deleted (DeleteVolume of a volume from a fork class, whose reclaim policy must be Delete), and outside Kubernetes when `agentfs fork finish` runs. A restart of agentfs, the node plugin or the kubelet never finishes a fork; with a pod-scoped claim, a drain or rollout does. Asked when a sandbox is finished (a: its claim is deleted; b: its last pod stops; c: an explicit finish), the owner: "1a". Built (§11.5, §14.2) |
| Fork delivery (2026-10-15) | The CSI driver only publishes; a receiver lands a published change in its source with `agentfs patch submit`. The library and the CLI also deliver a change, by four built-in methods: inbox, branch, command and webhook. The owner: "2 a for CSI and b for the library". Built: the CSI side (§14.2), and the library's methods with `agentfs fork deliver` (§11.5 "Delivery", forked volumes unit 3) |
| Fork content (2026-10-15) | Everything agentfs itself does with a fork's content treats it as potentially hostile: names, paths, sizes, symlinks and gitlinks are checked, and nothing is run. agentfs adds no CI filtering: forks publish where users' own CI can act on them (§1.1). The owner: "3 potentially hostile, but don't limit CI capabilities that users could create". Built for create (base's tree through the confined writer), publish (create-only refs), a finished fork (read-only: no record, and a read-only CSI publish) and `fork list` (every published change checked against its finish commit, §11.5) |
| Fork remote (2026-10-16) | Forks publish to the source's own remote by default, with no setup (the remote its patch queue uses, §12: a design choice). An explicit fork-remote setting may name another remote, which is then never one of the source's push remotes, never sets its limits and never becomes its patch remote (a design choice, design rule 1). The stated cost: wherever that repository's CI may write, a sandbox's CI file can write the source's inbox, so the gate is only as strong as that repository's CI write permissions. The owner, answering the design gate's question on where forks publish: "b". Built (§11.5) |
| Fork hostile residual (2026-10-16) | Accepted and stated, with no hardening work now: a sandbox's file calls are served by the privileged node plugin, which holds every profile's credentials, so a bug in that front would expose node root and every profile's credentials; and volume size is not enforced on the node's base path (§14.2), so a sandbox can fill it for every volume on that node. The owner, answering the design gate's question on the hostile residual: "a" |
| Fork design rules (2026-10-16) | (1) A fork's push target is fixed at create: `agentfs/<fork>` on the fork remote, never the source's branch; every other setting is read from the source's settings files at each open, and settings that cannot be read stop the fork opening. (2) A fork finishes once, writing one finish record, and the finisher retries publishing until the fork remote holds the change; the fork is not deleted until it is published, or until an operator abandons it on record. (3) agentfs publishes and the receiver gates: a receiver lands the exact change commit it reviewed, and the source applies a given change at most once. Approved with the design gate's answers, with no notes (2026-10-16). Built (§11.5) |
| Fork scope (2026-10-16) | Four units, tracer first, docs with each: (1) the tracer: fork identity and fork remote; create, finish and publish; CSI cloning of a claim, a fork StorageClass with `fork: "true"` and reclaim policy Delete, and its admission check; DeleteVolume waiting until the fork is published; the source applying a given change at most once; (2) refused and abandoned forks, settings that fail closed, finished forks read-only, `fork status` and `fork list`; (3) library delivery: the delivery contract, its four methods, `fork deliver`; (4) hidden refs, the history-rewrite runbook covering forks, telemetry, the runbook for refused publishes. The owner, answering the design gate's question on scope: "a" (build units 1 to 4 as outlined). All four are built (§11.5) |
| Content sources (2026-10-16) | Reverses §14.2's "nothing clones": CreateVolume clones a claim's `dataSource` claim when the StorageClass is a fork class (`fork: "true"`), copying the source's history up to its latest commit and writing that commit's tree locally, with no credentials; since F8 (2026-10-16) only from a source on the same node. Any other content source, and every snapshot, stays refused. Follows from "Forked volumes" and "Fork scope". Built (§14.2). A start class (`startFrom`, F8, 2026-10-17, "Start points are branches") also fills a new worktree, from a pinned commit fetched once from a remote: CreateVolume's one fetch |
| Fork remote flag (2026-10-16) | The explicit fork-remote setting of "Fork remote" is `remote.<name>.agentfsForkRemote = true` on one remote section of the source's settings: that section is only for forks, never one of the source's push remotes, never its patch remote and never a source of its record-time limits; with none, forks push to the source's patch remote. Two flagged sections, or an invalid one, leave no fork remote, and forks are refused rather than fall back. A design choice (forked volumes unit 1), not the owner's words |
| Patch duplicates (2026-10-16) | The source applies a given change at most once ("Fork design rules" 3) by a rule for every producer, keyed by the patch commit's id: a queued patch commit that an earlier inbox number already applied is consumed with a marker commit and `PatchDuplicate`, never merged again (§12). Cost: a producer can no longer resubmit the identical commit to apply it again (it must make a new commit), and a replaced or replayed copy of a change has another id and is not caught. A design choice (forked volumes unit 1), not the owner's words |
| Fork onDelete (2026-10-16) | A fork class's `onDelete` defaults to `delete`: publishing is a fork's gate before deletion, so the release decider weighs a fork's state, not its remotes. A design choice (forked volumes unit 1), not the owner's words |
| Published (2026-10-16) | A fork is published when the finisher has seen its fork remote hold `refs/agentfs/<fork>/change` and `refs/agentfs/<fork>/finish` at exactly their ids after pushing them create-only; the fork's branch is pushed first, and a branch a forge moved is §11.3 divergence (saved and alarmed), which never blocks publishing. A design choice (forked volumes unit 1), not the owner's words |
| Refused publish (2026-10-16) | One function, `fork::finish::refused_reason`, decides what is permanent: another writer's create-only ref (`another_writer`), the create-only push refused by the remote (`rejected`), one commit over `agentfsMaxPushSize` or one LFS object over `agentfsMaxLfsObjectSize` (`too_large`), and a finish naming a remote that is no longer the fork's fork remote (`remote_gone`); everything else stays pending and is retried. A refusal is recorded in `meta/fork.json`, raises `ForkRefused` once, is never retried by the node plugin, and is cleared by `agentfs fork finish --retry`. Cost: a branch push refused by a hook stays pending, since a volume's own pushes treat it so (§11.3). A design choice (forked volumes unit 2), not the owner's words |
| Abandon (2026-10-16) | `agentfs fork abandon --reason TEXT`, under the fork's state lock, records the reason, time, uid and state in `meta/fork.json`, in any state but published; then no finisher pushes the fork (each attempt reads the mark first) and DeleteVolume applies `onDelete`. Not covered: a push already running finishes (libgit2 cannot cancel it), and a fork mounted again pushes its branch on its interval as any volume does. The node plugin's finisher runs `--timeout 300s`, so the lock is free between scans for an operator's `--wait-lock`. A design choice (forked volumes unit 2, "Fork design rules" 2), not the owner's words |
| Finished forks read-only (2026-10-16) | A fork whose branch head is its finish commit takes no record (`ForkFinished`, in the recorder itself); the CLI's changing commands refuse it before they start, while a CSI front of it FUSE-mounts it read-only (`ro`) and is bind-mounted read-only. The kind e2e found the read-only bind alone insufficient: the container still wrote, its runtime binding the target read-write. A design choice (forked volumes unit 2, "Fork content"), not the owner's words |
| Fork remote LFS (2026-10-16) | A flagged fork remote must name its own `lfsurl` or `lfspushurl` while `lfs.url` or `lfs.pushurl` is set, and reads them before the global keys, so a fork's LFS objects never reach the source's store; without one the section is invalid and forks are refused. A design choice (forked volumes unit 2), not the owner's words |
| Submit's early refusal (2026-10-16) | `agentfs patch submit` of a given commit that takes a new number is refused (`AlreadySubmitted`) when the remote already queues or has applied that commit; `--replace N` and `--skip N` are not checked, since they name their number deliberately and refuse applied ones already. The queue's duplicate rule ("Patch duplicates") stays the guarantee, since two submits at once both pass the check. A design choice (forked volumes unit 2), not the owner's words |
| Delivery payload v1 (2026-10-16) | The command and webhook methods send one JSON document, schema 1: `schema`, `fork`, `source`, `base`, `change`, `finish` and `fetch` (`remote` as the runner named it, and the `change`, `finish` and `history` refs). `change` is the key receivers dedupe on, and the webhook's `Idempotency-Key`. A new field or meaning is a new schema number, never a silent change. Both methods are at least once; the inbox method is at most once by the §12 duplicate rule. A design choice (forked volumes unit 3), not the owner's words |
| Branch delivery idempotency (2026-10-16) | The branch method decides "already delivered" by content: its squash commit is deterministic (the change commit's author and committer, the message `change of fork <fork>` with `Agentfs-Change: <change>`), so the target ref at exactly the commit it would create is already delivered, and a ref at any other commit is refused and never moved; a trailer is never trusted, since anyone who may write the target can forge one. It pushes create-only (`--force-with-lease=<ref>:`). Cost: with `--onto`, a branch that moved since the first delivery makes a repeat refused rather than already delivered, and a receiver that rewrote the branch (a rebase in its pull request) gets the same refusal. A design choice (forked volumes unit 3), not the owner's words |
| Hidden forks (2026-10-16) | A fork's push target is `branch` (`refs/heads/agentfs/<fork>`, the default) or `hidden` (`refs/agentfs/<fork>/history`, never under `refs/heads/`); new forks get the source's `agentfs.forkRefs`, and the fork keeps it in its identity (`agentfs.fork.pushTarget`, written only for `hidden`), so changing the setting moves no fork that exists. Any other value of either key is refused (`BadChoice`), and an include may not set `pushTarget`. It is unit 4 of "Fork scope". Cost: whether a forge runs CI or webhooks on pushes to `refs/agentfs/*` is not verified (§19.2), and an operator who switches to `hidden` to contain exposure must abandon or finish the forks that already push branches. A design choice (forked volumes unit 4), not the owner's words |
| Fork telemetry (2026-10-16) | The finisher reports through §13's existing signals: spans `agentfs.fork.finish` (once, when the finish commit is written) and `agentfs.fork.publish` (one publish call, its outcome and refusal reason, with the pushes inside it), the histogram `agentfs.fork.publish.duration` (seconds from the finish commit to published, one point per fork, written with the published mark), and the events `ForkFinished` and `ForkPublished`, which are not alarms. A fork's lifecycle events (finished, published, abandoned) become INFO log records beside the alarms (`Event::is_logged`), so a fork's life is in the same log as its refusal. Cost: a duration measured from the finish commit's time trusts the node's clock, and a fork retried after a refusal counts the time it waited for the operator. A design choice (forked volumes unit 4), not the owner's words |
| Forks in history rewrites (2026-10-16) | The history-rewrite runbook covers forks by reachability, not a list: the affected refs are those containing the first commit that recorded the content, in mirror clones of the source's remotes and fork remotes; only forks whose base contains it are abandoned, before the rewrite, and their refs deleted with a lease; forks based earlier keep their ids and publish as before. What agentfs cannot enumerate (delivery targets, receivers' clones, CI caches, retained fork directories) is listed for the operator. Cost: a change based after the bad commit and not yet landed becomes unlandable (`parent_not_in_history`). A design choice (forked volumes unit 4), not the owner's words |
| Alert rules (2026-10-16) | Milestone F11 ships example alert rules and runbooks, without the Hermes end-to-end test it first planned. Asked whether to (a) drop that test and keep the alert rules and runbooks or (b) keep it, given "Design it, but leave Hermes out of it. We're building an open source project" (2026-10-15), the owner answered "3 a" (2026-10-16, in a Claude Code session). The rules are one Prometheus rules file, `deploy/alerts/agentfs.rules.yml`, with the `PrometheusRule` generated from it and checked in CI, so they have one home; they use the names of Prometheus's default OTLP translation, and each counter rule matches a counter's first sample (§13). The Helm option to install them waits for the chart (F9). Cost: another translation strategy, or a collector that renames metrics, needs the rules edited; the `PrometheusRule` is not applied to a Prometheus Operator in CI. The file layout, names and thresholds are design choices, not the owner's words |
| Alert volume label (2026-10-16) | The example rules join on `job`, `instance` and `agentfs_volume`, and deployment.md requires promoting `agentfs.volume` under the CSI driver, where every volume on a node reports the node plugin's pod as `instance` (found in the review of F9 to F11). Chosen over a distinct `service.instance.id` per history service, which would change what `instance` means (the pod) for every consumer. Cost: a Prometheus that does not promote it merges a node's volumes' series, which no rule can detect; a CLI command run in the sidecar shares the daemon's series for its run. A design choice, not the owner's words |
| Docker (2026-10-16) | Docker support is examples of the container pattern (§17.1 "Docker"): `docker run` and Compose, one agentfs container per volume with `/dev/fuse`, `CAP_SYS_ADMIN` and AppArmor unconfined rather than `--privileged`, a host directory on a shared mount, and workloads bound to it read-only and `rslave` with writable submounts; each example runs in the `docker` workflow. A Docker managed volume plugin was evaluated and not built: it needs its own long-running service for Docker's volume plugin API, the per-volume daemon supervision the CSI node plugin already has (duplicated, or factored out of `agentfs-csi` first), and a `docker plugin push` release path, and one plugin process would hold every volume's mount ([docker.md](docker.md#7-why-there-is-no-docker-volume-plugin)). Cost: no `docker volume create -d agentfs`, and no Docker Desktop, which cannot propagate mounts to the host. Volumes with no remote use what agentfs allows today (no `[remote]` section); a declared local-only mode is F8's. The ask, from the milestone brief: "examples of how to use agentfs as a docker volume", with "a Docker volume plugin ... implement it if it is a reasonable size, otherwise document why not". The size judgement is a design choice, not the owner's words |
| Helm charts (2026-10-16) | Milestone F9's brief asked for Helm support. `charts/agentfs-csi` (application) installs the driver and `charts/agentfs-sidecar` (library) gives a workload's own chart the phase 1 sidecar's containers, volumes and objects. `deploy/` stays the kustomize source; CI checks that the charts render the same objects, ignoring only Helm's labels, `checksum/` annotations, the release Namespace and kustomize's hash suffix. The driver is one per cluster with fixed object names; the chart does not create its namespace. Defaults: no profile and no StorageClass, credentials from `existingSecret` or an empty chart Secret, StorageClass `parameters` passed through as written, and the sidecar library refuses to render without an image tag or digest; `alerts.enabled` (off by default) installs `deploy/alerts`' rules as a PrometheusRule, read from that one file. Published to `oci://ghcr.io/mcurcio/charts` from main as `<version>-main.<run>` and from `vX.Y.Z` tags as `X.Y.Z`, the CSI chart's appVersion the commit. Design choices, not the owner's words (docs/helm.md) |
| One branch per worktree (2026-10-16) | A worktree (a volume's files and its branch on one node; pods mount and may share one) writes only its own branch `agentfs/<volume>`, for good: it creates the branch or fast-forwards it, decided from the remote's old value in the push's own negotiation (`push::claim`, F8 rule 2), and a branch another writer moved is saved and alarmed as a divergence, never overwritten (§11.3). Continuing elsewhere is a new volume started from the old branch (a start class, §14.2); which branch is current is the operator's choice, never fenced. Proposed after six design stops and accepted: the owner, on "one branch per worktree, for good" (moving = a new volume started from the old branch; operator picks which branch is current): "Go" (2026-10-16, R2) |
| Remotes on any storage (2026-10-16) | Worktrees are node-local, and what outlives a node is an ordinary git remote, which may live on a claim of any StorageClass, NFS 4.0 and iSCSI included. The owner: "my NFS server doesnt support 4.1 so I need a solution that works with NFS 4 or iscsi" (2026-10-16), and on sandboxes "Write, same node is fine" (2026-10-15) (R6). A fork is made on its source's node (§14.2) |
| Start-branch alarm (2026-10-16) | A started volume lists its start branch at every push to its start remote and raises `StartBranchMoved` while it holds commits the volume has not taken in or accepted (`push::start_branch_moved`); the new volume raises it, never the old node, so an old node's late commits, pushed to the old branch by its release scan after a move, are saved and alarmed, never overwritten (§11.3). The owner chose the option whose text said the old node's "unpushed changes saved and alarmed, never overwritten": "a" (F8 reframe Q1), and later "go" on the alarm "raised by the new volume when its start branch moves past the pinned start commit" (both 2026-10-16, R9) |
| The chart's git server (2026-10-16) | `charts/agentfs-csi` has an optional `server:` section, off by default: one pod of a StatefulSet running the minimal git server image `ghcr.io/mcurcio/agentfs-git-server` (`examples/docker/git-server`: nginx, basic auth, `git http-backend`; non-root, read-only root filesystem), its repositories on a claim of any StorageClass, behind the Service `agentfs-git-server`. Its credentials are wired once: one Secret holds the server's `htpasswd` and the profiles' `token`, which the node plugin mounts as credentials key `agentfs-server`; the chart makes it with a random password kept across upgrades, or takes `server.credentials.existingSecret`. No fencing, per-volume keys, launch commits or hooks; it is left out of `deploy/`. The owner, asked for a reference server, Forgejo or lighter: "Something lighter. This is a minimal fit remote. Pull Requests and the like can be achieved by configuring agentfs to include GitHub (or similar) as a remote. The point of this core server is for kubernetes-native replication"; on where it lives: "Ideally it's optional config in the one helm chart"; and on fencing, choosing the smallest F8 with no fencing server: "A and jj" (all 2026-10-16, R5). The StatefulSet, the Secret's shape and leaving it out of `deploy/` are design choices (F8 S4), not the owner's words ([helm.md](helm.md#the-git-server)) |
| Release decider (2026-10-17) | One pure decider, `release::decide`, answers DeleteVolume: delete (every declared remote holds the head, or no history, and `onDelete: delete`), keep (`onDelete: retain` once every remote holds it, or a profile with no remote declared) or wait (`UNAVAILABLE`: a remote lacks the head, cannot be listed or diverged; every remote section invalid; a fork not published). Its facts are gathered by DeleteVolume, one listing per remote (`RemotePusher::branch_claim`), never the push record. Every class of the driver has `reclaimPolicy: Delete`, enforced by the chart's admission policy `agentfs-class` (widened from fork classes), so DeleteVolume always runs; DeleteVolume never pushes; the node plugin's release scan (the fork publisher, generalised) pushes every unstaged worktree until held, raising `PushFailed` at each failure; CreateVolume does not refuse `onDelete: delete` on a profile without remotes (the decider keeps such a worktree). A design choice (F8 S1, "D1"), not the owner's words |
| Accepting a start-branch tip (2026-10-17) | The start-branch alarm is cleared by `agentfs start accept --tip <oid>` on the new volume: it fetches the start branch, checks `<oid>` is on it and creates `refs/agentfs/<volume>/accepted/<oid>` (never moved, pushed create-only with the branch); the alarm then stops for every commit the tip reaches and comes back if the old branch moves again. "Deleting the old branch clears it" was dropped. A design choice (F8 S1, "D2"), not the owner's words |
| Start points are branches (2026-10-17) | A `startFrom` names a branch, never a `refs/...` ref: a `diverged/*` ref is refused at parse (`BadStartPoint::NotABranch`, CreateVolume `INVALID_ARGUMENT`), since it never moves and nothing could alarm on it. The release decider answers wait for a diverged remote, so a diverged worktree is never removed and its release-scan pushes keep alarming. A design choice (F8 S1, "D3"), not the owner's words |

### 19.2 Open questions

1. **Performance budget** for the synchronous step, set after the phase 1
   benchmark (§18 test 13). *Decided (2026-10-13)*: an advisory budget
   from the first CI measurements, in §19.1 and
   [performance.md](performance.md#performance-budget).
2. **Multi-pod (§14):** RBAC and Pod Security are settled by the CSI
   driver (§14.2). *Closed (2026-10-15)*: several nodes writing one
   volume is not supported (§19.1, "One node per volume"). Attach is not
   a fence on most storage backends; a lock on shared storage can elect a
   holder but cannot fence a paused or cut-off one (Kleppmann, "How to do
   distributed locking"; client-go's `leaderelection` says it gives no
   fencing); the redesign that would fence in the storage did not
   converge; and no consumer needs it (the owner:
   "Write, same node is fine"). *Closed further (2026-10-16, F8)*: a
   volume does not move between nodes either. Each worktree stays on its
   node's disk with its own branch, for good, and continuing elsewhere is
   a new volume started from the old branch (§19.1, "One branch per
   worktree"), so no fence is needed: two worktrees never share a branch,
   and the shared basePath and its forced-detach residual are gone.
   Upgrading the node plugin drains the node (§14.2). The round
   trip per boundary over gRPC is measured on one node (§14.1, CI run
   38819854746, commit 2526397, `ubuntu-24.04`, ext4): the
   `grpc.health.v1` round trip is 148 µs (passthrough `auto` job) and
   90 µs (`off`) at the median, and a front's close is 1234 against
   996 µs (`auto`) and 1253 against 961 µs (`off`) for a single-process
   close at the median.
3. **Recovery tooling** beyond `restore`, `revert` and the runbooks (§15):
   whether a read-only history view for operators is worth building,
   whether undo through the patch queue (§12) replaces manual restores for
   iac. (Asking a running daemon to push now is built: `agentfs push`
   goes through the control socket, §2.3.)
4. **The FLUSH+RELEASE double record** (§3.1, §3.2). *Decided
   (2026-10-13)*: a racy record keeps its frozen copy open, and a
   boundary that would record only because of the racy rule compares the
   file with it, skipping the record when the bytes are equal and the
   copy is still the last state journaled at the path (asked of the
   history service by a front, `StillLast`). The benchmark now bounds
   the count at one record per boundary
   ([performance.md](performance.md#known-costs)); measured on CI,
   100,000 new files made 100,004 to 100,009 records where they made
   about 200,000.
5. **Residuals of multi-front ordering and live restores** (§6, §14.1,
   §15), known and accepted for now (2026-10-13):
   - *Same-tick writes on a coarse clock.* Two fronts' same-size writes
     within one `ctime` tick are resynced, so HEAD ends on the tree's
     state; the commits in between may hold the other pod's state for a
     moment, and the resync commit carries the service's committer, not
     the pod's that wrote last (the service cannot tell which did).
   - *Callers' groups at pid 0.* Without `hostPID` on the CSI node plugin
     (or any FUSE daemon in another pid namespace than its callers), a
     caller's supplementary groups are unknown: creations that only a
     supplementary group (a pod's `fsGroup`) allows fall back to the
     daemon, a racing open from another front can fail with `EACCES`,
     and `create`'s check of an existing file counts the primary group
     only. The shipped manifest sets `hostPID: true`; the fallbacks are
     counted.
   - *Clock steps.* A write that is the file's current state is never
     superseded, so a backward step of the wall clock no longer drops the
     last state; a late record frozen before the step and racing writes
     after it are still ordered by `ctime`, which the step makes
     arbitrary for up to the step's length.
   - *Live restores.* A pod that opens a file for writing between the
     service's per-path question and the write itself is not seen (as in
     mount mode); a restore stopped at shutdown leaves the one path it was
     writing, if that outlasts `--shutdown-grace`, written and
     unrecorded.
6. **Power-loss coverage** (§18 test 3). The `power-loss` workflow
   replays `dm-log-writes` logs in write order and checks at every FUA
   (or every flush, on demand): a disk may also reorder the writes
   between two flushes, which in-order replay never shows. The commit
   path's fsync order (§3.3) does not rely on order within an epoch, but
   only reviews say so: replaying random subsets of each epoch's writes
   is open, as is a power cut through a FUSE mount rather than the CLI's
   `record`.
7. **Reloading OTLP TLS files** (§13). The collector CA bundle, client
   certificate and key are read once, when the exporter is built, so
   rotating one (a cert-manager renewal, say) takes a restart of the
   daemon. Open: watching the files and rebuilding the exporter, or
   resolving them per connection, as the git credentials are re-read
   when they change (§11.1).
8. **Cloning a claim in use** (§19.1 "Content sources"). Kubernetes'
   documentation says a clone's source claim "must be bound and available
   (not in use)" (checked 2026-10-15). Answered 2026-10-16: the
   external-provisioner (v6.4.0, the version deploy/csi pins) checks only
   that the source claim is bound, not being deleted, of the same driver
   and no larger than the new claim (its `getPVCSource`; no in-use check,
   and no same-class rule), and on kind (Kubernetes 1.37) a sandbox's
   ephemeral claim on a fork class cloned a claim a running pod writes to
   (`ci/e2e-csi-forks.sh`, step 1, image workflow run 38871457716). Open:
   whether a later provisioner or Kubernetes release adds the check the
   documentation describes; the kind job would show it.
9. **Forge CI on hidden refs** (§19.1 "Hidden forks"). Whether a forge
   (GitHub, Gitea, GitLab) runs workflows or sends push webhooks for a
   push to `refs/agentfs/<fork>/history`, outside `refs/heads/`, is not
   verified; a bare repository runs only its own hooks. Open: check each
   forge an operator relies on before choosing `hidden` to keep a
   sandbox's output away from its CI.

## 20. Prior art

| Project | What it is | Why not use it |
|---|---|---|
| gitfs (Presslabs; fork `vtemian/gitfs`) | Python FUSE filesystem over git that commits after idle | Commits after a 5 s idle period; merges while running; history visible in the mount; no LFS. Last commits 2021 and 2025-08-11. Design reference |
| Cloudflare ArtifactFS | FUSE client for Cloudflare Artifacts | Commits only on `git commit`; single-user mount (`allow_root` impossible through jacobsa/fuse); built for large lazily loaded repositories. PR 197 in iac stalled on it |
| JuiceFS CSI driver | FUSE filesystem with a CSI driver and mount pods | Reference for §14's recovery problems |
| Hermes `checkpoint_manager` | Shadow git store outside the project, snapshot before each turn | Evidence that a git directory outside the tree works for undo; snapshots per turn, not per change |
| Mesa | Hosted versioned filesystem for agents | Hosted private beta |
| Coregit, Freestyle | Hosted git APIs for agents | APIs, not mounts |
| lakeFS | Branches and commits over object storage | Explicit commits, not git |
