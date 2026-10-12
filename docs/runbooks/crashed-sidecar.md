# Runbook: a crashed agentfs sidecar

The agentfs daemon in a pod's sidecar died (a crash, an OOM kill, `kill
-9`). Every change the workload saw succeed was journaled before its call
returned, so nothing is lost, but the mount is dead until the pod is
restarted. Phase 1 recovers by restarting the pod, by hand. Design:
[§17.1](../design.md#171-layer-1-core-and-daemon-rust); deployment:
[deployment.md](../deployment.md#7-a-crashed-daemon).

## Symptoms

- The workload's file calls under the mount fail with **"Transport
  endpoint is not connected"** (`ENOTCONN`). This is deliberate: agentfs
  never exposes the empty directory beneath a dead mount, where writes
  would go unrecorded.
- The pod is **NotReady**: the sidecar's readiness probe (`agentfs probe
  /agentfs/workspace`) fails.
- The sidecar container has restarted (`RESTARTS` ≥ 1 on the `agentfs`
  container) and its log says it is holding:

  ```
  /agentfs/workspace is an agentfs mount (agentfs:<volume>) whose daemon died: not remounting while workloads may still hold it. Phase 1 recovers by restarting the pod (kubectl delete pod <pod>); its next start recovers the journal. Holding until SIGTERM, then detaching the stale mount.
  ```

- Telemetry: no metrics from the pod since the crash, and
  `agentfs.push.oldest_unpushed_age` stops being reported. Alert on the
  sidecar's readiness and restarts.

## Check

```sh
NS=<namespace> POD=<pod>
kubectl -n "$NS" get pod "$POD" -o wide
kubectl -n "$NS" get pod "$POD" \
  -o jsonpath='{range .status.initContainerStatuses[*]}{.name}{" restarts="}{.restartCount}{" ready="}{.ready}{"\n"}{end}'
kubectl -n "$NS" exec "$POD" -c agentfs -- agentfs probe /agentfs/workspace
# stale agentfs mount (daemon gone)
kubectl -n "$NS" logs "$POD" -c agentfs --previous | tail -n 50   # why it died
kubectl -n "$NS" describe pod "$POD" | grep -A5 'Last State'       # OOMKilled?
```

`agentfs status` works in the holding sidecar and shows what is waiting
(it takes no lock and reads the journal without changing it):

```sh
kubectl -n "$NS" exec "$POD" -c agentfs -- agentfs status
# backlog:   3 record(s)
# lock:      free
```

## Recover

1. **Restart the pod.** With the example's `Recreate` strategy and a
   `ReadWriteOncePod` volume, the old pod is gone before the new one
   mounts the volume:

   ```sh
   kubectl -n "$NS" delete pod "$POD" --timeout=120s
   ```

   The workload containers stop first; the holding sidecar then gets
   SIGTERM, detaches the stale mount (`detached stale mount
   /agentfs/workspace` in its log) and exits, so the kubelet can remove
   the pod's volumes.

2. **Check the new pod.** It becomes Ready once the mount is live. Its
   sidecar's startup line reports the records the crash left
   uncommitted:

   ```sh
   NEW=$(kubectl -n "$NS" get pod -l app.kubernetes.io/name=agentfs-example -o name | head -n1)
   kubectl -n "$NS" logs "$NEW" -c agentfs | grep '^mounted '
   # mounted /data/backing at /agentfs/workspace (volume ..., 3 uncommitted record(s) recovered, ...)
   kubectl -n "$NS" exec "$NEW" -c agentfs -- agentfs status
   ```

   `backlog: 0` and, after the next `pushInterval`, each remote's
   `0 unpushed` mean everything is in history and pushed.

3. **Find out why it died** from the previous log (`--previous` on the
   old pod, or your log store). An OOM kill means the sidecar's memory
   limit is too low for the volume; a panic is a bug: report it with
   the log.

## If the pod is stuck Terminating

The kubelet cannot remove the pod's `emptyDir` while a dead FUSE mount
is still attached there. The holding sidecar detaches it at SIGTERM; if
the sidecar was not holding (an older image, or `AGENTFS_ON_STALE_MOUNT`
not set to `hold`), detach it on the node:

```sh
# on the node (or: kubectl debug node/<node> -it --image=busybox -- chroot /host sh)
findmnt -t fuse,fuse.agentfs      # source agentfs:<volume>
umount -l /var/lib/kubelet/pods/<pod-uid>/volumes/kubernetes.io~empty-dir/agentfs-mount/workspace
```

## What not to do

- **Do not unmount the stale mount by hand while the workload runs**,
  and do not send SIGTERM to a holding sidecar while the pod should keep
  running: detaching it exposes the bare directory under the mount
  point to the workload, and anything written there is not recorded (and
  is hidden again by the next mount). Delete the pod instead.
- **Do not add a liveness probe** to the sidecar: a restart cannot
  remount safely, so it would only restart into the same hold.
- Do not delete the PVC or the state area: the uncommitted records are in
  the journal there.
