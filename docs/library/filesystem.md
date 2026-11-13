# The filesystem: `agentfs-fuse` and `agentfs`

The filesystem layer turns file calls into records. It is a FUSE filesystem
over a real backing directory. Every change a workload makes through the
mount is recorded, through [`agentfs-core`](core.md), before the call
returns, or the call fails.

There are three ways to integrate at this layer:

1. **Run the `agentfs` binary.** It is the supported way. A workload uses
   the mount as a normal directory. You drive the binary with commands,
   flags, environment variables, exit codes and a control socket.
2. **Link `agentfs-fuse`.** It is a library: a kernel-independent set of
   operations (`Ops`), a thin adapter to `fuser`, and the mount and unmount
   code. Use it to serve a different mount, or to test file behaviour
   against a real directory without `/dev/fuse`.
3. **Talk to a running daemon** over its control socket, from a script or
   another program.

The reference for every flag and variable is
[configuration](../configuration.md). Procedures for operators are in the
[operator guide](../operator-guide.md). What the layer promises is in
[guarantees](../guarantees.md).

## The mount contract

A mount shows the backing tree. A workload reads and writes it as it would
any directory. The mount adds recording and nothing else a workload can see:
no history, journal, state or credential is reachable through it.

- **Three separate directories.** The state area, the backing tree and the
  mount point must be three separate directories, none inside another, after
  symlinks are resolved. A state area inside the backing tree would be
  visible through the mount. A directory under the mount point would be
  reached through the mount by the daemon itself.
- **Mount options.** `default_permissions` (the kernel checks modes and
  ownership), `allow_other` (unless `--no-allow-other`), `nodev`, `nosuid`,
  `rw`, filesystem name `agentfs:<volume>`, subtype `agentfs`. There is no
  `auto_unmount`: a crashed daemon leaves `ENOTCONN` at the mount point
  rather than exposing the bare directory beneath it to writes nothing would
  record.
- **Negotiated.** `FUSE_ATOMIC_O_TRUNC` and `FUSE_PARALLEL_DIROPS`. Never the
  writeback cache (a flush must see every write the daemon serves), never
  POSIX or BSD locks (they stay in the kernel, which is right for one mount),
  never POSIX ACLs (only mode bits are enforced).
- **Caching.** Entries are cached for 1 s. Attributes are not cached.
- **Threads.** `--threads`; the default is the CPU count, between 2 and 16,
  with a cloned `/dev/fuse` descriptor per thread.
- **Executing files** works through the mount. The kernel's exec flag is
  dropped before the daemon opens the file.

### What is recorded

A change boundary is the point where a change becomes a record. The call
returns after the record is durable, except for the late backstops.

| Event | Recorded | Before the call returns |
|---|---|---|
| `close` of a file that changed since its last record | Content and executable bit | Yes |
| `rename` | Source removed, destination added. A directory is one move, reconciled before commit | Yes |
| `unlink`, `rmdir` | Removal | Yes |
| `link`, `symlink` | The new path | Yes |
| `truncate`, or a change of the executable bit | New content or mode | Yes |
| `mknod` of a regular file | Its (empty) content | Yes |
| `fsync` on an fsync-class path | The whole group, at most once per `agentfs.coalesce` | Yes |
| The trailing timer of an fsync-class group | The whole group | No: at most one `agentfs.coalesce` late |
| Release of the last handle that could write, if changed since the last record | Content | No |
| The periodic sweep (`agentfs.sweepInterval`, default 30 s) | Files still open and changed | No |

`mkdir`, `chown`, `utimens`, `chmod` of other bits and extended attributes
pass through to the backing tree and are not recorded. Empty directories are
not recorded, because git cannot represent them. FIFOs, sockets and devices
are never recorded. What a record contains, and what is left out, is in
[recording](../recording.md).

### How dirty detection works

The daemon does not see every write. Writes through a shared mapping reach it
only at writeback, and a writer that shares a passthrough backing id not at
all. So "changed" is read from the inode.

- An inode is *armed* by a write-open, a write the daemon serves, or a size
  change.
- A flush does an `fstat` of armed inodes only, and records when the stamp
  (device, inode, size, mtime, ctime, mode) differs from the one taken just
  before the last freeze. A close of a file that did not change records
  nothing, so a build that writes a log does not record it thousands of times.
