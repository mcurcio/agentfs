# Runbook: a node is lost

An agentfs volume is used from one node at a time
([design §19.1](../design.md#191-decisions-2026-10-12), 2026-10-15). With
a shared basePath (NFS) it can move to another node, but only after it
has been detached from the first. When a node stops answering,
Kubernetes cannot tell a powered-off node from one that is cut off but
still running, so it does not detach the volume by itself: the pods stay
Terminating and the volume stays attached to the lost node until an
operator says the node is down. Design:
[§14.2](../design.md#142-csi-driver-built), the CSI guide:
[§11](../csi.md#11-shared-filesystems).

**Never do this for a node that may still be running.** A node that
stops answering the API server may still run its history service and
pods. While it still reaches the NFS server it keeps renewing its lease
and holds the state lock, so a service on another node cannot start. Once
it is cut off from the NFS server too, the server expires its lease (90 s
by default on Linux) and a service on another node can take the lock.
When the network returns, the cut-off node's processes, which never
stopped, write again: there are two writers and the journal can be
corrupted (the forced-detach residual, csi.md §11). Being cut off is
therefore no proof that a node is down; only a node that is powered off,
or whose agentfs processes are confirmed dead, is safe to declare lost.
The only strong guard is a node-local basePath, which pins each volume to
its node.

This procedure is not run in CI.

## Symptoms

- `kubectl get nodes` shows the node `NotReady`.
- Pods that used agentfs volumes on it are `Terminating` and stay so.
- Their replacements stay `ContainerCreating` on another node, with
  `FailedAttachVolume` events (`Multi-Attach error`, or on Kubernetes
  1.37 `Waiting for detach`), because the VolumeAttachment still names
  the lost node.

## Steps

1. **Confirm the node is really down**: powered off, or fenced so that
   none of its processes can run again. `NotReady`, or a node that no
   longer reaches the API server or the NFS server, is not enough, and
   neither is the NFS server's client list (a Linux NFS server may keep
   an expired client listed as a courtesy client). Confirm it from
   outside Kubernetes: the machine's power state on its console, BMC or
   hypervisor, or a power cycle, or on its console that kubelet and every
   `agentfs serve-history` and `agentfs mount` process are gone. If you
   cannot confirm it, power the node off first.
2. **Mark it out of service** (Kubernetes non-graceful node shutdown):

   ```sh
   kubectl taint nodes <node> node.kubernetes.io/out-of-service=nodeshutdown:NoExecute
   ```

   Kubernetes then deletes the node's pods and detaches their volumes
   without waiting for the node.
3. **Wait for the pods to be deleted and the VolumeAttachments to go**:

   ```sh
   kubectl get pods -A --field-selector spec.nodeName=<node>
   kubectl get volumeattachments | grep <node>
   ```

   Both must come back empty.
4. **Check that the volume's state lock is free** before the new pod
   starts, from the node plugin pod of another node (basePath is shared):

   ```sh
   NODE_POD=$(kubectl -n agentfs-csi get pod -l app.kubernetes.io/name=agentfs-csi-node \
     --field-selector spec.nodeName=<other node> -o name)
   kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- \
     agentfs --state /var/lib/agentfs-csi/volumes/<pv>/state status
   ```

   `lock:      free` means no history service holds it. `lock: held by
   pid=… command=serve-history` while the node is down means the NFS
   server has not yet expired the lost node's lease (Linux's default
   lease is 90 s): wait and check again. If it stays held, the lost
   node may still be running: go back to step 1. The replacement pod's
   NodeStage starts the volume's service on the new node, which recovers
   the journal (journaled records are committed under their original
   pod's committer); what failed with `EIO` on the lost node is lost with
   its pods.
5. **After the node is repaired** and its agentfs processes are gone
   (check with `ps` on the node: no `agentfs serve-history` or `agentfs
   mount`), remove the taint:

   ```sh
   kubectl taint nodes <node> node.kubernetes.io/out-of-service=nodeshutdown:NoExecute-
   ```

## Related

- [csi-recovery.md](csi-recovery.md): crashed fronts, services and node
  plugins on a node that is still up.
- [history-service-down.md](history-service-down.md): fronts failing
  with `EIO` while the history service is down.
