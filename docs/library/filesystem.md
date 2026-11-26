# The filesystem: `agentfs-fuse` and `agentfs-cli`

The filesystem layer turns file calls into records. It is a FUSE filesystem
over a real backing directory. Every change a workload makes through the
mount is recorded, through [`agentfs-core`](core.md) or, on a tree view,
[`agentfs-engine`](engine.md), before the call
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
| `tree_view` | The view host: FUSE views of the tree library on clients' sessions (below) |

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

### Stale mounts

`agentfs_fuse::stale` holds what the recording mount and the tree view host
share about mounts whose daemon died. An agentfs mount is a FUSE mount whose
source starts `agentfs:`; `source_volume(source)` gives the volume it names
(`agentfs:<volume>` or `agentfs:<volume>:<anything>`).

- `mount_state(path)` tells `MountState::Live`, `Stale` (the daemon is gone),
  `NotMounted` or `Foreign` from `/proc/self/mountinfo` and a `statfs`.
  Nothing else is read: no state area, no lock.
- `is_disconnected(errno)` (and `is_disconnected_io` for a `std::io::Error`)
  tells a dead FUSE connection's answer: `ENOTCONN` once the kernel has
  disconnected the mount, and `ECONNABORTED` for a call that was queued or in
  flight when the connection was aborted. That can be a call made just after
  the daemon died, while a child it forked still holds `/dev/fuse` until its
  `exec`. Every probe of a possibly dead mount asks it.
- `detach(path)` detaches whatever is mounted there lazily: `umount2` with
  `MNT_DETACH` with the privilege to unmount, `fusermount3 -u -z` otherwise.
- `detach_stale_layers(path, max)` detaches stale layers, topmost first, while
  the topmost is stale, at most `max` (`STALE_LAYERS`, 16). `agentfs_layers`
  counts the agentfs mounts at a path.
- `prepare_stack_source(dir)` and `stack(dir, target)` stack a new mount over
  a stale one. FUSE is mounted at a private empty directory, bind-mounted over
  the stale mount, made shared so propagation carries it into every copy of
  that mount (workload containers, subPath binds), and the private directory
  is detached. Stacking needs `CAP_SYS_ADMIN`.