- Timestamps come from a coarse clock, so git's racy rule applies. If the
  last record's stamp is within twice the clock's resolution of its record
  time, the next boundary looks again. It compares the file's bytes with the
  frozen copy and records nothing if they are equal. Any doubt records.
- A created file is recorded at its first flush even when empty (`touch`).
  Opening an existing file for writing and closing it without a change
  records nothing.
- A shell's `>` is two records: the shell truncates and closes the file
  before the command writes, then the command's last close records the
  content.

The frozen content is the handle's own descriptor's inode, whatever renames
happened. Other boundaries open the path with `openat2` and do the same.

### Renames

A file rename records the source's removal and the destination's content in
one record. A directory rename is journaled as one *move*, under the mount's
name lock, so no other boundary can name a path beneath either directory
until it is durable. It takes constant time in the number of files.

Each move leaves a *reconcile duty*. Before each commit pass the commit loop
walks the destination in the backing tree and records what recording every
file there would have decided differently: files the rules exclude at their
new path are removed, files that were not tracked and that the rules record
are added, and tracked paths no longer in the tree are removed. The walk
opens every directory with `openat2` beneath the root and never follows a
symlink or opens a FIFO or device. A duty that fails is retried with a backoff
and, after repeated failure, alarms and stops holding commits back
([reconcile-failed runbook](../runbooks/reconcile-failed.md)).

`RENAME_EXCHANGE` of directories, and a rename to or from a path the mount
never records, record both names' removal and then every regular file and
symlink under the destination, in records of at most 4096 changes.

### Kernel passthrough

`--passthrough auto` (the default) asks for `FUSE_PASSTHROUGH` when the
kernel offers it and the daemon may register backing files (`CAP_SYS_ADMIN`
in the initial user namespace: root, not root inside a rootless container).
`on` refuses to mount without it. `off` never asks. The start line reports
the result and why.

- Only a handle that cannot write gets passthrough. A mapping of a
  passthrough file holds the backing file, so the release can come while the
  mapping is live and nothing would see a write through it afterwards.
- The kernel refuses to mix two kinds of open on one inode, so a writer
  opened on an inode that already has a backing id shares it. That inode is
  watched until the kernel forgets it, and is recorded once more then.
- Files in an fsync-class group never use passthrough.
- If registering a backing file fails for one inode (a stacked backing
  filesystem, `EMFILE`), only that inode is served through the daemon while
  it has a handle open. With `on`, a later failure fails that open with `EIO`
  and stops the mount.
- A descriptor opened for writing under passthrough before a daemon crash
  keeps writing to the backing file, unrecorded, until closed. Use
  `--passthrough off` where every write must be recorded.

### Permissions and ownership

The kernel checks the caller's access against the stored ownership and mode.
The daemon then acts with its own privileges, except when creating.

- With `CAP_SETUID`, `CAP_SETGID`, `CAP_CHOWN` and `CAP_FOWNER` (decided
  from the effective capability set, not the uid), the daemon creates every
  new file, directory, symlink and FIFO **as the caller**. For the one
  creating system call, the worker thread switches its filesystem uid, gid
  and supplementary groups to the caller's (`creds`). The object is the
  caller's from the instant it exists. A creation refused as the caller (a
  device node needs `CAP_MKNOD`) is made as the daemon and then given to the
  caller, with the exact mode the caller asked for.
- Without those capabilities, everything the daemon creates belongs to its
  own user.
- Owner and mode are set on a descriptor opened without following and checked
  to be the object just made, never by name. A name swapped for a symlink or
  hard link meanwhile is refused with `ESTALE`.
- FUSE reports pid 0 for a caller outside the daemon's pid namespace. The
  daemon cannot read that caller's supplementary groups. A creation that only
  a supplementary group allows is then made as the daemon and given to the
  caller. `agentfs.fuse.create_as_caller_fallbacks` counts these.
- `st_ino` is the daemon's node id: one per backing inode, shared by hard
  links, kept for the life of the mount, but not stable across remounts.
- **Confinement.** The daemon reaches the backing tree only through a
  descriptor opened at mount and `openat2` with `RESOLVE_BENEATH`,
  `RESOLVE_NO_SYMLINKS` and `RESOLVE_NO_MAGICLINKS` (Linux 5.6 or later). It
  checks that every path still leads to the node's backing inode (`ESTALE`
  otherwise). A symlink in a path is never followed. Path resolution is
  ordered against renames, so an ancestor rename never makes a `stat` of an
  existing file fail.

