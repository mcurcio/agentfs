# agentfs operator guide

How to run `agentfs mount`: what it needs, how to lay out a volume, its
flags and settings, what it records and when, how it starts and stops, and
what to do when something goes wrong. The design behind it is
[design.md](design.md); section numbers below (§) refer to it.

## 1. Requirements

- **Linux 5.6 or later.** Every path in the backing tree is resolved with
  `openat2`, so no path can leave the tree or follow a symlink. Older
  kernels fail at mount with "the kernel has no openat2".
- **Linux 6.9 or later for passthrough** (optional). With it, reads, writes
  and `mmap` go straight to the backing files without passing through the
  daemon. Without it everything still works, through the daemon.
- **`/dev/fuse`.** In a container, give it the device (and, for a mount
  other containers see, a privileged sidecar: see section 11 below).
- **Run as root** for multi-user volumes. What the daemon does is decided
  from its effective capabilities, not its uid:
  - with `CAP_CHOWN` and `CAP_FOWNER` it gives every new file to the user
    who created it, with the mode that user asked for;
  - with `CAP_SYS_ADMIN` in the initial user namespace it can use
    passthrough. Root inside a rootless container (a user namespace) does
    not have that, so passthrough is off there;
  - `CAP_DAC_OVERRIDE` lets it serve files whose modes would deny its own
    user (the kernel has already checked the caller's permissions).

  Root has all of these. A non-root daemon given them behaves the same; one
  without them works, but every file it creates is owned by the daemon's
  user, and passthrough is off.
- **A non-root daemon** also needs the `fuse3` package (`fusermount3`) and
  `user_allow_other` in `/etc/fuse.conf`, unless it mounts with
  `--no-allow-other`.
- **Filesystem.** XFS (or btrfs) is recommended for the backing tree and
  state area: recording freezes a file's content with a reflink, which costs
  the same whatever the file's size. On ext4 it is a copy (§3.2): measured
  on CI, closing a clean 1 GiB file takes 3 ms on XFS and 2.9 s on ext4,
  while a small file's change costs about 2 to 4 ms through the mount on both.
  Workloads of many small files run 14 to 83 times slower than in a plain
  directory, and excluded paths still pay about 0.6 ms per file in FUSE.
  See [performance.md](performance.md) for the numbers and the budget.
- **`git` 2.32 or later on `PATH`** for the hourly repack (§8); the
  container image has it. Without it each repack fails with a
  `RepackFailed` alarm and nothing else changes; set
  `agentfs.repackInterval = off` to stop trying.
- Building needs no FUSE library: the FUSE layer is pure Rust (fuser).

## 2. Layout

A volume is three directories, side by side:

```
/data/backing   the files the workload sees (the backing tree)
/data/state     history, journal, frozen copies, push state (the state area)
/workspace      where the backing tree is mounted (the mount point)
```

`agentfs mount` refuses to start, before anything is mounted, if:

| Error | Why |
|---|---|
| the state area is inside the backing tree (or the reverse, or they are the same directory) | history, journal and credentials would be visible through the mount |
| the mount point is inside the backing tree or the state area, or either is inside the mount point | the mount would recurse into itself, or the daemon would reach its own files through its own mount |
| a remote's `agentfsCredentials`, `agentfsSshKey` or `agentfsKnownHosts` file is under the backing tree or the mount point, for any `[remote]` section, valid or not | the workload could read the secret, or rewrite known_hosts to trust any host (a man in the middle). This is an error, not an alarm |
| `--root` / `AGENTFS_ROOT` is set and is not the backing tree | the two would disagree about which tree is recorded |
| the mount point is a stale FUSE mount (`ENOTCONN`) | a daemon died; see §10 |

Paths are compared after resolving symlinks, so a symlinked path to inside
the backing tree is caught too.

While mounted, the daemon listens on `<state>/meta/control/agentfs.sock`
(directory `0700`, socket `0600`; only root and the daemon's own user may
connect) for `agentfs restore` and `agentfs revert` (section 12 below). It is
removed at shutdown; a stale one left by a crash is replaced at the next
mount.

## 3. Quick start

```sh
agentfs --state /data/state init --volume hermes
git --git-dir /data/state/git config remote.origin.url https://github.com/acme/agent-history.git
git --git-dir /data/state/git config remote.origin.agentfsCredentials /run/secrets/agentfs/origin
agentfs --state /data/state mount /data/backing /workspace
```

Over SSH instead (a binary built with `--features ssh`), name a private key
and a known_hosts file; both are required, and neither may be under the
backing tree:

```sh
git --git-dir /data/state/git config remote.origin.url git@github.com:acme/agent-history.git
git --git-dir /data/state/git config remote.origin.agentfsSshKey /run/secrets/agentfs/origin-ssh/ssh-privatekey
git --git-dir /data/state/git config remote.origin.agentfsKnownHosts /run/secrets/agentfs/origin-ssh/known_hosts
```

The key must be unencrypted, and known_hosts must list every host key type
the server has (`ssh-keyscan github.com`, fingerprints checked against the
provider's). Every remote key is described in the
[configuration reference](configuration.md).

The daemon stays in the foreground. Its first line says what it mounted
and how:

```
mounted /data/backing at /workspace (volume hermes, passthrough: on, threads 8, 0 uncommitted record(s) recovered, remotes: origin); stop with SIGINT or SIGTERM
```

Every change made under `/workspace` is now recorded before the call that
made it returns, committed to the branch `agentfs/hermes` in
`/data/state/git` a moment later, and pushed every `pushInterval`.

## 4. Flags and environment

Global flags (every command):

| Flag | Environment | Default | Meaning |
|---|---|---|---|
| `--state DIR` | `AGENTFS_STATE` | `.` | The state area |
| `--root DIR` | `AGENTFS_ROOT` | | The backing tree, for `record`, `flush` and `run`, and for `restore` and `revert` when no mount is running. With `mount` it must match the positional backing tree |
| `--wait-lock DURATION` | `AGENTFS_WAIT_LOCK` | `0s` | Wait this long for another process to release the state area instead of exiting 75 (§9) |

`agentfs init` takes `--volume NAME` (`AGENTFS_VOLUME`), `--branch NAME`
and `--include FILE` (repeatable: operator config fragments, set as the
config's `include.path` entries), and creates `--root` when it is missing.
It is safe to re-run: for the same volume it exits 0 and changes nothing
but missing directories and the includes; for another volume or branch
it exits 1 and changes nothing
([configuration reference](configuration.md#agentfs-init)).

`agentfs mount BACKING MOUNTPOINT`:

| Flag | Environment | Default | Meaning |
|---|---|---|---|
| `--passthrough auto\|on\|off` | `AGENTFS_PASSTHROUGH` | `auto` | Kernel passthrough: `auto` uses it when the kernel (6.9+) and privileges (`CAP_SYS_ADMIN`, see §1) allow, `on` refuses to mount without it and stops the mount (exit 1) if a file cannot be given a backing file later, `off` never uses it |
| `--threads N` | `AGENTFS_THREADS` | CPUs, 2 to 16 | FUSE request threads |
| `--no-allow-other` | | | Only the mounting user can use the mount |
| `--unmount-grace DURATION` | | `10s` | At shutdown, how long to wait for open files on a busy mount after detaching it |
| `--push-grace DURATION` | | `5s` | At shutdown, how long to spend on pushes in progress and one last push of the final commit to each remote that lacks commits; `0s` skips the last push (§8) |
| `--on-stale-mount refuse\|hold\|recover` | `AGENTFS_ON_STALE_MOUNT` | `refuse` | When the mount point is an agentfs mount whose daemon died: `refuse` exits 1 with the command that clears it; `recover` (the shipped sidecar, §10) mounts over the stale mount, if it is a mount of this volume (another volume's is refused, exit 1), so every copy of it reaches the new daemon, and detaches the stale layers when it stops (needs `CAP_SYS_ADMIN`; refused for fronts); `hold` mounts nothing and takes no lock, waits for SIGINT or SIGTERM, then detaches the stale mount and exits 1. With nothing stale, `recover` and `hold` mount normally |
| `--seal-mountpoint` | `AGENTFS_SEAL_MOUNTPOINT` | off | Before mounting, make the bare mount point root's with mode 0000 (needs root). The mount on top is unaffected; between mounts (a sidecar restarting after a clean exit), a non-root process, or root without `CAP_DAC_OVERRIDE`, that reaches the bare directory, such as a `subPath` bound in that gap, gets `EACCES` instead of writing unrecorded. The shipped sidecar sets it (§10) |

A missing mount point is created (mode `0755`; its parent must exist), so
a sidecar can mount into a subdirectory of a fresh `emptyDir`.

`agentfs serve-history` (needs `--root`, the backing tree the fronts
mount; §16):

| Flag | Environment | Default | Meaning |
|---|---|---|---|
| `--listen ADDR` | `AGENTFS_HISTORY_LISTEN` | `<state>/meta/history/history.sock` | Where fronts reach it: `unix:<path>` or an absolute socket path |
| `--shutdown-grace DURATION` | | `10s` | At shutdown, how long to wait for fronts' requests in progress |
| `--push-grace DURATION` | | `5s` | At shutdown, how long to spend on pushes in progress and one last push of the final commit to each remote that lacks commits, as `mount` does; `0s` skips the last push |

`agentfs mount` and `agentfs record` as fronts of a history service
(§16):

| Flag | Environment | Default | Meaning |
|---|---|---|---|
| `--history-service ADDR` | `AGENTFS_HISTORY_SERVICE` | | Record through the history service at ADDR instead of journaling and committing in this process; takes no state-area lock |
| `--history-timeout DURATION` | `AGENTFS_HISTORY_TIMEOUT` | `10s` | How long a change boundary waits for the service before it fails with `EIO` |
| `--history-wait DURATION` | `AGENTFS_HISTORY_WAIT` | `30s` | How long to wait at start for the service; then exit 1 with nothing mounted |

`agentfs probe MOUNTPOINT [--allow-stale]` prints what is mounted there
and takes no lock and reads no state area, for Kubernetes startup and
readiness probes:

| Output | Exit status |
|---|---|
| `live agentfs mount (agentfs:<volume>)` | 0 |
| `stale agentfs mount (daemon gone)` | 1, or 0 with `--allow-stale` |
| `not mounted` | 1 |
| `not an agentfs mount (<fstype>)` | 1 |

Pod flags (`mount`, `run`, `record`, `flush`, `push`, `serve-history`), from the downward
API in Kubernetes: `--namespace` (`AGENTFS_NAMESPACE`), `--pod`
(`AGENTFS_POD`), `--pod-uid` (`AGENTFS_POD_UID`), `--node`
(`AGENTFS_NODE`), `--replica` (`AGENTFS_REPLICA`). They fill the commit
trailers and the committer identity (§9) and the telemetry resource.

Telemetry is configured with the standard `OTEL_*` variables; see the
[README](../README.md#telemetry).

## 5. Settings that affect the mount

Settings live in the history repository's own config,
`<state>/git/config` (§4.3). The mount reads them when it starts; changing
them takes a remount. Every key, including the per-remote ones, is in the
[configuration reference](configuration.md).

| Setting | Default | Effect |
|---|---|---|
| `agentfs.commitWindow` | `200ms` | Records this close in time become one commit |
| `agentfs.pushInterval` | `60s` | How often each remote is pushed |
| `agentfs.sweepInterval` | `30s` | How often files held open are checked and recorded, and failed records retried (minimum `1s`) |
| `agentfs.repackInterval` | `1h` | How often the history is repacked with `git repack` (`0` or `off`: never; minimum `1m`); see [Databases](#14-databases-the-fsync-class) |
| `agentfs.nested` | `exclude` | Nested git repositories: `exclude` records nothing in them, `working-tree` records their working files (§5) |
| `agentfs.respectTreeRules` | `false` | Whether `.gitignore` and `.gitattributes` in the tree take part in rules |
| `$GIT_DIR/info/exclude` | | Paths never recorded when new (git ignore syntax) |
| `$GIT_DIR/info/attributes` | | `-agentfs` and the other `agentfs.*` attributes (§4.2) |

## 6. Permissions

- The mount uses the kernel's `default_permissions`: the kernel checks the
  real ownership and mode bits stored in the backing tree, so root and other
  users behave as they would on the backing filesystem. With `allow_other`
  (the default) every user can reach the mount.
- **New files belong to their creator.** When the daemon runs as root, a
  file, directory, symlink or device created through the mount is given to
  the calling user and group, with exactly the mode the caller asked for
  (after the caller's umask). In a setgid directory the group comes from the
  directory, as on any filesystem.
- `chown`, `chmod`, `utimens` and extended attributes are passed through to
  the backing tree. `chmod` of a symlink is not supported (Linux has no
  `lchmod`).
- **POSIX ACLs are not enforced** through the mount; only mode bits are.
- **What history keeps:** file content, the executable bit and symlinks.
  Ownership, other mode bits, timestamps and extended attributes stay in the
  backing tree only (§6).
- `st_ino` through the mount is agentfs's node number: the same for every
  hard link of a file and stable while mounted, but not across remounts.
  Tools that cache inode numbers (`git status` on a repository in the mount)
  re-read files once after a remount.

## 7. What is recorded, and when

Each of these is recorded before the call returns (§3.1):

| The workload does | History gets |
|---|---|
| closes a file it changed (every `close`, including a forked child's) | the file's content and executable bit, at every name it has |
| renames a file or directory | the old name removed and the new one added; a directory's whole content moves |
| deletes a file or directory | the removal |
| makes a hard link or symlink | the new name |
| truncates a file, or changes its executable bit | the new content or mode |

Two late backstops cover writes no close follows:

- when the last descriptor and mapping of a file go away (RELEASE), a
  change made after the last close (through `mmap`, say) is recorded;
- every `sweepInterval`, files still open for writing that changed are
  recorded, so a log held open for days is still in history;
- at unmount, a final sweep records whatever is still open.

Never recorded: anything inside a `.git` directory (git's own data:
`git init`, clones, submodules), paths the rules exclude, empty
directories, FIFOs, sockets and devices. A `.git` that is not a repository
(an ordinary directory or file someone named `.git`) is not recorded
either, but raises one `PathRefused` alarm per such `.git`, so its contents
never vanish without a trace. A path git cannot store (a `.git`
look-alike such as `.GIT`, or a name that is not UTF-8) is not recorded and
raises a `PathRefused` alarm; the file itself works normally.

A file opened for writing but not changed, or closed again without
changes, is not recorded again.

Databases in the `fsync` class are recorded differently, as whole groups
and up to one interval late: see [Databases](#14-databases-the-fsync-class).

## 8. Lifecycle

- **Start.** The daemon takes the state-area lock, checks the layout,
  recovers the journal (records made before a crash are committed),
  fetches and applies the patch queue (§12; at most
  `agentfs.patchFetchTimeout` waiting for the remote), starts
  the commit and push loops, mounts, and prints the line shown in §3.
- **Stop.** On SIGINT or SIGTERM it:
  1. stops serving restores (waiting up to `--unmount-grace` for one in
     progress), then unmounts; if files are still open, the mount is
     detached lazily (it disappears from the namespace, open files keep
     working);
  2. waits up to `--unmount-grace` for those files to close;
  3. runs a final sweep, recording files still open and retrying failed
     records. Paths still failing are listed and saved in
     `<state>/meta/repairs`; the next mount records them from the backing
     tree before it serves anything;
  4. makes the final commit, then pushes it once more to each remote that
     lacks commits, all within `--push-grace` (pushes in progress
     included), printing `final push: <remote>: ...` per remote; a remote
     still pushing at the deadline is left (`... abandoned`), and with
     `--push-grace 0s` nothing more is pushed (`... skipped, push grace
     spent`, or `nothing new`);
  5. flushes telemetry and releases the lock.
  A second signal exits at once.
- **Unmounted from outside** (`umount`, `fusermount3 -u`): the daemon
  notices, runs the same shutdown, and exits 0.
- **Exit status:** `0` clean; `1` an error, or the final commit failed (the
  records stay journaled and are committed at the next start); `75` the
  state area is in use by another process; `130` a second signal.

In Kubernetes, give the pod a `terminationGracePeriodSeconds` longer than
`--unmount-grace` plus `--push-grace` plus a few seconds, so the final
commit and its push always happen. Pushing never delays shutdown beyond
`--push-grace`; commits still unpushed go out at the next start.

## 9. The state-area lock

One process uses a state area at a time (§2.3). `mount`, `run`,
`serve-history`, `record`, `flush`, and `push`, `restore` and `revert`
when no daemon answers on the control socket, take an
exclusive lock on `<state>/meta/lock` before
opening the volume; a second one exits with status 75 and names the
holder:

```
error: state area /data/state is in use (pid=4711 command=mount since=20261012T110302Z)
```

`--wait-lock 30s` waits for the holder instead. `agentfs status` takes no
lock and changes nothing, so it is safe while mounted, and shows the
holder:

```
lock:      held by pid=4711 command=mount since=20261012T110302Z
```

While a daemon runs, `agentfs push`, `agentfs restore` and `agentfs
revert` are not refused: they go to it over its control socket. `push`
asks `mount`, `run` or `serve-history` to commit and push now (below);
restores and reverts are served by a mount (section 12), while `run` and
`serve-history` answer that they serve none (exit 75). The kernel
releases the lock when the holder dies, even after `kill -9`.

**Pushing now.** `agentfs push [--remote NAME]... [--timeout DURATION]`
against a running daemon commits everything recorded and wakes each
chosen remote's push loop at once, past `pushInterval` and any backoff:

```
$ agentfs --state /data/state push
through the running daemon (pid 4711): 3 record(s) into 1 commit(s)
origin: pushed 1a2b3c4..5d6e7f8
```

It waits up to `--timeout` (default `120s`, 1s to 10m) and exits 0 only
when every chosen remote succeeded in time; a remote still pushing then
(`origin: still pushing after 120s; it continues in the background`)
makes it exit 1, and `agentfs status` shows how that push ended. A remote
the daemon does not know is refused (exit 1, with the configured
remotes). With no daemon running, `push` takes the lock and pushes
directly, as before. In Kubernetes: `kubectl exec -c agentfs <pod> --
agentfs push`.

## 10. Crashes and stale mounts

Every change was journaled before its call returned, so a crash loses
nothing that a workload saw succeed. The exception is the `fsync` class
(databases, [section 14](#14-databases-the-fsync-class)): changes made
since a group's last capture are in the files but not in history, and
are recorded when the database next changes. To record a database that
will not change again, run `agentfs record <path>` with the workload
stopped.

- After the daemon dies, the mount point fails with "Transport endpoint is
  not connected" (`ENOTCONN`). agentfs does not use `auto_unmount`, so the
  empty directory underneath is never exposed to writes nothing would
  record.
- Clear it, then mount again:

  ```sh
  fusermount3 -u -z /workspace     # or, as root: umount -l /workspace
  agentfs --state /data/state mount /data/backing /workspace
  ```

  `agentfs mount` refuses a stale mount point with exactly this advice.
- At start, records in the journal that are not in history yet are
  committed (`N uncommitted record(s) recovered` in the startup line).
- In Kubernetes the shipped sidecar recovers by itself (design §17.1).
  The kubelet restarts the crashed container, which runs with
  `--on-stale-mount recover`: it takes the lock the dead daemon's exit
  released, recovers the journal, mounts FUSE at a private directory
  (`<state>/meta/stack/mnt`) and bind-mounts it over the stale mount.
  Mount propagation carries the new mount into the workload containers
  and subPath binds, so their next calls reach the new daemon without a
  restart. It logs `stacked over a stale mount left by a crashed daemon
  at ...`. Until then the workload's calls fail with `ENOTCONN`, so
  nothing goes unrecorded. At its next stop the sidecar detaches the
  stale layer beneath. Stacking needs `CAP_SYS_ADMIN` (the sidecar is
  privileged); `--on-stale-mount hold` keeps the old behaviour (the
  sidecar waits, NotReady, for `kubectl delete pod`). See the
  [crashed sidecar runbook](runbooks/crashed-sidecar.md) and
  [deployment.md](deployment.md#7-a-crashed-daemon).
- A sidecar that exits cleanly mid-pod (SIGTERM, or the mount was
  unmounted from outside) is restarted by the kubelet and mounts afresh.
  In the gap, the workload's read-only `emptyDir` mount makes writes fail
  with `EROFS` instead of landing in the bare directory. A workload with a
  `subPath` of the mount keeps the old, dead mount (`ENOTCONN`) until it
  restarts: give it the liveness probe from `deploy/sidecar`.

## 11. Kubernetes

Phase 1 runs agentfs as a privileged native sidecar in the workload's pod
(§17.1). The example manifests are in [`deploy/sidecar`](../deploy/sidecar)
and [deployment.md](deployment.md) explains them. In short:

- one `ReadWriteOncePod` PVC mounted once at `/data`, holding
  `backing/` and `state/` as siblings (reflinks fail across mounts), and a
  Deployment with the `Recreate` strategy, so one pod writes the volume;
- an init container, `agentfs init --include /etc/agentfs/config`, safe to
  run at every start;
- the sidecar (`restartPolicy: Always`, privileged) runs
  `agentfs mount /data/backing /agentfs/workspace`: the mount point is a
  subdirectory of an `emptyDir` mounted at `/agentfs` with
  `Bidirectional` propagation; workload containers mount the `emptyDir`
  read-only with `HostToContainer` and use `/agentfs/workspace` (the FUSE
  mount in it stays writable; the bare directory, while no daemon is
  mounted, does not);
- `agentfs probe --allow-stale /agentfs/workspace` is its startup probe
  (workloads start once the mount is live), `agentfs probe` its readiness
  probe, and there is no liveness probe; `AGENTFS_ON_STALE_MOUNT=recover`
  makes a sidecar restarted after a crash mount over the stale mount
  (§10);
- settings and remotes come from a ConfigMap fragment (`include.path`),
  the rules from the same ConfigMap mounted over `info/exclude` and
  `info/attributes`, credentials from a Secret outside the backing tree
  (rotation needs no restart), and the pod flags from the downward API;
- `terminationGracePeriodSeconds: 60`, above `--unmount-grace` plus
  `--push-grace`.

## 12. Recovery and undo

Bring files back as they were in a commit, or undo a commit (§15). Both
commands write the tree and record the result as **one commit**
(`Agentfs-Trigger: restore`, plus `Agentfs-Restore: <commit>` or
`Agentfs-Revert: <commit>`), authored by whoever ran them.

```sh
agentfs --state /data/state restore <path> <commit>   # a file, symlink, directory, or `.`
agentfs --state /data/state revert <commit>
```

| Flag | Meaning |
|---|---|
| `--dry-run` | Print the plan (`write`, `remove`, `symlink` or `mode` per path) and write nothing. LFS objects are not downloaded: those missing from the cache are counted |
| `--force` | `revert`: also revert paths changed again since the commit (the later versions stay in history). Both: write files a workload holds open for writing through the mount. Never overrides a change that is not in history yet |
| `--root DIR` | The backing tree; only needed when no mount is running |
| `--wait-lock D` | With no mount running, wait for the state-area lock |
| pod flags | As for the other commands (section 4) |

**Which mode runs:**

- **A mount is running**: the command connects to
  `<state>/meta/control/agentfs.sock` (run it as root or as the daemon's
  user) and the daemon does the work: it waits until history has caught
  up with the tree, checks against its mount point, and writes through
  its own mount, so the workload sees the result at once. In Kubernetes,
  `kubectl exec -c agentfs <pod> -- agentfs restore ...` in the sidecar.
- **Nothing is running**: the command takes the state-area lock and
  writes the backing tree (`--root`) itself.
- **`agentfs run` or `serve-history` runs**: it answers that it serves no
  restores; exit 75.
- **`record`, `flush` or a direct `push` holds the state area**: exit 75. One restore runs at a time per mount; a second one also exits
  75.

Exit status: `0` done (or `already matches: nothing to do`), `1` refused
(a one-line reason and a hint) or failed, `75` busy. A request sent to
the control socket by hand must name its `path` (`"."` for the whole
tree); one without is refused.

Runbooks, with the full procedures:

- [recovery-and-undo.md](runbooks/recovery-and-undo.md): finding a change,
  pausing the workload, restore and revert, every refusal and its fix,
  undoing a restore, plain-git alternatives;
- [divergence.md](runbooks/divergence.md): a remote whose branch someone
  else moved (`Diverged`);
- [history-rewrite.md](runbooks/history-rewrite.md): removing a recorded
  secret from the state area and every remote;
- [stuck-pushes.md](runbooks/stuck-pushes.md): pushes that keep failing;
- [patch-queue.md](runbooks/patch-queue.md): queued patches, a blocked
  queue, submitting with plain git;
- [history-service-down.md](runbooks/history-service-down.md): fronts
  failing with `EIO` because the history service is down, hung or
  refusing them (§16).

**Patches** (§12) are the other way to change the tree from outside:
producers queue them on the remote (`agentfs patch submit`), and every
mount applies them, in order, after journal recovery and before it
serves. `agentfs status` shows a `patches:` line, `agentfs patch status`
the whole queue, and `agentfs patch apply` applies it with the volume
stopped. A patch that cannot be applied blocks the queue (`PatchBlocked`)
without stopping the mount.

## 13. Large files (LFS)

Git LFS (design §7) keeps large content out of git history: history holds
a pointer, the content goes to an LFS server. It is off by default.

- **Enabling.** Set `agentfs.lfsThreshold` (for example `8m`), or add
  `filter=lfs` or `agentfs.store=lfs` lines to `<state>/git/info/attributes`
  (`*.psd filter=lfs`), then restart the mount. Every remote then needs an
  LFS endpoint: the one derived from an `https` remote's URL (GitHub,
  GitLab, Gitea), an explicit `remote.<name>.lfsurl`, or nothing at all for
  local and `file://` remotes. SSH remotes need `lfsurl`. Endpoints and
  credentials are in the
  [configuration reference](configuration.md#git-lfs).
- **What history looks like.** LFS files are pointer blobs; the root
  `.gitattributes` in history ends with an agentfs block of
  `filter=lfs diff=lfs merge=lfs -text` lines, one per LFS path. The
  workload's own `.gitattributes` lines are kept above it; the backing
  tree's file is never changed, so the workload never sees the block.
  A pointer is never committed without its line. If the block cannot be
  written (a `.gitattributes` that fits `agentfsMaxBlobSize` alone but not
  with the block, for example), that commit stores its LFS files as
  ordinary blobs and, if needed, drops the `.gitattributes` change with a
  `PathRefused` alarm (stage `commit`); commits never stop for it. A
  directory named `.gitattributes` or `.gitmodules` is never recorded.
- **Cloning.** A clone needs git-lfs:

  ```sh
  GIT_LFS_SKIP_SMUDGE=1 git clone -b agentfs/<volume> https://github.com/acme/agent-history.git
  cd agent-history
  git lfs install --local
  git lfs pull           # downloads the content of the checked-out commit
  ```

  For a separate LFS server, point the clone at it first
  (`git config lfs.url https://lfs.example.com/acme/agent-history`).
  Without git-lfs, LFS files show as pointer text. A `.lfsconfig` the
  workload wrote is in history and steers the cloner's git-lfs; agentfs
  itself never reads it.
- **Pushing.** Each push first uploads the objects of the commits the
  remote lacks; only then are refs pushed. Failures show as
  `failed (<kind>, stage lfs)` and in `agentfs status` (`last error (…,
  stage lfs)`); see [stuck-pushes.md](runbooks/stuck-pushes.md#lfs-stage-failures).
- **The cache.** Objects are kept in `<state>/lfs/objects` from commit
  time on, and **never pruned**, locally or on the server: budget the
  state area for every version of every LFS file, and watch
  `agentfs.state.bytes{part="lfs"}`. A push needs every object its
  commits reference in the cache; a lost object blocks that remote's
  pushes with a `local` failure.
- **Restores** write LFS content from the cache, or download it from the
  remotes (in name order) when the cache lacks it, before writing
  anything. `--dry-run` downloads nothing: it reports how many objects
  (and bytes) are missing from the cache and would be downloaded, without
  checking that a remote has them.
- **Status** prints the threshold and each remote's LFS endpoint, or why
  it has none (`lfs: none (…)`).

## 14. Databases: the fsync class

A database is held open and changed in place, so "recorded at close" could
mean never, and a copy of the main file alone, taken while the writer is
busy, may not open. The `fsync` class (design §8) records such files as
whole, consistent groups instead.

- **Opting in.** Add a line to the operator's
  `<state>/git/info/attributes` (in Kubernetes, the ConfigMap's
  `attributes`), then restart the mount:

  ```text
  *.db* agentfs.commit=fsync agentfs.group=sqlite
  ```

  `*.db*` matters: it puts the `-wal` and `-journal` files in the class
  too. Only the operator's file counts; a tracked `.gitattributes` cannot
  opt in. A bad value (`agentfs.commit=yes`, `agentfs.coalesce=100ms`)
  stops the volume from opening, naming the line, rather than being
  ignored.
- **Groups.** `app.db`, `app.db-wal` and `app.db-journal` in one directory
  with the same `agentfs.group` are one group, captured together; two
  databases are two groups. A class path without `agentfs.group` is a
  group of its own. `-shm` files are never recorded (SQLite rebuilds them).
- **When.** A group is captured when a member is `fsync`ed, at most once
  per `agentfs.coalesce` (default `30s`, at least `1s`, set per pattern:
  `agentfs.coalesce=5m`); by a timer one interval after its first change
  since the last capture, so a database whose commits never `fsync`
  (SQLite WAL with `synchronous=NORMAL`) is still recorded; when the
  group's last open handle is closed or goes away; and at unmount
  (`unmount: N database group(s) captured`). Closing one member while
  another stays open (SQLite's `-journal` in DELETE mode, every
  transaction) records nothing by itself. Renames and deletes of a
  member are folded into the next capture.
- **The guarantee is weaker on purpose.** A change to a database is in
  history at most one interval late (plus the commit window), not before
  the call returns. An `fsync` returning does not mean it is recorded.
  If the daemon or node crashes, the changes since each group's last
  capture are not in history until the database next changes (section
  10).
- **What a capture costs the workload.** Database files never use kernel
  passthrough: every write goes through the daemon. During a capture,
  writes to the group wait while its files are copied (a reflink on XFS,
  a full copy on ext4), and so do renames, deletes and creates *anywhere
  on the mount*, since the capture holds the mount's name lock. Reads and
  writes of other files do not wait. A large database on ext4 can stall a
  rename-heavy workload for the copy time once per interval. Choose
  `agentfs.coalesce` with the copy time and the history size in mind.
- **Every capture is one point in time,** as a crash would leave the
  files, which SQLite recovers from, and is recorded whole or not at all:
  if one member is refused (`agentfs.maxSize`, a remote's blob limit) or
  a path rule leaves a member out (`info/exclude` matching `*-wal`:
  `group_member_skipped`), the group is skipped with a `PathRefused` alarm
  and captured again after its next change. A member the daemon cannot
  read (other than gone) fails the capture with `RecordFailed`, and the
  timer retries. Restore a version with the
  [runbook](runbooks/recovery-and-undo.md#7-restoring-a-database).
- **Size.** Every capture stores each changed member in full.
  `agentfs.maxSize=512m` (any path, operator attributes) refuses larger
  content when it is captured; `agentfs.store=lfs` or `filter=lfs` sends
  the versions to LFS (section 13) instead of git. The history is
  repacked every `agentfs.repackInterval` (default `1h`) with
  `git repack --geometric=2`, which stores versions of a database as
  deltas. A round with nothing loose is skipped. A repack is skipped
  while a push runs; a failing one raises `RepackFailed` and counts in
  `agentfs.repack.failures`. Watch `agentfs.state.bytes{part="git"}`.
- **Repack memory.** git runs with one thread and bounded windows
  (`pack.windowMemory=64m`, `pack.deltaCacheSize=16m`,
  `core.bigFileThreshold=64m`, `core.packedGitLimit=128m`): plan for about
  256 MiB more than the daemon's usual use, plus the largest object, in
  the sidecar's memory limit (the example manifests set `512Mi`), or set
  `agentfs.repackInterval = off`. Commits wait while a repack runs;
  stopping the daemon kills it.
- **`agentfs record`** of a member records its whole group; run it with
  the workload stopped, since it does not hold writes.
- **Restore and revert** of a member restore its whole group from one
  commit (the database with its `-wal` or `-journal`), in one operation.
  Stop the workload first (see the runbook).
- **Limits.** A hard link to a member under a name the pattern does not
  match is an ordinary file.

## 15. Troubleshooting

- **`passthrough: off (...)` in the startup line.** The reason is given:
  the kernel is older than 6.9, the daemon lacks `CAP_SYS_ADMIN` or runs in
  a user namespace, or `--passthrough off` was set. Everything still works,
  through the daemon. `--passthrough on` turns any of these into a mount
  failure.
- **`passthrough refused for inode N` in the log.** Registering one file's
  backing file failed (a backing tree on overlayfs or another stacked
  filesystem, or too many open files). That file is served through the
  daemon while it is open; every other file keeps passthrough. The first
  ten are logged. With `--passthrough on` the open fails with `EIO` and the
  mount stops with exit status 1 instead.
- **`close` fails with `EIO`.** Recording failed, usually because the state
  area is full or unwritable. A `RecordFailed` alarm says why. The file is
  still changed in the backing tree and stays marked as changed, so its next
  close records it. A rename, delete or link whose record failed also
  returns `EIO` after it happened; the sweep records the affected paths once
  recording works again (§3.5). Paths still queued at shutdown are saved in
  `<state>/meta/repairs` and recorded by the next mount before it serves
  anything ("N path(s) a previous mount could not record: ..." at start).
  If that file could not be written either, the shutdown message lists the
  paths: history differs from the backing tree there until each is changed
  again, or recorded by hand with `agentfs --root <backing> record <path>`. Many programs ignore errors from `close`, so
  watch the alarms.
- **Alarms** are printed to stderr and exported as telemetry logs (see the
  README): `PathRefused`, `RecordFailed`, `CommitFailed`, `PushFailed`,
  `Diverged`, `RemoteMisconfigured`, `PatchBlocked`, `PatchFetchFailed`,
  `PatchIgnored` (a warning: a stale inbox entry that is never applied).
- **`patches: blocked at N (<reason>)`** at mount: patch N could not be
  applied; the mount serves without it and the patches after it. The
  reason and its fix are in
  [patch-queue.md](runbooks/patch-queue.md#4-a-blocked-queue).
- **`patches: the queue could not run`** at mount (and `blocked at 0
  (failed)` in `agentfs status`): the patch phase failed as a whole, so no
  patch was applied; the message says why (the LFS cache, the backing
  tree, a locked ref). The mount serves anyway; fix the cause and restart.
- **`failed (<kind>, stage queue)`** from `push`: the patch remote's
  queue refs (status, applied, inbox) were not pushed; the branch push is
  unaffected and the queue push is retried with the next push.
- **`failed (<kind>, stage lfs)`** from `push` or in a `PushFailed`
  alarm: uploading LFS objects failed, so no ref was pushed. `auth`: the
  LFS credentials (`agentfsLfsCredentials`, or `agentfsCredentials` for an
  endpoint on the git host); `local`: no endpoint (an SSH remote without
  `lfsurl`), no `https` feature, or an object missing from
  `<state>/lfs/objects`. See
  [stuck-pushes.md](runbooks/stuck-pushes.md#lfs-stage-failures).
- **A path is not in history.** Check, in order: is it under a `.git`
  directory or a nested repository (`agentfs.nested`)? Excluded by
  `info/exclude` or `-agentfs`? Refused (an alarm says so)? Still open (it is
  recorded at close, or by the next sweep)?
- **A hard link made before the mount** is only known by its names once
  each has been looked up through the mount; a change through one name is
  recorded at the names the mount has seen.
- **Exit status 75** from `flush`, `record` or `run` (or a `push` that
  found no daemon to ask): the volume is mounted (or another command is
  running). Use `--wait-lock`, or stop the
  mount first. From `restore` or `revert`: `agentfs run` (or another
  command) holds the state area, or the mount is already running a
  restore; retry when it is done.
- **`restore refused: ...`** (exit 1): nothing was written. The message
  says why and the `hint:` line what to do; the refusals and their fixes
  are listed in [recovery-and-undo.md](runbooks/recovery-and-undo.md).
  `<path> differs from history` means the tree holds a change that is
  not recorded yet; it is never overwritten, even with `--force`.
- **`warning: telemetry: <signal> export failed: ...`** on stderr: the
  collector is down, refused the export (an HTTP error) or timed out.
  Telemetry never stops a command. The warning repeats at most once a
  minute while exports keep failing (with the count), a
  `telemetry: <signal> export recovered` line follows when they work again,
  and each failure counts in `agentfs.telemetry.export_failures{signal}`
  (exported once metrics get through).

## 16. Several mounts of one volume (history service)

A volume can be mounted more than once on one node, for example by two
pods that share a backing tree, with one history (design §14). One
process, `agentfs serve-history`, is the volume's history service: it
holds the state-area lock and is the only writer of the journal and the
branch, and runs the commit and push loops. Every mount is a **front**,
`agentfs mount --history-service ADDR`: it serves FUSE for its pod, and at
each change boundary applies the rules and checks, freezes the content
into `pending/` and sends the record to the service, returning only once
the service has journaled it. Without `--history-service`, `mount` stays
the single-process mount of the rest of this guide.

**Requirements.** The service and every front on the same node, with the
same state area and backing tree (the socket is a Unix socket in the
state area). The fronts run as root or as the service's user: the
service refuses any other peer. Cross-node mounts are not built yet.

**On Kubernetes**, the CSI driver runs this for you: a service per staged
volume, a front per pod with that pod's identity, restarts in the same
slot, and recovery of the mounts. See [csi.md](csi.md) and
[runbooks/csi-recovery.md](runbooks/csi-recovery.md).

**Start order.** Start the service first; a front waits up to
`--history-wait` (30 s) for it, then exits 1 with nothing mounted.

```sh
agentfs --state /data/state --root /data/backing serve-history
AGENTFS_POD=pod-a agentfs --state /data/state mount \
  --history-service /data/state/meta/history/history.sock /data/backing /mnt/a
AGENTFS_POD=pod-b agentfs --state /data/state mount \
  --history-service /data/state/meta/history/history.sock /data/backing /mnt/b
```

The service opens the volume (recovering its journal), applies the patch
queue only if no front is running (patches write the backing tree, which
running fronts serve; it says so otherwise), starts the commit and push
loops, then listens and prints `serving history for volume ... at ...`.
A front's startup line names the service and its slot. `agentfs record
--history-service ADDR PATH...` records paths through the service the
same way, without a mount.

**What each process holds.**

| Process | Holds |
|---|---|
| `serve-history` | `meta/lock` (holder `command=serve-history`), `meta/history/history.sock` (directory 0700, socket 0600), `meta/history/address`, the journal, the branch, the push state |
| a front | `meta/fronts/slot-N.lock`, an `flock` lease of pending-id slot N (1 to 1023), its frozen copies in `pending/` and its repair queue `meta/fronts/slot-N.repairs` |

**Commits.** A commit holds the changes of one front only. Its committer
is rendered from the identity templates (§9 of the design,
[configuration](configuration.md#agentfs-identity)) with that front's pod
flags (`AGENTFS_POD`, `AGENTFS_REPLICA`, ...), and it carries that pod's
`Agentfs-Pod`, `Agentfs-Pod-Uid`, `Agentfs-Node` and `Agentfs-Namespace`
trailers, also after a crash and replay. A front whose pod flags do not
render the templates is refused when it connects. History follows the
order the service received the boundaries; in the backing tree, the last
writer wins, as without agentfs.

**`agentfs status`** shows the service and the fronts:

```text
lock:      held by pid=4242 command=serve-history since=20261012T100000Z
history:   serving fronts at unix:/data/state/meta/history/history.sock
fronts:    slot 1: pid=4300 command=mount pod=pod-a replica=pod-a since=20261012T100005Z
fronts:    slot 2: pid=4301 command=mount pod=pod-b replica=pod-b since=20261012T100005Z
```

A `repairs:` line names a slot whose repair queue waits for a front.

**When the service is down.** Every change boundary through a front fails
with `EIO` (a namespace operation with its own error), raises
`RecordFailed`, and the first one raises `HistoryServiceUnavailable`; a
service that does not answer within `--history-timeout` fails it the same
way. Reads and writes themselves still work. Restart the service: the
fronts reconnect at their next boundary (printing `history service ... is
reachable again`), and their repair queue, release and sweep record the
final state of what failed. States in between are lost. See
[runbooks/history-service-down.md](runbooks/history-service-down.md).
Metrics: `agentfs.front.round_trip`, `agentfs.front.service_failures` and,
on the service, `agentfs.history_service.fronts`.

**Crashes.** A front that dies leaves a stale mount, handled as in §10
(its lease is released by the kernel; its unsent frozen copies are
removed when a front next leases its slot). A service that dies is restarted the
same way it was started: it recovers its journal, commits what fronts had
journaled with their own committers, and replaces the stale socket.
A front started in place of a dead one should pass `--history-slot N`
(`AGENTFS_HISTORY_SLOT`) with the dead front's slot (from `agentfs
status` before it died, or its logs): it then leases that slot when it
is free, and drains the slot's repair queue as the same pod. Without
it, or when the slot is taken, a front leases the lowest free slot.

**Stopping.** Stop the fronts first (each unmounts as `mount` does, but
leaves the commit to the service), then the service: on SIGINT or SIGTERM
it stops accepting, waits up to `--shutdown-grace` for fronts' requests,
makes the final commit and then, as `mount` and `run` do, pushes it: every
remote that lacks commits gets one more attempt, all within `--push-grace`
(pushes in progress included). The CSI driver stops the service after the volume's
last pod, and nothing else would push that commit. A push still running
at the deadline is abandoned, and a failed one leaves the commits for the
next start or an `agentfs push`.

**Limits.**

- No `restore` or `revert` while the service runs: they exit 75. Stop the
  fronts and the service, then restore directly or through a
  single-process mount.
- POSIX and BSD locks are kept by each mount's kernel and are not shared
  between fronts. A database or any lock-coordinated file must be written
  through one front only, and the `fsync` class (§14) is per front.
- Patches are applied only when the service starts with no front
  running.
- Two fronts changing one path at nearly the same instant are ordered by
  freeze stamps: the service skips a write frozen before a state it
  already journaled ("superseded"; `agentfs.history_service.superseded`),
  so history ends on the newer state. Two states within one tick of the
  filesystem's timestamp clock, of the same size and mtime, cannot be
  ordered and are journaled in arrival order, as before.
- A file open in one front that another front renames or removes: the
  first front's later close records it under its current name, or not
  at all once it is removed, so history never gets back a path the tree
  no longer has. A record that reaches the service after its path left
  the tree is skipped ("no longer in the backing tree").
- The rules and settings are read when each process starts: after a
  change, restart the fronts and the service.
- Frozen copies a front made but never got journaled (it died between
  freeze and send, or the service died while reading the request) stay
  in `pending/` until a front next leases that slot: the service then
  removes the slot's copies older than the new lease that no journaled
  record references. A front that keeps running keeps such copies (each
  as large as the file it froze) until it restarts. A slot's repair
  queue is drained only by the next front in that slot, which commits
  each entry as the pod that made the change (files written by older
  versions name no pod: their entries are committed as the new front's).
- The service serves at most 1024 connections at once (one thread each).
  A front holds about as many as it has boundaries in flight at once
  (at most its FUSE threads plus its sweep, 17 by default on a 16-CPU
  node) and drops them after 20 s idle; the service closes any idle for
  60 s. So about 60 fronts can all be busy at once; past that, a
  boundary fails with `EIO` and `RecordFailed` ("history service ... is
  busy"), without the `HistoryServiceUnavailable` alarm.
- A front checks that the socket is served by root or by its own user and
  refuses anything else, so a socket in a directory other users can
  write cannot be answered by them. Keep `--listen` in a directory only
  the service's user can write (the default `meta/history/` is 0700).
