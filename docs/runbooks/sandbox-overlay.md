# Runbook: the sandbox overlay

The sandbox overlay is a second mount point of a volume, served by the
same daemon, that shows sandboxes only the files the operator's rules
would record and keeps their other new files in a private tmp layer.
Design: [§2.4](../design.md#24-the-sandbox-overlay), its limits in
[§1.1](../design.md#11-threat-model); deployment:
[deployment.md §12](../deployment.md#12-the-sandbox-overlay); flags:
[configuration](../configuration.md#flags-and-environment).

## Boot refusals

Each of these stops `agentfs mount` before anything is mounted, with
exit 1 (exit 2 for a usage error). The sidecar container restarts with
the same message until the cause is fixed.

| Message | Cause | Fix |
|---|---|---|
| `the sandbox overlay needs agentfs.respectTreeRules = false: rule files read from the tree could be edited through the overlay` | The volume reads rule files (`.gitignore`, `.gitattributes`) from its tree; a sandbox could edit the rules behind its own view | Set `agentfs.respectTreeRules = false` and keep the rules in the operator's `info/exclude` and `info/attributes` (design §4.1) |
| `the sandbox overlay's mount point <A> and the mount point <B> are the same directory or one is inside the other; give each its own directory` | `--sandbox-overlay` is the mount point, or nested with it | Give the overlay a directory outside the mount point, in another `emptyDir` |
| `the sandbox overlay's mount point <A> and the mount point <B> are both in <P>: a container given that directory would reach both; give the sandbox overlay a directory of its own` | The two mount points share a parent directory (both in one `emptyDir`) | Mount a second `emptyDir` for the overlay (`/agentfs-sandbox`, mount point `/agentfs-sandbox/overlay`) |
| `the sandbox overlay's tmp layer <P> is on another filesystem than the backing tree (device <S>, the tree's <T>): ...; keep the state area and the backing tree on one filesystem` | The state area and the backing tree are on different filesystems, so a tmp file cannot be renamed into the tree atomically | Put both on one filesystem: one claim mounted once, with `backing/` and `state/` as siblings (deployment.md §3) |
| A usage error naming `--history-service` (exit 2) | The overlay was asked for on a front | The overlay is served only by a single-process `mount`, not on fronts or CSI volumes (design §19.2 item 13) |
| `--sandbox-owner <U>:<G>: the daemon (uid <E>) can act only as itself without CAP_SETUID, CAP_SETGID, CAP_CHOWN and CAP_FOWNER (run it as root, or as the owner)` | A non-root daemon cannot act as another user | Run the sidecar as root (the shipped one is), or set the owner to the daemon's own uid |
| `creating the sandbox overlay's mount point <P>: ...` | The overlay's parent directory is missing; only one level is created | Mount the `emptyDir` at the parent (`/agentfs-sandbox`) |
| `the sandbox overlay's mount point <P> is a stale FUSE mount (transport endpoint is not connected), probably left by a daemon that died; ...` | A crash left the overlay's mount dead and the sidecar runs with `--on-stale-mount refuse` | Use `recover` (the shipped setting) or `hold`; see [a crashed sidecar](crashed-sidecar.md) |
| `the sandbox overlay could not be mounted after the mount point was: <error>; the mount point was unmounted as at a stop` | Mounting the overlay failed after the main mount was served; the main mount was stopped cleanly | Fix the error it names, then let the sidecar restart |

`--sandbox-overlay` and `--sandbox-owner` come in pairs: one without the
other is a usage error (exit 2).

## A sandbox cannot save

A sandbox's tool fails to write a file through the overlay, and
`agentfs.sandbox_overlay.refusals` counts it by `reason`
([telemetry](../telemetry.md#signals)). The overlay offers only what
keeps it a boundary for names (design §1.1), so some ways of saving fail
by design:

- **`EACCES` (Permission denied) moving a recorded file out**
  (`reason="not_recordable"`). A tool that renames the original to a
  backup name the rules skip (`config.yaml` to `config.yaml~` or
  `config.bak`) before writing the new one is refused: a recorded file
  never moves to a name that is not recorded. Copying it still works.
  Configure the tool to write a temp file and rename it over the
  original (the usual pattern), or to copy for its backup.
- **`EXDEV` (Invalid cross-device link) moving a directory**
  (`reason="cross_device"`). No directory moves through the overlay.
  `mv` falls back to copy and delete, file by file; it fails to remove
  the source when the directory holds hidden files ("Directory not
  empty"), which stay where they were.
- **`git init` and `git clone` fail.** A `.git` directory is never a
  path the rules can record, so it cannot be created through the
  overlay. Work on repositories through the main mount, or outside the
  volume.
- **WAL databases fail.** SQLite in WAL mode needs a `-shm` file, which
  is never recordable. Use another journal mode for databases a sandbox
  writes through the overlay, or keep them off the volume.
- **`EACCES` for a hard link, a symlink or a special file**
  (`reason="kind"`), and **`EPERM` for `chown` or set-id bits**
  (`reason="ownership"`): never offered through the overlay.
- **`EACCES` everywhere for one sandbox user**: every operation is done
  as the view owner (`--sandbox-owner`), whatever user the sandbox runs
  as. Check that the view owner may write the file and search its
  directories on the backing tree; the sandbox process's own groups
  never count.
- **"No such file" for a file the main container sees**: it is not
  recordable, so the overlay hides it. Allowlist it in the operator's
  rules if sandboxes should edit it (a change of rules needs a pod
  restart, design §4.1).

## Tmp files gone after a restart

By design. A file a sandbox creates under a name the rules skip lives in
the tmp layer (`<state>/tmp-layer`), which only the overlay sees, which
is never recorded, and which is wiped when the daemon starts and when it
stops. A sidecar restart, a crash and a pod restart all discard it. A
file that should survive must have a recordable name, or be renamed onto
one before the sidecar stops. A wipe that fails at stop prints a warning
line (`the sandbox overlay's tmp layer was not wiped: ...`); the next
start wipes it.

Tmp files count in `agentfs.state.bytes{part="tmp_layer"}`. Nothing
limits their size, so a sandbox can fill the state area's disk with
them. A tmp file whose directory the main container removed stays in
the store, reachable only through handles still open on it, until the
next wipe.

## The overlay was unmounted from outside and both mounts stopped

The daemon's log says `the sandbox overlay at <path> was unmounted from
outside; stopping both mount points` (an unmount of the main mount says
`unmounted from outside`). By design, either mount point ending stops
the daemon, which unmounts the other, commits and pushes, and the
kubelet restarts the sidecar into fresh mounts. Workload containers and
sandboxes see the gap as in [a crashed sidecar](crashed-sidecar.md)'s
"clean exit" case. Find what unmounted it (a node agent, a sandbox's
container runtime cleaning up its binds with recursive unmounts) and
stop it.

## Sandboxes still fail with ENOTCONN after a recovery

The sidecar recovered from a crash and the pod is Ready, but a sandbox
started before the crash still gets "Transport endpoint is not
connected". Its container runtime bound the overlay's path with private
propagation (the default), so the new mount stacked over the dead one
never reached it. Restart the sandbox. To avoid this, have the runtime
bind with `rslave` propagation (Docker: `--mount
type=bind,src=...,dst=...,bind-propagation=rslave`). Not tested.