Ownership, other permission bits, timestamps and extended attributes are kept
by the backing filesystem and are not recorded.

### fsync-class files

Paths with `agentfs.commit=fsync` (databases and their `-wal` and `-journal`
files) are recorded as whole groups. No boundary records one member alone.
Each change to a member is held against captures. The group is captured as one
atomic record at `fsync` (rate limited), by the trailing timer, at the close
and release of the group's last handle, and at unmount. `-shm` files are never
recorded. While a restore or revert through the mount changes a group, only
the operation records it, whole, at its end (`Ops::capture_operation`). See
[recording](../recording.md).

## The library: `agentfs-fuse`

| Module | Offers |
|---|---|
| `ops` | `Ops`: every filesystem operation over the backing tree, independent of the kernel. The inode table, dirty detection, change boundaries and the repair queue |
| `fuse` | `FuseFs`, the `fuser::Filesystem` adapter over any `Operations`, and `Passthrough` and `PassthroughStatus` |
| `session` | The `Operations` trait that `fuse.rs` calls, `TreeOperations`, `Actor` (who is calling), `NameCheck` |
| `overlay` | `Overlay`, the sandbox overlay's session, and `ViewOwner` |
| `mount` | `preflight`, `Mount::start`, `Mount::shutdown`, `mount_state`, `detach`, `MountConfig` |
| `groups` | The fsync-class gate: holding writes while a group is captured, and each group's schedule |
| `sweep` | `Sweeper` (the periodic backstop and repair retries) and `Capturer` (the fsync-class timer) |
| `invalidate` | Kernel cache coherence across a daemon's sessions |
| `backing` | Confined access to the backing tree |
| `access`, `creds`, `privileges` | Mode-bit checks, creating as the caller, what the daemon may do |
| `tree_view` | The FUSE view of the tree library (below) |

`Ops` is where behaviour lives, and it has no kernel dependency, so tests run
against a real directory and state area. Each operation returns
`OpResult<T>`, a `Result<T, Errno>`:

```rust
use std::sync::Arc;
use agentfs_fuse::backing::Backing;
use agentfs_fuse::ops::{Ops, OpsConfig};

// `recorder` is an agentfs_core::volume::Recorder (local or a front's).
let backing = Backing::open(std::path::Path::new("/srv/tree"))?;
let ops = Arc::new(Ops::new(backing, recorder, OpsConfig::default())?);

// Operations take FUSE node ids; the root is inodes::ROOT.
let entry = ops.lookup(agentfs_fuse::inodes::ROOT, b"notes.txt")?;
```

Node ids and handle ids are `Ops`'s own. The caller of a write-style
operation is an `Actor` (`Actor::of_process(caller)` for an ordinary request).

### Mounting

Run `preflight` first. It refuses:

- a state area, backing tree and mount point that are not three separate
  directories;
- any remote section's `agentfsCredentials`, `agentfsSshKey`,
  `agentfsSshKeyPassphrase`, `agentfsSshAgent`, `agentfsKnownHosts` or
  `agentfsLfsCredentials` file under the backing tree or the mount point,
  invalid sections included. This is an error and not an alarm: carrying on
  would hand the secret to the workload, or let it rewrite `known_hosts` to
  trust any host;
- an `include.path` fragment under the backing tree or the mount point;
- a stale mount point (`ENOTCONN`), with the command that clears it.

```rust
use std::time::Duration;
use agentfs_fuse::{Mount, MountConfig, MountKind, Passthrough, preflight};

let (backing, mountpoint) =
    preflight(&state, &settings, &backing, &mountpoint, MountKind::Main)?;

let mount = Mount::start(
    recorder,
    MountConfig {
        volume: "demo".into(),
        backing,
        mountpoint,
        passthrough: Passthrough::Auto,
        allow_other: true,
        threads: 0,                       // CPU count, between 2 and 16
        sweep_interval: Duration::from_secs(30),
        repair_store: Some(state.repairs()),
        stack_over: None,
        read_only: false,
        overlay: None,
        unmount_grace: Duration::from_secs(10),
    },
)?;

// ... serve until a signal, or until `mount.ended()` is set ...
let report = mount.shutdown();
```

`Mount::start` mounts with fuser, repairs the paths a previous mount could not
record (`MountConfig::repair_store`) before it serves any request, and starts
the sweep. It returns once the kernel has finished INIT. It refuses
`--passthrough on` without the privilege.

