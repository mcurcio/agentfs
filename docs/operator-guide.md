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
  other containers see, a privileged sidecar: see §11 below).
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
  the same whatever the file's size. On ext4 it is a copy (§3.2).
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
| a remote's `agentfsCredentials` file is under the backing tree or the mount point | the workload could read the secret. This is an error, not an alarm |
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

`agentfs mount BACKING MOUNTPOINT`:

| Flag | Environment | Default | Meaning |
|---|---|---|---|
| `--passthrough auto\|on\|off` | `AGENTFS_PASSTHROUGH` | `auto` | Kernel passthrough: `auto` uses it when the kernel (6.9+) and privileges (`CAP_SYS_ADMIN`, see §1) allow, `on` refuses to mount without it and stops the mount (exit 1) if a file cannot be given a backing file later, `off` never uses it |
| `--threads N` | `AGENTFS_THREADS` | CPUs, 2 to 16 | FUSE request threads |
| `--no-allow-other` | | | Only the mounting user can use the mount |
| `--unmount-grace DURATION` | | `10s` | At shutdown, how long to wait for open files on a busy mount after detaching it |
| `--push-grace DURATION` | | `5s` | At shutdown, how long to wait for pushes in progress |

Pod flags (`mount`, `run`, `record`, `flush`, `push`), from the downward
API in Kubernetes: `--namespace` (`AGENTFS_NAMESPACE`), `--pod`
(`AGENTFS_POD`), `--pod-uid` (`AGENTFS_POD_UID`), `--node`
(`AGENTFS_NODE`), `--replica` (`AGENTFS_REPLICA`). They fill the commit
trailers and the committer identity (§9) and the telemetry resource.

Telemetry is configured with the standard `OTEL_*` variables; see the
[README](../README.md#telemetry).

## 5. Settings that affect the mount

Settings live in the history repository's own config,
`<state>/git/config` (§4.3). The mount reads them when it starts; changing
them takes a remount.

| Setting | Default | Effect |
|---|---|---|
| `agentfs.commitWindow` | `200ms` | Records this close in time become one commit |
| `agentfs.pushInterval` | `60s` | How often each remote is pushed |
| `agentfs.sweepInterval` | `30s` | How often files held open are checked and recorded, and failed records retried (minimum `1s`) |
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

## 8. Lifecycle

- **Start.** The daemon takes the state-area lock, checks the layout,
  recovers the journal (records made before a crash are committed), starts
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
  4. makes the final commit and waits up to `--push-grace` for pushes in
     progress;
  5. flushes telemetry and releases the lock.
  A second signal exits at once.
- **Unmounted from outside** (`umount`, `fusermount3 -u`): the daemon
  notices, runs the same shutdown, and exits 0.
- **Exit status:** `0` clean; `1` an error, or the final commit failed (the
  records stay journaled and are committed at the next start); `75` the
  state area is in use by another process; `130` a second signal.

In Kubernetes, give the pod a `terminationGracePeriodSeconds` longer than
`--unmount-grace` plus `--push-grace` plus a few seconds, so the final
commit always happens. Pushing never delays shutdown beyond
`--push-grace`; unpushed commits go out at the next start.

## 9. The state-area lock

One process uses a state area at a time (§2.3). `mount`, `run`, `record`,
`flush`, `push`, and `restore` and `revert` when no mount runs, take an
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

While mounted, `agentfs push` is therefore refused: the mount's own push
loop pushes every `pushInterval`. `agentfs restore` and `agentfs revert`
are not refused: they go to the mount over its control socket (section 12).
While `agentfs run` holds the lock they exit 75: `run` serves no
restores. The kernel releases the lock when the holder dies, even after
`kill -9`.

## 10. Crashes and stale mounts

Every change was journaled before its call returned, so a crash loses
nothing that a workload saw succeed.

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
- In Kubernetes, phase 1 accepts restarting the pod as the recovery: the
  sidecar restarts, and workload containers that held the old mount must
  restart too (§17.1).

## 11. Kubernetes sidecar sketch

Phase 1 runs agentfs as a native sidecar in the workload's pod (§17.1):

```yaml
spec:
  terminationGracePeriodSeconds: 30
  initContainers:
    - name: agentfs
      image: ghcr.io/acme/agentfs:<tag>
      restartPolicy: Always            # a native sidecar
      args: [mount, /data/backing, /workspace]
      env:
        - { name: AGENTFS_STATE, value: /data/state }
        - { name: AGENTFS_POD, valueFrom: { fieldRef: { fieldPath: metadata.name } } }
        - { name: AGENTFS_POD_UID, valueFrom: { fieldRef: { fieldPath: metadata.uid } } }
        - { name: AGENTFS_NAMESPACE, valueFrom: { fieldRef: { fieldPath: metadata.namespace } } }
        - { name: AGENTFS_NODE, valueFrom: { fieldRef: { fieldPath: spec.nodeName } } }
      securityContext:
        privileged: true               # /dev/fuse, mount(2), passthrough
      volumeMounts:
        - { name: data, mountPath: /data }
        - { name: workspace, mountPath: /workspace, mountPropagation: Bidirectional }
        - { name: credentials, mountPath: /run/secrets/agentfs, readOnly: true }
  containers:
    - name: workload
      volumeMounts:
        - { name: workspace, mountPath: /workspace, mountPropagation: HostToContainer }
  volumes:
    - name: data
      persistentVolumeClaim: { claimName: hermes-data }   # ReadWriteOncePod: backing/ and state/
    - name: workspace
      emptyDir: {}
    - name: credentials
      secret: { secretName: agentfs-origin }
```

- The backing tree and the state area live on one `ReadWriteOncePod` PVC,
  as siblings, so the state area is never under the backing tree.
- The mount point is an `emptyDir` shared with Bidirectional propagation
  from the sidecar and HostToContainer in the workload.
- Credentials come from a Secret volume outside the backing tree; rotated
  Secrets are picked up without a restart (§11.1).
- Workload containers should wait for the mount (a startup probe on the
  sidecar that checks `/proc/mounts`, or a check in the workload's
  entrypoint).

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
| `--dry-run` | Print the plan (`write`, `remove`, `symlink` or `mode` per path) and write nothing |
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
- **`agentfs run`, `record`, `flush` or `push` holds the state area**:
  exit 75. One restore runs at a time per mount; a second one also exits
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
- [stuck-pushes.md](runbooks/stuck-pushes.md): pushes that keep failing.

## 13. Troubleshooting

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
  `Diverged`, `RemoteMisconfigured`.
- **A path is not in history.** Check, in order: is it under a `.git`
  directory or a nested repository (`agentfs.nested`)? Excluded by
  `info/exclude` or `-agentfs`? Refused (an alarm says so)? Still open (it is
  recorded at close, or by the next sweep)?
- **A hard link made before the mount** is only known by its names once
  each has been looked up through the mount; a change through one name is
  recorded at the names the mount has seen.
- **Exit status 75** from `push`, `flush`, `record` or `run`: the volume is
  mounted (or another command is running). Use `--wait-lock`, or stop the
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
