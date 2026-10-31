# Runbook: a crashed agentfs sidecar

The agentfs daemon in a pod's sidecar died (a crash, an OOM kill, `kill
-9`). Every change the workload saw succeed was journaled before its call
returned, so nothing is lost. The shipped sidecar recovers by itself:
the kubelet restarts the container, and it mounts over the dead mount
(`AGENTFS_ON_STALE_MOUNT=recover`). This runbook is for checking that
recovery, and for the cases where it does not happen. Design:
[§17.1](../design.md#171-layer-1-core-and-daemon-rust); deployment:
[deployment.md](../deployment.md#7-a-crashed-daemon).

## What automatic recovery looks like

1. For a few seconds the workload's file calls under the mount fail with
   **"Transport endpoint is not connected"** (`ENOTCONN`). This is
   deliberate: agentfs never exposes the empty directory beneath a dead
   mount, where writes would go unrecorded.
2. The kubelet restarts the `agentfs` container (`RESTARTS` goes up by
   one). The new daemon takes the state-area lock, recovers the journal,
   and stacks a new mount over the dead one. Its log shows:

   ```
   stacked over a stale mount left by a crashed daemon at /agentfs/workspace (1 stale layer(s) below): the workload's copies of the mount now reach this daemon
   mounted /data/backing at /agentfs/workspace (volume ..., 3 uncommitted record(s) recovered, ...)
   ```

3. The pod is Ready again. The workload containers were not restarted:
   their next calls reach the new daemon.
4. When that sidecar later stops (pod deletion, a rollout), it detaches
   the dead layer beneath its own mount (`detached 1 stale layer(s) at
   /agentfs/workspace`).

**With the sandbox overlay** ([deployment.md](../deployment.md#12-the-sandbox-overlay))
the crash leaves both mount points dead, and the same steps cover both.
Calls through the overlay fail with `ENOTCONN` too, the new daemon
stacks over each mount point (the overlay through
`<state>/meta/stack/overlay`), and its log has a second line:

```
stacked the sandbox overlay over a stale mount left by a crashed daemon at /agentfs-sandbox/overlay (1 stale layer(s) below): the sandboxes' copies of it now reach this daemon
```

At its stop it detaches the dead layers under both. Sandboxes whose
container runtime bound the overlay with private propagation (the
default) keep the dead mount: restart them
([runbook](sandbox-overlay.md#sandboxes-still-fail-with-enotconn-after-a-recovery)).
The sandbox overlay's tmp files are gone after a crash: the new daemon
wipes the tmp layer at start.

## Check

```sh
NS=<namespace> POD=<pod>
kubectl -n "$NS" get pod "$POD" \
  -o jsonpath='{range .status.initContainerStatuses[*]}{.name}{" restarts="}{.restartCount}{" ready="}{.ready}{"\n"}{end}'