`Mount::shutdown` unmounts (lazily if the mount is busy; the overlay first),
waits up to `unmount_grace` for the sessions to end, stops the sweep and runs a
final one over every file still open and the repair queue, captures every
changed fsync-class group, and wipes the overlay's tmp layer. The
`UnmountReport` says what it did: whether the unmount was lazy, the paths it
could not repair (and where they are saved), and the count of swept and
captured items. Committing and pushing the last records is the caller's: call
`Service::shutdown` from the core layer after it.

`mount_state(path)` tells `MountState::Live`, `Stale` (the daemon is gone),
`NotMounted` or `Foreign` from `/proc/self/mountinfo` and a `statfs`. Nothing
else is read: no state area, no lock. `detach(path)` clears a stale mount.

### Errors

`MountError` covers a missing backing tree, a failed mount, a missing
`/dev/fuse`, `--passthrough on` without the privilege, a stacking failure,
an overlay refused or failed, and I/O. `PreflightError` names each layout
refusal. A file operation never panics a request: it answers an errno.

### Recording failures

When a record fails (the state area is full, the history service is down)
the operation returns `EIO`. A flush keeps the inode marked as changed, so
the next boundary records it. A namespace operation has already changed the
backing tree, so its paths go on a repair queue that the sweep retries, and
that is saved in `meta/repairs` (a front's in `meta/fronts/slot-N.repairs`)
so the next mount repairs it before it serves. Because records hold absolute
state, a repair is idempotent. Paths still queued at unmount are listed on
exit with where they are saved. See the
[runbook](../runbooks/recording-failures.md).

## The sandbox overlay

The sandbox overlay is a second mount point of one volume. It shows only the
names the operator's rules would record if they were new. A container that
starts other containers is given only this mount point, so the sandboxes it
starts see and change only recordable files, and what they change is
recorded and visible through the main mount at once. It is a boundary for
names. The operator's view is the [sandbox-overlay
runbook](../runbooks/sandbox-overlay.md).

Turn it on with `mount --sandbox-overlay PATH --sandbox-owner UID:GID`.
Its source is `agentfs:<volume>:sandbox-overlay`, so `probe` and recovery tell
the two mounts apart.

**One daemon, two sessions, one `Ops`.** The daemon serves both mount points
as two FUSE sessions over one `Ops`: one inode table, one name ordering, one
set of open files, one recorder and one sweep. `overlay::Overlay` is a
decorator in front of that `Ops`; `Ops` does not know it is there. A change
through the overlay is recorded like any other, with the view owner as its
caller. Restore and revert check files open through either mount.

**The visibility gate** (`agentfs_core::visibility`) is the one judge. It is a
pure decision over the volume's rules and never asks whether a name exists,
so its answer for a name is the same whether or not something is there. A
rules error hides or refuses; it never shows.

- *Show.* A file or symlink is shown when its path is recordable. A directory
  is shown when the rules could record something beneath it. A special file
  is never shown. An fsync-class group is shown whole or not at all, and
  `-shm` files never.
- *Place new.* A recordable file name goes to the shared tree. A file name the
  rules skip goes to the tmp layer. A directory goes to the tree when the
  rules could record beneath it, else it is refused. Symlinks, hard links and
  special files are always refused.
- *Place move.* A directory never moves (`EXDEV`: `mv` and similar tools copy
  what they can see and delete it). A symlink never moves. A shown tree file
  moves only to a recordable name. A tmp file moves into the tree under a
  recordable name, and stays in the tmp layer otherwise.

**The tmp layer** (`overlay::tmp`, in `<state>/tmp-layer`) holds the files a
sandbox creates under names the rules skip, so the save-by-temp-file-and-rename
pattern works without recording temp names. It is the overlay's upper layer: a
tmp name hides a hidden tree entry of the same name. It must be on the backing
tree's filesystem, so moving a file into the tree is one atomic `renameat2`.
Renaming a tmp file onto a recordable name records it at the new path only; no
tmp name reaches history. The main mount, history and restores never see a
tmp file. It is wiped at start and at stop. Nothing limits its size, and it
counts in `agentfs.state.bytes{part="tmp_layer"}`.