`mount_state`, `detach`, `detach_stale_layers` and `agentfs_layers` are also
re-exported at the crate root.

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
names; what it covers and what it does not is in
[guarantees](../guarantees.md#the-sandbox-overlay).

The `agentfs` binary does not serve it: tree volumes do not build it yet. A
program linking `agentfs-fuse` turns it on with `MountConfig::overlay`. Its
source is `agentfs:<volume>:sandbox-overlay`, so recovery tells the two mounts
apart.

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
- The overlay is served only by a single-process `Mount` with no front; the
  `agentfs` binary serves none. It is also refused with
  `agentfs.respectTreeRules`, whose rule files a sandbox could edit.

**Boot refusals.** Each exits 1 before anything is mounted: tree rules, an
overlay mount point that is the main one or nested with it, the two mount
points in one parent directory (a container given that directory would reach
both), a tmp layer on another filesystem than the backing tree, a front, and a
view owner the daemon cannot act as. An unmount of either mount point from
outside stops the daemon, which unmounts the other.

## The `agentfs` command

The binary runs two engines. `init`, `mount`, `host` and `fork end` run the tree
views ([below](#tree-views), and [trees](../trees.md)): `mount` is the sidecar's,
Docker's and the command line's launcher, and `host` is the CSI driver's
([csi](../csi.md)). The other commands run the recording engine, on recording
state areas only. No flag selects an engine. Every recording command refuses a tree volume before it reads
or writes anything (`` `agentfs push` runs on the recording engine only; <state>
is a tree volume, where it is not built yet ``, exit 1), and `init` and `mount`
refuse a recording state area, naming the
[upgrading runbook](../runbooks/upgrading-the-sidecar.md).

### Commands

| Command | What it does |
|---|---|
| `init` | Creates a tree volume, or finds it under the same name; prints `<name> <id> created\|found`. Safe to run at every start. Exit 1 for another name or format, 75 busy. `--include` sets the operator settings' includes; `--owner` the new root's owner |
| `mount` | Serves a tree volume's root, and optionally a fork, as tree views until SIGINT, SIGTERM, the exit of `-- COMMAND` or an unmount |
| `host` | Serves a tree volume's views on its parent's requests over its standard input, until EOF, SIGINT or SIGTERM ([below](#the-view-host-command)). The CSI node plugin runs one per volume in use |
| `front` | A front of a history service: a FUSE mount of a recording volume's backing tree whose records go to the service |
| `run` | Runs the commit and push loops in the foreground with no mount, serving `push` on the control socket |
| `serve-history` | The history service of a volume, for fronts on the same node |
| `serve-git` | Serves the volume's history repository over git's smart HTTP as the patch source. Takes no lock |
| `record` | Records the given paths of a backing tree directly, with no mount, through the same recorder. A path in the fsync class records its whole group atomically |
| `flush` | Commits every recorded change now |
| `push` | Pushes once to every remote (or those named with `--remote`). With a daemon running, asks it over the control socket |
| `status` | Read only: the head, the last committed sequence, the backlog, the lock holder, the LFS threshold and each remote's state. Takes no lock |
| `probe` | Reports whether a mount point holds a live agentfs mount, a tree view or a front. Takes no lock and reads no state area |
| `restore` | Writes a path back as it was in a commit |
| `revert` | Undoes a commit's changes |
| `checkpoint` | Cuts a checkpoint on the change line |
| `patch` | The patch queue |
| `lfs` | The local LFS object cache |
| `fork` | Forked volumes, and `fork end` for a tree volume's keyed fork |
| `start` | A volume started from another branch |

Nested commands:

| Group | Commands |
|---|---|
| `patch` | `submit` (the producer's side, in its own git repository), `approve`, `apply`, `status` |
| `lfs` | `fetch`, `prune` |
| `fork` | `create`, `finish`, `status`, `abandon`, `list`, `deliver`; `end` (tree volumes) |
| `start` | `accept` |
| `serve-git` | `admit` and `arrived`, the git hooks, hidden from help |

A finished fork is read-only: `run`, `record`, `restore`, `revert` and `patch`
`apply` refuse it; its front mounts read-only.

Details worth knowing before you script them:

- `status` reads the state area and takes no lock, so it runs beside a
  daemon. `push` prints a `queue:` line for the patch remote.
- `probe MOUNTPOINT [--allow-stale]` exits 0 for a live agentfs mount; 1 for a
  stale one (0 with `--allow-stale`), for nothing, and for another
  filesystem. It is the startup and readiness probe.
- `restore PATH COMMIT` and `revert COMMIT` take `--force` and `--dry-run`.
  Through the history service, they go over the control socket. With nothing
  running they write `--root` directly. Exit 0 done, 1 refused (with a reason
  and a hint) or failed, 75 busy. A restore or revert is recorded as one
  commit.
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

`run`, `serve-history`, `record`, `flush`, `patch apply`, `fork create`, `fork
finish` and `fork abandon` take the state-area lock. They exit **75**
(`EX_TEMPFAIL`) while another process holds it, unless `--wait-lock DURATION`
(`AGENTFS_WAIT_LOCK`) says to wait. `push`, `restore` and `revert` take it only
in direct mode, with no daemon answering. Fronts take no lock. `init`, `mount`
and `fork end` hold a tree volume's own lock (`actor.lock`) and exit 75 at once
while another process holds it; they refuse `--wait-lock`.

| Code | Meaning |
|---|---|
| 0 | Done |
| 1 | Refused or failed. The message says why and, where there is one, the next command |
| 75 | The state area, or the cache lock, is busy, or a remote is being pushed (`push --lease`): try again |
| 127 | `mount -- COMMAND`: the command could not start |
| 130 | A second signal arrived during shutdown: the process exits at once |

### `front`: order and shutdown

`front --history-service ADDR BACKING MOUNTPOINT` is a front: no lock and no
control socket. **Order:** preflight (layout checks; a missing mount point is
created); lease a pending slot; connect to the service, waiting up to
`--history-wait` (30 s) and exiting 1 with nothing mounted if it never answers;
then the FUSE session, whose repair queue is the slot's
`meta/fronts/slot-N.repairs`. Every round trip is bounded by
`--history-timeout` (10 s). `--history-slot N` asks for a particular slot, so
a front restarted after a crash drains its predecessor's repairs.

**Shutdown** on SIGINT, SIGTERM or an external unmount: stop answering the
service's calls; unmount (a plain unmount, then a lazy detach if busy) and wait
up to `--unmount-grace` (10 s) for the session to end; run a final sweep and
capture every fsync-class group with changes; flush telemetry. The commit and
the pushes are the service's. A second signal exits 130.

### The control socket

A running `run` or `serve-history` listens on
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
them and each other.
`serve-history` writes it into the backing tree after calling every live front
to settle, report open files and drop caches. `run` serves no mount, so it
answers `restore` and `revert` with `busy`. A daemon from before a newer operation answers `{"error":"malformed
request: unknown variant ..."}`, and the command then falls back to its direct
path, which the lock refuses with exit 75.

### The history service

`record --history-service ADDR` records paths through the service as a front
does.

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

`front`, `flush`, `record`, `push` and `run` export OpenTelemetry signals over
OTLP when the standard `OTEL_*` variables configure an endpoint, and print
alarms to stderr. `mount` and `host` export the `agentfs.not_built` gauge (one series
per capability a configured setting needs and tree volumes do not build) and
the export-failure counter. `init` and `status` export nothing. All of these commands
take the pod flags (`--pod`, `--namespace`, `--node`, `--replica` and their
`AGENTFS_*` variables), so every command's telemetry resource is the same.
See [telemetry](../telemetry.md).

## Tree views

`init`, `mount`, `host` and `fork end` run the tree views over a git-backed tree
library (`agentfs_engine::tree_actor`), with their own state format, mounting
directories and regular files with their recorded modes ([trees](../trees.md)).

```sh
agentfs --state STATE init --volume NAME [--include FILE]... [--owner UID:GID]
agentfs --state STATE mount ROOT [--fork-mount FORK [--fork-key KEY]] [-- COMMAND...]
agentfs --state STATE host --launcher ID --volume NAME    # stdin: a Unix socket
agentfs --state STATE fork end --fork-key KEY
```

`mount` first judges each view's mount point (a live or foreign mount exits 1; a
stale one is refused, held or stacked over by `--on-stale-mount`; a bare one is
created when missing, refused when it holds files, and sealed with
`--seal-mountpoint`), then reads the operator settings (`<state>/config`, an
include under a view refused, each key not built yet logged and counted by
`agentfs.not_built`), then recovers and mounts the root's view, and with
`--fork-mount` also a fork (`fork/N`) of it: a new one, which ends when its view
is unmounted, or the open fork under `--fork-key`; it never creates a volume.
Each view is a client of its tree: the command rebinds the client a killed mount
left (same launcher, tree, label and access), else attaches one, then ends its
launcher's other clients and drops the ended forks nothing holds, and detaches
each client as its view ends ([trees](../trees.md#clients)). The volume is
composed by the service ([`agentfs-service`](service.md)), whose one
dispatcher hands each domain event to its consumer after the call that
caused it:
`LastDetached` to Trees (an unkeyed fork ends), `ForkDropped` to Clients (its
writer claim is released), and each landing the view host takes makes Trees
judge the drops again ([forks](../trees.md#forks-keys-end-and-the-drop)).
`--writer` and `--fork-writer` are the clients' labels; `--read-only` and
`--fork-read-only` make a client read-only. A fork's second read-write client
exits 1. Once every view is up it prints `mounted volume NAME at ROOT[, fork
fork/N at FORK]` and starts `-- COMMAND`, whose exit drains the volume and whose
status, when not 0, is the mount's ([trees](../trees.md#running-a-command-in-the-mount)).
`fork end` ends a keyed fork (exit 75 while a mount holds the volume). The views
mount as `agentfs:<volume>` (a fork's as `agentfs:<volume>:<tree>`), with FUSE
`allow_other` unless `--no-allow-other` (`MountOptions`), each entry presented
with its recorded owner, and `--metrics-output FILE` writes counters every
100 ms and at unmount. A stop, the command's exit or unmounting every view
drains it.

### The view host command

`agentfs host` serves one tree volume's views on request: a parent process
opens and closes views on it, and creates and ends keyed forks, over the
host's standard input. It is the integration point for a launcher that serves
views of one volume to several consumers over time; the CSI driver is one
([library/kubernetes](kubernetes.md)). The transport follows the LSP and MCP
stdio transports and Erlang ports: a parent drives its child with JSON
messages over the child's own stdio. The messages are pure values in
`agentfs_contracts::host`.

**The control channel.** The standard input must be a Unix stream socket, one
end of a socket pair whose other end only the parent holds. Anything else
exits 2 before the state area is touched (`agentfs host is run by a launcher
that controls it over its standard input, a Unix socket`). No socket path
exists, so only the parent can reach it. The parent closing its end (EOF) is
a stop, as SIGTERM and SIGINT are, so a host never outlives its parent. With
no view open the host drains and exits 0 once Volume releases the volume. With
a view open it exits 3 at once, logging `stopped with N view(s) open`: each
view's mount is left stale and its client attached, as a crash leaves them,
because the views belong to pods the parent does not own; the next host
rebinds each client and stacks over each mount. Every change a view took is in
the journal already. Before it exits it ends the volume's own processes, its
git children, as a crash ends them, and waits for each, so the volume's lease
is free once the parent has reaped it: a next host started then is not busy. A drain logs `tree DrainBlocked` with the clients of
other launchers it waits for: only their own launcher ends them. The last view
ending does not stop the host.

**Boot.** As `mount`'s, under `--launcher ID`: the volume found (exit 1 for
another format, 75 while another process holds it) and checked against
`--volume`, the operator settings read and each key not built logged and
counted by `agentfs.not_built`, the volume staged and its domains restored as
that launcher, and the view host started. It then writes `Hello` and reads
requests. `--namespace`, `--pod`, `--pod-uid` and `--node` are its own
events' pod context and its telemetry resource's; each view's writer carries
the pod context its `open` names.

**Protocol, version 2.** One JSON object per line. The host writes `Hello`
once (`{"v":2,"volume":NAME,"id":ID}`). After it, every request and every
reply travels in an envelope with an id, as JSON-RPC's and LSP's do:

```text
{"id":7,"body":{"op":"close","at":"/m"}}
{"id":7,"body":"closed"}
```

The parent picks each request's id, unique among its requests still
unanswered, and need not wait for a reply before it sends the next. The host
answers each request once, in an envelope that echoes its id, in any order.
A parent reads `v` first and refuses another version: the CSI driver's
plugin answers `FAILED_PRECONDITION` (`the host of volume <v> speaks host
protocol v<n>, this plugin v2`) and never starts that host again, because the
node plugin's image and its `agentfs` binary must be of one release.

**What a request waits for.** A request is answered at once unless a `close`
whose view has not ended holds a resource it names, or a request already
waiting names one of them. The resources are a view's mount point, its
client label and, for a fork's view, its fork key:

| Request | Names |
|---|---|
| `open` | its `at`, its `label` and, for a fork's view, the fork's key |
| `close` | its `at` |
| `fork_create`, `fork_end` | its `key` |
| `reconcile` | nothing: a closing view is still live. It still waits for every `open` that arrived before it, since it ends the clients no live view uses and a waiting open is about to bind one |

A closing view holds what the `open` that made it named. Requests that wait
are answered in the order they arrived, resource by resource: when a view
ends, its `close` is answered, then each waiting request is looked at again
in arrival order. So a `close` that waits for a file a process still holds
stalls only the requests about that mount, that client or that fork; an
`open` of another claim's view at another mount point is answered at once. A
waiting request cannot be cancelled: it waits as long as the close it waits
for.

| Request (`op`) | Fields | Reply |
|---|---|---|
| `open` | `tree` (`"root"` or `{"fork":KEY}`), `at`, `label`, `read_only`, `pod` | `{"opened":{"tree":..,"stacked":BOOL}}` |
| `close` | `at` | `"closed"`, once the view has ended: its files released and its client detached |
| `reconcile` | | `{"reconciled":{"ended":N}}`: the launcher's clients that no open view uses are ended |
| `fork_create` | `upstream` (a tree as in `open`), `key`, `settings` (Trees' `ForkSettings`) | `{"forked":{"tree":..}}` |
| `fork_end` | `key` | `"ended"` |

Any request may instead get `{"refused":{"kind":KIND,"message":TEXT}}`. The
kinds are exhaustive, so a parent maps each: `fork_taken` (the fork has had
its one writer), `key_unknown`, `key_ended`, `nested_not_built` (forks of
forks), `approval_not_built` (`Landing::Approval`), `mount_failed`, `stale`
(another source's stale mount at `at`), `occupied` (a view is open there with
another tree, label or access), `draining` and `stopped` (the actor stopped
after a recording failure). The message carries the real error. A malformed
line, and a request whose id is already in flight, is the last one read: the
host stops and exits 2.

**Opening a view.** `at` is an existing empty directory, or a stale agentfs
mount of the view's own source, which the view stacks over
(`MountOptions::over_stale`; [stale mounts](#stale-mounts)). The view's client
is rebound when the launcher, tree, label and access match one a crashed host
left, else attached with the request's pod context, so a restarted host keeps a
fork's one writer. An `open` of a view already open at `at` with the same tree,
label and access answers `opened` again; with another, `occupied`. A stacked
open logs `stacked over a stale mount at PATH left by a crashed daemon`. A
`close` of a mount point with no view answers `closed`, and a `fork_create`
under a key whose fork is open answers `forked` with that fork, so a parent
can repeat a request after a crash. The `mount` command's flags for these
choices (`--writer`, `--read-only`, `--on-stale-mount`) are not used: the
request names the label and access, and a stale mount of the view's own
source is always stacked over.

**Exit status.** 0 once the volume is released; 1 for a refusal at boot or a
stopped actor; 2 without a control channel or after a malformed line; 3 when
stopped with views open (left stale, the volume not released, its processes
ended); 75 while another process holds the volume, a killed host's git
children among them until each has exited. At each status the host makes
itself (0, 1, 2, 3) no process it started holds the volume's lease, so the
lease is free when its exit is reaped. A host killed by a signal, or one that
panicked (101), ended nothing: its git children die of their parent-death
signal after it, and the lease is free once each has exited. An end the kernel
refused is logged (`tree AbandonIncomplete`), and the lease is then free once
each git child has exited.

### The view host

`tree_view::ViewHost` serves one volume's views through its sessions
(`agentfs_engine::session::Sessions`) and implements the `Views` port:

```rust
use std::sync::Arc;
use agentfs_contracts::views::{MountOptions, MountPoint, Views};
use agentfs_fuse::tree_view::{HostEvent, ViewHost};

// `sessions` holds the served volume; Clients bound `binding.session`.
let notify = Arc::new(|event: HostEvent| { /* to the owner's thread */ });
let mut host = ViewHost::new(sessions, &state, None, notify)?;
let options = MountOptions { allow_other: true, over_stale: false };
let view = host.open(MountPoint(dir), binding, options)?;
// ... HostEvent::ViewEnded(view) once it is unmounted ...
host.wait()?;
```

`new` reads the volume's name and starts the take worker (one take at a time,
off the actor's lock, woken by built and failed events) and, with a metrics
output, the metrics worker. Each view mounts with the source
`agentfs:<volume>` (`agentfs:<volume>:fork/<n>` for a fork's view) and the
subtype `agentfs`, so `agentfs probe` and stale detection see it as an agentfs
mount. `open` checks the session is bound (`UnknownSession`), then what is
mounted at the mount point ([stale mounts](#stale-mounts)):

- nothing: the mount point must be an existing empty directory outside the
  state area, every open view and the metrics output (`MountFailed(EINVAL)`);
- a stale mount of this volume with `MountOptions::over_stale`: the view is
  stacked over it. FUSE is mounted at `<state>/stack/<view>`, bound over the
  mount point, and the private mount detached; a failed stack (no
  `CAP_SYS_ADMIN`: `EPERM`) unmounts it again and answers `MountFailed`. Once
  the view ends, the stale layers under it are detached (logged as
  `tree DetachedStale` or `tree DetachStaleFailed`);
- a stale mount without `over_stale`, or one of another volume: `Stale`, with
  its source, and nothing mounted;
- a live mount or another filesystem: `MountFailed(EBUSY)`.

It then checks that the tree is among the actor's saved trees
(`MountFailed(ENOENT)`): it never waits for history. The view
writes as the session's client and is mounted read-only for a read-only
grant. It serves `lookup`, `readdir`, `create`, `mkdir`, `rmdir`, `unlink`,
`rename`, `symlink` and `readlink` in any directory (inode 1 the root, every
other inode its node number + 2, `..` its parent's), and each entry's recorded
kind, mode, uid, gid, link count, size and times in `getattr`. A create, a
`mkdir`, a regular file's `mknod`, a `symlink` and `O_TMPFILE` give the new
entry the kernel's mode (`0777` for a link) and the caller's uid and gid; the
library applies a set-group-ID directory's group. `setattr` maps a mode, uid
and gid to one `set_meta`, the values not given kept from the entry's
metadata, and stands behind the lag gate; atime and mtime map to one
`set_times` (`TimeOrNow::Now` to `SetTime::Now`, a specific time to
`SetTime::At`, one before the epoch `EINVAL`), which builds no commit and is
not gated. The kernel judges authority and access (`default_permissions`).
An open of a link answers `ELOOP`; `mknod` of a FIFO or a socket makes it in
the untracked area, and of a device answers `EPERM`; extended attributes are
not served. `RENAME_NOREPLACE` is
served; `RENAME_EXCHANGE` and `RENAME_WHITEOUT` answer `EINVAL`.

Each view has two threads of its own. Its request thread answers the kernel in
order and does not wait for a save while its tree has no failed one (after a
failed save, a `RELEASE` saves on this thread: [trees](../trees.md#pending-saves)):
a `RELEASE` calls `TreeSession::start_close`,
is answered, and hands a `Closing::Decide` to the view's save worker
(`tree_view/save_worker.rs`, thread `tree-saves`), which runs `SaveToDecide::finish`
for each in the order they were begun and logs `tree SaveFailed` or
`tree OrphanDropped`. A job that panics is caught and the worker goes on. A
read-only open reads `SessionGuard::open_map`, so the view's own client sees
its pending saves ([trees](../trees.md#pending-saves)). An open (`open_map` or
`SessionGuard::write_open`) may release the actor's lock to fold those saves, so
`create` replies with the attributes of the object it opened, never a lookup of
the path after the open (a rename may come between), and the view's table of
open handles is locked only after the open, never across the release. A call refused with
`PathStep::WaitFor` (`unlink`, a rename onto a pending file, a move in onto one,
a setattr's path truncate or time set) and a move out of the tree of a file
with a pending save (`SessionGuard::pending_save`) drop the guard and hand a
retry of the whole request to `TreeSession::when_saved_then`, replying only from
the retry, which locks with `TreeSession::lock_retrying` and the first refusal, so
it waits only for the saves begun before that refusal; a setattr retries only what it has not done, so its metadata step is
not made twice. A move out whose unlink is refused after it placed its copy is
retried whole, replacing that copy.
A thread joins each view's session, drains its save worker, then sends
`HostEvent::ViewEnded`; `Landed` follows each landed take,
after its pages were dropped. `close` is a lazy unmount (`stale::detach`),
idempotent. `wait`, after every view ended, joins the workers and fails when
the actor stopped.

Each view merges its tree's untracked area (`TreeSession::untracked_area`)
into the tree ([trees](../trees.md#rules-and-the-untracked-area)), routing each
call to one half in `tree_view`, with the untracked half in
`tree_view/untracked.rs`:

- **Inodes.** A tracked entry's inode is its node number + 2; an untracked
  entry's is `1 << 62 | n`, `n` from the view's table keyed by the backing
  file, with the paths it is known by (more than one after a hard link), kept
  through renames, links and unlinks. The ranges never meet: a tracked number
  whose inode would fall in `[1 << 62, 1 << 63)` answers `EOVERFLOW`.
- **Names.** `lookup` finds the tracked entry first, then the area's. A new
  name (`create`, a regular file's `mknod`, `mkdir`, `symlink`) goes to the
  half the rules place it in (`RuleSet::place`, judged under the actor's lock,
  with `inside_repository` read from the area); an existing name stays in its
  half, and `O_EXCL` answers `EEXIST` for a name in either.
- **Listings.** A tracked directory lists its tracked children at offsets 1
  on, then the area's entries at the same path that no tracked child hides, at
  offsets `(1 << 62) + j + 1`; an untracked directory lists the area's.
- **Untracked calls.** Plain passthrough on the area, never gated or
  journaled: `open` (the caller's access flags with
  `O_NOFOLLOW`, `FOPEN_DIRECT_IO`), `pread` and `pwrite` (page fills and page
  writes included; an `O_APPEND` write at the file's end), `fsync` and
  `fdatasync` of the descriptor, `fchmodat`, `fchownat`, `ftruncate` and
  `utimensat` for `setattr`, `unlinkat`, `mkdirat`, `symlinkat`, `mknodat`
  of a FIFO or socket (the rules place both untracked), `readlinkat`,
  `linkat`, and `renameat2` within the half (`RENAME_NOREPLACE` served). A
  new entry is given to the caller when the view host has `CAP_CHOWN` and
  `CAP_FOWNER`. Every change holds the area against a fork's copy while it
  runs (`AreaWrite`, the only way to change an area). A read-only view answers
  `EROFS` to each.
- **The actor's lock.** Calls through an open descriptor (`read`, `write`,
  `fsync`, `getattr` and a setattr by descriptor) never take it; `open` and
  `setattr` take it only to ask whether the view is writable. Calls that decide
  or change a name (`lookup`, `create`, `mknod`, `mkdir`, `symlink`, `link`,
  `unlink`, `rmdir`, `rename`) and listings do their area I/O under it, so a
  name's half is decided at one instant against the tracked half: an `O_EXCL`
  create, a rename across the halves and a merged listing each see both halves
  as one. They wait behind every other call on the volume, a fork's copy
  included.
- **Across the halves.** A rename's target class is tracked when a tracked
  entry holds the name, untracked when a visible untracked entry does, else
  where `RuleSet::place` puts the source's kind; the table in
  [trees](../trees.md#moves-across-the-halves) gives what each move does.
  Every move but one within the untracked half stands behind the lag gate. A
  tracked file or link moved out is read from `SessionGuard::open_map` (the
  client's pending saves, else a clone of the saved map) with the actor unlocked, placed with `UntrackedArea::place`, then unlinked
  (`SessionGuard::unlink`, which waits for a pending save of the file). An untracked file moved in is one
  `SessionGuard::replace_file` of the source's descriptor (`O_RDONLY |
  O_NOFOLLOW`; mode, uid, gid and size from its `fstat`), then `unlinkat`; the
  view marks every open it has of that file (same `st_dev` and `st_ino`)
  moved, so its writes and truncates answer `ESTALE`, and drops the kernel's
  pages of the target from another thread. A tracked directory's rename first
  removes an empty container left at the target (any other entry there:
  `EXDEV`), then moves its own container after the engine's step. A hard link
  is `linkat` within the untracked half, `first_link` for a tracked anonymous
  file, `EPERM` for a named tracked file and `EXDEV` across the halves.
  `O_TMPFILE` in an untracked directory answers `EOPNOTSUPP`. A tracked
  directory with untracked entries is not empty (`ENOTEMPTY`), and its `rmdir`
  removes its empty container. `fsyncdir` makes the area's directory durable
  as well.

The views use zero entry and attribute TTLs and direct I/O. Read-only
descriptors keep saved bytes; writable ones read their own calls and the
file's page writes. A read with no lock owner is a page fill and a write with
`FUSE_WRITE_CACHE` is a page write, for shared mappings. `fsync` flushes the
journal and the final release begins the save, which the save worker lands. The code is `agentfs_fuse::tree_view`
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
  `agentfs serve-history` and `run` run it.
- The `agentfs` binary serves no sandbox overlay.
- It needs Linux 5.6 or later for the confined `openat2` access, and kernel
  6.9 or later with privileges for passthrough.