kubectl -n "$NS" logs "$POD" -c agentfs | grep -E '^(stacked|mounted) '
kubectl -n "$NS" exec "$POD" -c agentfs -- agentfs probe /agentfs/workspace
# live agentfs mount (agentfs:<volume>)
# with the sandbox overlay, probe its mount point too:
kubectl -n "$NS" exec "$POD" -c agentfs -- agentfs probe --sandbox-overlay /agentfs-sandbox/overlay
# live agentfs sandbox overlay (agentfs:<volume>:sandbox-overlay)
kubectl -n "$NS" exec "$POD" -c agentfs -- agentfs status
# backlog:   0 record(s)
# lock:      held by pid=1 command=mount since=...
kubectl -n "$NS" exec "$POD" -c agentfs -- agentfs push      # push now, do not wait for pushInterval
kubectl -n "$NS" logs "$POD" -c agentfs --previous | tail -n 50   # why it died
kubectl -n "$NS" describe pod "$POD" | grep -A5 'Last State'       # OOMKilled?
```

Each crash recovered this way leaves one dead layer under the live mount
until the sidecar stops. To see them, on the node:

```sh
findmnt -t fuse,fuse.agentfs -S agentfs:<volume>   # one live mount on top, plus one entry per dead layer
findmnt -t fuse,fuse.agentfs -S agentfs:<volume>:sandbox-overlay   # the same for the sandbox overlay
```

Find out why it died from the previous log. An OOM kill means the
sidecar's memory limit is too low for the volume; a panic is a bug:
report it with the log.

## When to step in

- **`CrashLoopBackOff`**: the daemon crashes again at every start (a
  corrupt state area, a full disk, an OOM during journal recovery). Each
  restart waits longer (up to 5 minutes), and the workload's calls fail
  meanwhile. Read the previous log, fix the cause, then delete the pod.
  Each crash adds a dead layer; a stop detaches up to 16.
- **Recovery refused**: the log says `--on-stale-mount recover: stacking
  a mount needs CAP_SYS_ADMIN` (a sidecar that is not privileged) and the
  container exits 1. Run it privileged, as the example does, or switch to
  `hold` and recover by deleting the pod (below).
- **Another volume's stale mount**: the log says `is a stale mount of
  agentfs:<other>, not of this volume` and the container exits 1. The
  mount point holds a dead mount of a different volume (a reused mount
  point); stacking over it would send that volume's workloads into this
  one's history. Delete the pod, or detach that mount on the node (below)
  if nothing uses it.
- **A hung daemon**: alive, but calls under the mount hang. Nothing
  restarts it (there is no liveness probe). Delete the pod; if the
  daemon is stuck in the kernel, the node may need the mount detached
  (below).
- **Files open with passthrough at the crash**: with `--passthrough auto`
  as root, writes through descriptors the workload opened for writing
  while another had the file open read-only (the only writers given
  passthrough, design §17.1) before the crash go straight to the backing file and are not recorded until the
  file changes again through the mount. If every write must be recorded,
  restart the workload after a crash, or run with `--passthrough off`.
- **A clean exit while the pod runs** (someone sent the sidecar SIGTERM,
  or unmounted it): the kubelet restarts it into a fresh mount. Workload
  containers that mount the `emptyDir` read-only (the example) got
  `EROFS` in the gap. A container with a `subPath` of the mount keeps the
  dead mount until its liveness probe restarts it; without that probe,
  restart it by hand. If it restarts inside the gap, its `subPath` is the
  bare directory, which the sidecar sealed (`AGENTFS_SEAL_MOUNTPOINT`:
  root's, mode 0000): a non-root container gets `EACCES` there, but a
  root one with `CAP_DAC_OVERRIDE` still writes, unrecorded.

## Recover by hand (hold mode, or as a fallback)

With `AGENTFS_ON_STALE_MOUNT=hold` the restarted sidecar mounts nothing:
its log says `Holding until SIGTERM, then detaching the stale mount`, the
pod is NotReady, and the workload's calls keep failing with `ENOTCONN`.
Restart the pod. With the example's `Recreate` strategy and a
`ReadWriteOncePod` volume, the old pod is gone before the new one mounts
the volume:

```sh
kubectl -n "$NS" delete pod "$POD" --timeout=120s
```

The workload containers stop first; the holding sidecar then gets
SIGTERM, detaches every dead layer (`detached stale mount
/agentfs/workspace (N layer(s))`, and the same line for the sandbox
overlay's mount point when it is served) and exits, so the kubelet can
remove the pod's volumes. The new pod's startup line reports the records the crash
left uncommitted; `backlog: 0` in `agentfs status` and `0 unpushed` per
remote mean everything is in history and pushed. Deleting the pod works
the same in recover mode.

## If the pod is stuck Terminating

The kubelet cannot remove the pod's `emptyDir` while a dead FUSE mount
is still attached there. The sidecar detaches dead layers when it stops;
if it could not (an older image, `AGENTFS_ON_STALE_MOUNT` unset, or the
sidecar crashed again at the end), detach them on the node:

```sh
# on the node (or: kubectl debug node/<node> -it --image=busybox -- chroot /host sh)
findmnt -t fuse,fuse.agentfs      # source agentfs:<volume>
umount -l /var/lib/kubelet/pods/<pod-uid>/volumes/kubernetes.io~empty-dir/agentfs-mount/workspace
# with the sandbox overlay, its mount point too:
umount -l /var/lib/kubelet/pods/<pod-uid>/volumes/kubernetes.io~empty-dir/agentfs-sandbox/overlay
# repeat while findmnt still lists it
```

## What not to do

- **Do not unmount the dead mount by hand while the workload runs**:
  detaching it exposes the bare directory under the mount point to the
  workload. With the example's read-only `emptyDir` writes then fail
  with `EROFS`; a `subPath` consumer, or a workload without the
  read-only mount, would write there unrecorded (hidden again by the
  next mount; the sealed mount point refuses non-root writers).
- **Do not add a liveness probe** to the sidecar: the kubelet already
  restarts a daemon that dies, and a probe would kill a slow but live one
  (a long journal recovery).
- Do not delete the PVC or the state area: the uncommitted records are in
  the journal there.