**The view owner** (`--sandbox-owner UID:GID`) is the one user every
operation through the overlay is checked and done as, whoever the caller is,
as NFS's `all_squash` does. It has no supplementary groups, so the sandbox
process's own groups never count. The overlay's session has no
`default_permissions`: the overlay checks every access itself against the
stored mode bits, including `user.*` extended attributes and the execute bit
for an open to execute. The daemon must run as that uid or as root. `getattr`
reports the real stored owners.

**Name checks run under the names ordering.** The overlay hands every
operation it passes on a `NameCheck` (its `Judge`). `Ops` calls it while it
holds the names ordering, after it has resolved the names and before it changes
the tree. So the kind the gate judges is the kind that is there when the change
is made, and a hidden entry at a placeable name is refused and never replaced
or opened.

**Coherence by events.** Each session's kernel caches names for 1 s and the
pages of files open through it, so a change through one session is an event
every other session's kernel must hear. A `Coherent` decorator reports each
change it served, and one worker thread (`Invalidator`) sends the entry and
inode invalidations to every other session, never from a request thread, since
the kernel may wait on a request of the session it invalidates. The other
session's kernel hears a change a moment after the call returned, not before.

What a sandbox sees, by case:

| Case | Answer |
|---|---|
| Lookup or `getattr` of a hidden name, whether or not it exists | `ENOENT`, and absent from listings |
| The rules could not decide | `EIO`, hidden or refused |
| Create of a skipped file name, `O_EXCL` included | A new tmp file, never `EEXIST` from a hidden tree entry |
| `mkdir` of a directory the rules could record nothing beneath | `EACCES` |
| Create, `mkdir` or rename onto a recordable name holding a hidden kind | `EACCES` |
| `symlink`, `link`, `mknod` of anything but a regular file | `EACCES`, whatever exists |
| Rename of a recorded file to a skipped name | `EACCES` |
| Rename or exchange of a directory | `EXDEV` |
| `RENAME_EXCHANGE` with a tmp end | `EACCES` |
| `RENAME_WHITEOUT` | `EINVAL` |
| `rmdir` of a directory holding tmp files | `ENOTEMPTY` |
| `copy_file_range` between a tmp file and a tree file | `EXDEV` (programs fall back to read and write) |
| `chown`, `chgrp`, set-id or sticky bits | `EPERM` |
| `setxattr`, `removexattr` | `ENOTSUP` |
| `getxattr` outside `user.*` | `ENODATA` |
| The view owner lacks the access | `EACCES` |

Each refused change is counted once in `agentfs.sandbox_overlay.refusals` by
`reason`. Reads of hidden names are not counted. The full per-operation table
is the module documentation of `agentfs-fuse`'s `overlay.rs`.

**Limits.**

- A sandbox loses hard links, creating symlinks, directory moves, WAL
  databases (their `-shm` file is never recordable) and `git init` and `git
  clone` (a `.git` directory is never a repository path). The last two are
  not tested.
- Shown entries' inode numbers skip one per hidden entry, so `d_ino` leaks
  that something is hidden. Not tested.
- A tmp file whose directory the main container removes stays in the store,
  still reachable through the handles open on it, until the next wipe. Not
  tested.
- Locks are not shared between the two mounts, and mode-bit checks run outside
  the names lock.
- The overlay is served only by a single-process `mount`. It is refused on a
  front, so it is not available on CSI volumes. It is also refused with
  `agentfs.respectTreeRules`, whose rule files a sandbox could edit.

**Boot refusals.** Each exits 1 before anything is mounted: tree rules, an
overlay mount point that is the main one or nested with it, the two mount
points in one parent directory (a container given that directory would reach
both), a tmp layer on another filesystem than the backing tree, a front, and a
view owner the daemon cannot act as. An unmount of either mount point from
outside stops the daemon, which unmounts the other.

## The `agentfs` command

The binary composes the layers: it takes the state-area lock, opens the
volume, runs the patch phase, starts the service and the push loops, mounts
FUSE, serves the control socket, and on a signal shuts down in order.

### Commands

