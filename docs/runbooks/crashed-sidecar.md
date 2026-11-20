# Runbook: a crashed agentfs sidecar

The agentfs daemon in a pod's sidecar died (a crash, an OOM kill, `kill
-9`). The sidecar runs the tree engine ([trees](../trees.md)): every change
the workload saw succeed was journaled before its call returned, so nothing
is lost. The shipped sidecar recovers by itself: the kubelet restarts the
container, and it stacks a new mount over the dead one
(`AGENTFS_ON_STALE_MOUNT=recover`). This runbook is for checking that
recovery, and for the cases where it does not happen. Deployment:
[deployment.md](../deployment.md#7-a-crashed-daemon); the mechanism:
[restarting after a crash](../trees.md#restarting-after-a-crash).

## What automatic recovery looks like

1. For a few seconds the workload's file calls under the mount fail with
   **"Transport endpoint is not connected"** (`ENOTCONN`). This is
   deliberate: agentfs never exposes the empty directory beneath a dead
   mount, where writes would go unrecorded.
2. The kubelet restarts the `agentfs` container (`RESTARTS` goes up by
   one). The new daemon sees a stale mount of its own volume at the mount
   point, takes the state area's lock, recovers the volume, and stacks a
   new mount over the dead one. Its log shows the operator settings not
   built yet, then:

   ```
   stacked over a stale mount at /agentfs/workspace left by a crashed daemon
   mounted volume <volume> at /agentfs/workspace; stop with SIGINT or SIGTERM
   ```

3. The pod is Ready again. The workload containers were not restarted:
   their next calls reach the new daemon, through the mount propagated
   into them and through `subPath` binds alike.
4. When that view ends, it detaches the dead layers beneath it (`tree
   DetachedStale: /agentfs/workspace: 1 layer(s)`).

## Check

```sh
NS=<namespace> POD=<pod>
kubectl -n "$NS" get pod "$POD" \
  -o jsonpath='{range .status.initContainerStatuses[*]}{.name}{" restarts="}{.restartCount}{" ready="}{.ready}{"\n"}{end}'
kubectl -n "$NS" logs "$POD" -c agentfs | grep -E '^(stacked|mounted|not built) '
kubectl -n "$NS" exec "$POD" -c agentfs -- agentfs probe /agentfs/workspace
# live agentfs mount (agentfs:<volume>)
kubectl -n "$NS" exec "$POD" -c agentfs -- git --git-dir /data/state/history.git log -3 --format='%h %s' root
kubectl -n "$NS" logs "$POD" -c agentfs --previous | tail -n 50   # why it died
kubectl -n "$NS" describe pod "$POD" | grep -A5 'Last State'       # OOMKilled?
```

History is built behind the calls, so the newest commits can trail the
files for a moment after the restart; the new daemon builds what the dead
one left unbuilt.

Each crash recovered this way leaves one dead layer under the live mount
until the view ends. To see them, on the node:

```sh
findmnt -t fuse,fuse.agentfs -S agentfs:<volume>   # one live mount on top, plus one entry per dead layer
```

Find out why it died from the previous log. An OOM kill means the
sidecar's memory limit is too low for the volume; a panic is a bug:
report it with the log.

## When to step in

- **`CrashLoopBackOff`**: the daemon crashes again at every start (a
  corrupt state area, a full disk, an OOM during recovery). Each restart
  waits longer (up to 5 minutes), and the workload's calls fail
  meanwhile. Read the previous log, fix the cause, then delete the pod.
  Each crash adds a dead layer.
- **Recovery refused**: the log says `--on-stale-mount recover: stacking
  a mount needs CAP_SYS_ADMIN` (a sidecar that is not privileged) and the
  container exits 1. Run it privileged, as the example does, or switch to
  `hold` and recover by deleting the pod (below).
- **Another volume's stale mount**: the log says the mount point `is a
  stale agentfs mount (agentfs:<other>) this mount does not stack over`
  and the container exits 1. The mount point holds a dead mount of a
  different volume (a reused mount point); stacking over it would send
  that volume's workloads into this one's history. Delete the pod, or
  detach that mount on the node (below) if nothing uses it.
- **A hung daemon**: alive, but calls under the mount hang. It is not
  detected: there is no liveness probe on the sidecar. Delete the pod; if
  the daemon is stuck in the kernel, the node may need the mount
  detached (below).
- **A clean stop while the pod runs** (someone sent the sidecar SIGTERM,
  or unmounted the view): the sidecar drains, the view is unmounted and
  the pod turns NotReady. Workload containers that mount the `emptyDir`
  read-only (the example) get `EROFS` in the gap. Once the process exits,
  the kubelet restarts it into a fresh mount. A container with a
  `subPath` of the mount holds the drained view (its writes fail with
  `EROFS`), and the sidecar does not exit while it does: kill it (`kill
  -9` of its pid from the node, or delete the pod). The `subPath` then
  fails with `ENOTCONN` until the container's liveness probe restarts it;
  without that probe, restart it by hand. If it restarts inside the gap,
  its `subPath` is the bare directory, which the sidecar sealed
  (`AGENTFS_SEAL_MOUNTPOINT`: root's, mode 0000): a non-root container
  gets `EACCES` there, but a root one with `CAP_DAC_OVERRIDE` still
  writes, unrecorded.
- **The restarted sidecar exits 1 with `the mount point /agentfs/workspace
  is not empty`**: a root `subPath` consumer wrote into the bare
  directory in the gap. Those files were never recorded. Scale the
  workload down, move them out of the mount point on the node, and
  restart the pod.

## Recover by hand (hold mode, or as a fallback)

With `AGENTFS_ON_STALE_MOUNT=hold` the restarted sidecar mounts nothing
and takes no lock: its log says `Holding until SIGTERM, then detaching the
stale mount(s).`, the pod is NotReady, and the workload's calls keep
failing with `ENOTCONN`. Restart the pod. With the example's `Recreate`
strategy and a `ReadWriteOncePod` volume, the old pod is gone before the
new one mounts the volume:

```sh
kubectl -n "$NS" delete pod "$POD" --timeout=120s
```

The workload containers stop first; the holding sidecar then gets
SIGTERM, detaches every dead layer (`detached stale mount
/agentfs/workspace (N layer(s))`) and exits, so the kubelet can remove
the pod's volumes. The new pod recovers the volume at start and builds
what the crash left unbuilt. Deleting the pod works the same in recover
mode.

## If the pod is stuck Terminating

The kubelet cannot remove the pod's `emptyDir` while a dead FUSE mount
is still attached there. A view detaches the dead layers below it when it
ends; if it did not end (a `subPath` consumer made the pod's stop end in
the kubelet's kill, or the sidecar crashed again at the end), detach them
on the node:

```sh
# on the node (or: kubectl debug node/<node> -it --image=busybox -- chroot /host sh)
findmnt -t fuse,fuse.agentfs      # source agentfs:<volume>
umount -l /var/lib/kubelet/pods/<pod-uid>/volumes/kubernetes.io~empty-dir/agentfs-mount/workspace
# repeat while findmnt still lists it
```

## What not to do

- **Do not unmount the dead mount by hand while the workload runs**:
  detaching it exposes the bare directory under the mount point to the
  workload. With the example's read-only `emptyDir` writes then fail
  with `EROFS`; a `subPath` consumer, or a workload without the
  read-only mount, would write there unrecorded (the sealed mount point
  refuses non-root writers).
- **Do not add a liveness probe** to the sidecar: the kubelet already
  restarts a daemon that dies, and a probe would kill a slow but live one
  (a long recovery).
- Do not delete the PVC or the state area: the journal there holds what
  history has not built yet.