| Command | What it does |
|---|---|
| `init` | Creates a state area, or accepts one that already belongs to the volume. Safe to run at every start. Exit 0 for the volume it holds, 1 for another volume or branch. `--include` sets the config's `include.path` entries; `--root` creates the backing tree |
| `mount` | Mounts a backing tree at a mount point and runs the commit and push loops in the same process until SIGINT, SIGTERM or an unmount |
| `run` | Runs the commit and push loops in the foreground with no mount, serving `push` on the control socket |
| `serve-history` | The history service of a volume, for fronts on the same node |
| `serve-git` | Serves the volume's history repository over git's smart HTTP as the patch source. Takes no lock |
| `record` | Records the given paths of a backing tree directly, with no mount, through the same recorder. A path in the fsync class records its whole group atomically |
| `flush` | Commits every recorded change now |
| `push` | Pushes once to every remote (or those named with `--remote`). With a daemon running, asks it over the control socket |
| `status` | Read only: the head, the last committed sequence, the backlog, the lock holder, the LFS threshold and each remote's state. Takes no lock |
| `probe` | Reports whether a mount point holds a live agentfs mount. Takes no lock and reads no state area |
| `restore` | Writes a path back as it was in a commit |
| `revert` | Undoes a commit's changes |
| `checkpoint` | Cuts a checkpoint on the change line |
| `patch` | The patch queue |
| `lfs` | The local LFS object cache |
| `fork` | Forked volumes |
| `start` | A volume started from another branch |
| `trees` | The git-backed tree views (below) |

Nested commands:

| Group | Commands |
|---|---|
| `patch` | `submit` (the producer's side, in its own git repository), `approve`, `apply`, `status` |
| `lfs` | `fetch`, `prune` |
| `fork` | `create`, `finish`, `status`, `abandon`, `list`, `deliver` |
| `start` | `accept` |
| `trees` | `init`, `fork`, `mount` |
| `serve-git` | `admit` and `arrived`, the git hooks, hidden from help |

A finished fork is read-only: `mount` (not a front), `run`, `record`,
`restore`, `revert` and `patch` `apply` refuse it.

Details worth knowing before you script them:

- `status` reads the state area and takes no lock, so it runs beside a
  daemon. `push` prints a `queue:` line for the patch remote.
- `probe MOUNTPOINT [--allow-stale]` exits 0 for a live agentfs mount; 1 for a
  stale one (0 with `--allow-stale`), for nothing, and for another
  filesystem. With `--sandbox-overlay` it expects the overlay's mount, and a
  mount of the other kind is exit 1 either way round. It is the startup and
  readiness probe.
- `restore PATH COMMIT` and `revert COMMIT` take `--force` and `--dry-run`.
  Through a running mount or the history service, they go over the control
  socket. With nothing running they write `--root` directly. Exit 0 done, 1
  refused (with a reason and a hint) or failed, 75 busy. A restore or revert
  is recorded as one commit.
- `patch apply` with nothing running fetches and applies the queue under the
  lock. With a daemon running it asks the daemon, which judges without landing
  and says so (exit 75, "workload attached; lands at next mount").
- `lfs fetch` takes no state-area lock, holds the cache lock shared, and exits
  1 unless every object is cached afterwards. `lfs prune` takes the cache lock
  exclusive and exits 75 while restores or fetches hold it.
- `fork finish` exits 0 when published, 1 when pending at the timeout or
  refused, 75 when busy. `fork status` reads state with no lock and no
  network. `fork list` reads published changes on a fork remote from a
  receiver's repository, never a state area. `fork deliver` exits 0 when
  delivered or already, 1 when refused or not published, 75 when unavailable.
- `start accept --tip OID` takes no lock.

### The state-area lock and exit codes

`mount`, `run`, `serve-history`, `record`, `flush`, `patch apply`, `fork
create`, `fork finish`, `fork abandon`, and `init` of an existing state area
take the state-area lock. They exit **75** (`EX_TEMPFAIL`) while another
process holds it, unless `--wait-lock DURATION` (`AGENTFS_WAIT_LOCK`) says to
wait. `push`, `restore` and `revert` take it only in direct mode, with no
daemon answering. Fronts take no lock.

| Code | Meaning |
|---|---|
| 0 | Done |
| 1 | Refused or failed. The message says why and, where there is one, the next command |
| 75 | The state area, or the cache lock, is busy, or a remote is being pushed (`push --lease`): try again |
| 130 | A second signal arrived during shutdown: the process exits at once |

### `mount`: order and shutdown

**Order.** Preflight; lock; open the volume (journal replay, repairs); the
patch phase (fetch the inbox within `agentfs.patchFetchTimeout`, apply the
queue, block on the first failure); then the service and the push loops;
then the FUSE session; then the control socket. A blocked queue never stops
the mount. The overlay, when asked for, is mounted last, so a probe of it
gates on both mounts. If it fails to mount, the main mount is unmounted as at
a stop.

**A mount point that is not empty** (and not stale) is mounted over with a
warning: what is in it is hidden and was not recorded.

**`--seal-mountpoint`** (`AGENTFS_SEAL_MOUNTPOINT`) makes the bare mount point
root's with mode 0000 before mounting (needs root). What a workload reaches
while no daemon is mounted is then a directory no non-root process can write:
`EACCES` instead of a write nothing records. It seals the overlay's mount point
too.

**A stale mount point** (`--on-stale-mount`):

- `refuse` (the default) exits 1 with the command that clears it.
- `hold` mounts nothing and takes no lock, waits for SIGINT or SIGTERM, then
  lazily detaches every stale layer and exits 1. Recovery is then a restart by
  hand.
- `recover` is for a restarted sidecar. It takes the lock (the kernel released
  the dead daemon's), opens the volume, mounts FUSE at `<state>/meta/stack/mnt`,
  bind-mounts that over the stale mount point, makes the new layer shared and
  lazily detaches the private directory. Mount propagation carries the new
  layer into every copy of the stale mount: workload containers and `subPath`
  binds. It never detaches the stale mount first, because workload containers
  hold copies and their writes would reach the bare directory in between. It
  needs `CAP_SYS_ADMIN`, is refused for fronts (the CSI driver stacks those),
  and refuses (exit 1, naming both) a stale mount whose source is not
  `agentfs:<this volume>`, since stacking over another volume's dead mount
  would send that volume's workloads into this one's history. With nothing
  stale, `recover` is a normal mount. After each stop, a mount detaches the
  stale layers (at most 16) still at its mount point.

**Shutdown** on SIGINT, SIGTERM or an external unmount:

1. Stop the control socket, which refuses a client's connect from then on.
2. Unmount: a plain unmount, then a lazy detach if busy (`fusermount3 -u`,
   then `-u -z`, when not root), and wait up to `--unmount-grace` (10 s) for
   the session to end.
3. Run a final sweep and capture every fsync-class group with changes.
4. Make the final commit.
5. Push it once more to each remote that lacks commits, within `--push-grace`
   (default 5 s).
6. Flush telemetry and release the lock.

A second signal exits 130.

### The control socket

A running `mount`, `run` or `serve-history` listens on
`<state>/meta/control/agentfs.sock` while it holds the lock. The directory is
`0700`, the socket `0600`, and each connection's peer credentials
(`SO_PEERCRED`) must be root or the daemon's own user. Both ends address the
socket as `/proc/self/fd/<dir fd>/agentfs.sock`, so a long state-area path
never hits the 108-byte `sun_path` limit.

The protocol is version 1: JSON lines, one request per connection, at most 64
KiB, read within 10 s.

| `op` | Request fields | Reply |
|---|---|---|
| `restore`, `revert` | `path` (restore only), `commit`, `force`, `dry_run` | `ok`, `refused` or `busy` |
| `push` | `remotes` (optional), `timeout_ms` (optional) | `pushed` |
| `receive_fork` | `path`: a finished fork's state area | `received` |
| `apply` | none | `attached`: the queue judged, not applied, since this daemon holds the volume |
| `checkpoint` | `message` (optional) | `checkpoint` (`change` and `marker`, both `null` when nothing is new) |
| `judge` | none | `judged`: the queue judged and the integrator notified; nothing landed |

A reply is `{"ok":{...}}`, `{"pushed":{...}}`, `{"received":{...}}`,
`{"attached":{...}}`, `{"checkpoint":{...}}`, `{"judged":{...}}`,
`{"refused":{"kind":...,"message":...,"hint":...}}`, `{"busy":"..."}` or
`{"error":"..."}`.

One restore or revert runs at a time; a second gets `busy`. Pushes run beside
them and each other. `mount` writes a restore through its own mount.
`serve-history` writes it into the backing tree after calling every live front
to settle, report open files and drop caches. `run` serves no mount, so it
answers `restore` and `revert` with `busy`. A daemon from before a newer operation answers `{"error":"malformed
request: unknown variant ..."}`, and the command then falls back to its direct
path, which the lock refuses with exit 75.

### Fronts

`mount --history-service ADDR` is a front: no lock and no control socket.
Order: preflight; lease a pending slot; connect to the service, waiting up to
`--history-wait` (30 s) and exiting 1 with nothing mounted if it never answers;
then the FUSE session, whose repair queue is the slot's
`meta/fronts/slot-N.repairs`. Every round trip is bounded by
`--history-timeout` (10 s). `--history-slot N` asks for a particular slot, so
a front restarted after a crash drains its predecessor's repairs.
Shutdown is as for `mount`, without the commit and the pushes, which are the
service's. `record --history-service ADDR` records paths through the service
in the same way.

`serve-history --root BACKING` takes the lock (holder `command=serve-history`),
opens the volume, applies the patch queue only when no front lease is live and
the launcher did not say a workload is attached, starts the commit and push
loops and the control socket (`push` only), and listens on `--listen`
(`AGENTFS_HISTORY_LISTEN`, default `unix:<state>/meta/history/history.sock`).
It writes the address to `meta/history/address` for `status`. On a signal it
stops accepting at once, waits up to `--shutdown-grace` (10 s) for requests in
progress (those still running fail with `EIO`), makes the final commit and
pushes it once more within `--push-grace`. `status` adds a `history:` line
while a service holds the lock, a `fronts:` line per live lease, and a
`repairs:` line for a slot whose repair file waits for a front. The protocol
and its failure modes are in the [core layer](core.md#several-mounts-one-writer-the-history-service)
and the [history-service-down runbook](../runbooks/history-service-down.md).

### Telemetry

`mount`, `flush`, `record`, `push` and `run` export OpenTelemetry signals over
OTLP when the standard `OTEL_*` variables configure an endpoint, and print
alarms to stderr. `init` and `status` export nothing. All of these commands
take the pod flags (`--pod`, `--namespace`, `--node`, `--replica` and their
`AGENTFS_*` variables), so every command's telemetry resource is the same.
See [telemetry](../telemetry.md).

## Tree views

`trees` is a separate set of commands over a git-backed tree library
(`agentfs_core::tree_actor`). It has its own state format, not the volume's,
and it mounts flat regular files only. Use it to try the model; it is not a
replacement for `mount`.

```sh
agentfs --state STATE trees init
agentfs --state STATE trees mount ROOT --sandbox SANDBOX
```

`trees mount` recovers and mounts the root's view, and with `--sandbox` also a
sandbox fork (`fork/N`) of it. Each `Binding` names its tree by
`agentfs_core::tree_actor::TreeId`. `--fork fork/N` reuses a sandbox, and `trees
fork` makes one and prints its name. `--writer` and `--sandbox-writer` set
each view's trusted writer label, and `--metrics-output FILE` writes counters
every 100 ms and at unmount. Mount points must be empty, separate and outside
the state area. Unmount every view to exit.

The views use zero entry and attribute TTLs and direct I/O. Read-only
descriptors keep saved bytes; writable ones read their own calls and the
file's page writes. A read with no lock owner is a page fill and a write with
`FUSE_WRITE_CACHE` is a page write, for shared mappings. `fsync` flushes the
journal and the final release publishes. The code is `agentfs_fuse::tree_view`
(`Binding`, `serve`, `serve_with_metrics`); the rules are in [trees](../trees.md).

## What this layer does not do

- It does not record anything when the daemon is not mounted. A workload that
  reaches the bare mount point while no daemon is mounted writes unrecorded.
  `--seal-mountpoint`, a read-only `emptyDir` in Kubernetes, or a stacked
  mount closes that gap in the deployments that use them; a root workload
  with default capabilities can still write to an unsealed bare directory.
- It does not detect a hung daemon that is alive but not answering.
- It does not share POSIX or BSD locks between mounts. Each mount's kernel
  keeps its own, so a SQLite or other lock-coordinated database must be
  written from one mount.
- It does not enforce POSIX ACLs. Only mode bits are enforced.
- It does not record ownership, timestamps, extended attributes or any
  permission bit but the executable bit.
- It does not record a change made through a hard link it has never seen. An
  inode's paths are the names the mount has looked up, created or linked. A
  hard link made before the mount and never looked up is not known, so a
  change through another name is not recorded at it.
- It does not commit or push by itself. The core layer's `Service` does;
  `agentfs mount` runs both.
- It does not serve the sandbox overlay on a front.
- It needs Linux 5.6 or later for the confined `openat2` access, and kernel
  6.9 or later with privileges for passthrough.
