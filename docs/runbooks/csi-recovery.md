# Runbook: recovering CSI volumes

What to do when an agentfs CSI volume misbehaves: a crashed front or
history service, a node plugin upgrade, a node reboot, a volume stuck
staging. The node plugin recovers crashes by itself; this runbook is for
checking that it did and for the cases it cannot handle. Design:
[§14.2](../design.md#142-csi-driver-built); operator guide:
[csi.md](../csi.md#8-recovery).

Throughout, `$NODE_POD` is the node plugin pod on the node running the
workload, and `$PV` the volume's PersistentVolume name:

```sh
NODE=$(kubectl -n <ns> get pod <pod> -o jsonpath='{.spec.nodeName}')
NODE_POD=$(kubectl -n agentfs-csi get pod -l app.kubernetes.io/name=agentfs-csi-node \
  --field-selector spec.nodeName=$NODE -o name)
PV=$(kubectl -n <ns> get pvc <claim> -o jsonpath='{.spec.volumeName}')
status() { kubectl -n agentfs-csi exec $NODE_POD -c agentfs-csi -- \
  agentfs --state /var/lib/agentfs-csi/volumes/$PV/state status; }
plugin_log() { kubectl -n agentfs-csi logs $NODE_POD -c agentfs-csi "$@"; }
```

## A crashed front

**Symptoms.** One pod's file calls on the volume fail with "Transport
endpoint is not connected" (`ENOTCONN`) for a few seconds; other pods on
the volume keep working. The plugin log says:

```
[front <ns>/<pod> <key>] ...
front for <ns>/<pod> exited (signal 9); restarting in 1s in slot 3
front for <ns>/<pod> restarted in slot 3; its mount is stacked over the stale one at /var/lib/kubelet/pods/.../mount
```

**Check.**

1. `status` lists a front for the pod (`fronts:    slot 3: pid=… pod=<pod>`)
   with a new pid, in the same slot as before.
2. In the pod: `ls <mountPath>` works. If it still fails with `ENOTCONN`
   and the volume mount has no `mountPropagation: HostToContainer`, the
   container still sees the dead mount: restart the container (`kubectl
   exec <pod> -- kill 1`, or delete the pod). Add `HostToContainer` to
   the workload (csi.md §6).
3. A front restarting in a loop (`restarting in 30s`) logs why in the
   lines prefixed `[front <ns>/<pod> …]` above it: a full disk, a broken
   profile, a service that refuses it (`refused this front: …`).

Nothing is lost: a change whose call returned was journaled by the
service (design §3.2); changes whose call failed are recorded with their
final state by the new front's repairs (design §3.5).

## A crashed or crash-looping history service

**Symptoms.** Every pod on the volume gets `EIO` on closes and renames,
`HistoryServiceUnavailable` alarms fire, and the plugin log says
`service for <pv> exited (…); restarting in …`.

**Check.**

1. `plugin_log | grep "\[service $PV\]" | tail -n 50`: the service's own
   last lines say why it stopped.
2. `status`: `history:   serving fronts at unix:…` once it is back; the
   journal line shows what it recovered.
3. **The lock holder.** If the service cannot start because the state
   area is locked (`locked by pid=… command=…`), something else holds it:
   a leftover process from a previous plugin (check `ps` on the node), or
   an operator's `agentfs` command in the plugin pod. Stop it; the plugin
   retries with backoff.
4. Fronts reconnect by themselves within their next request; no pod
   restart is needed.

For a service that keeps failing on the journal or history, follow
[history-service-down.md](history-service-down.md) inside the plugin pod.

## Upgrading the node plugin

The DaemonSet uses `updateStrategy: OnDelete`, because a restarting plugin
stops its children: every agentfs mount on that node answers `ENOTCONN`
until the new plugin has recovered them (seconds), and containers without
`HostToContainer` propagation need a restart.

Per node:

1. Either drain it (`kubectl drain <node> --ignore-daemonsets`), so no
   workload is affected, or accept a brief `ENOTCONN` for its pods.
2. `kubectl -n agentfs-csi apply -k deploy/csi` (the new image), once.
3. `kubectl -n agentfs-csi delete pod $NODE_POD`. On SIGTERM the plugin
   stops every front and service; each makes its final commit within the
   pod's grace period (60 s).
4. The new pod logs `reconcile: started the history service for <pv>`
   and `reconcile: recovering the front for <ns>/<pod> at …` for every
   volume and target. Check `status` per volume.
5. Restart containers without `HostToContainer` propagation, then
   `kubectl uncordon <node>`.

## A node reboot

Nothing to do in the common case: the records under
`/var/lib/agentfs-csi/node` survive, the new plugin restarts the staged
volumes' services, forgets the targets that no longer exist, and the
kubelet stages and publishes the volumes again for the pods it restarts.
Each service recovers its journal before it serves (design §3.3).

Check that the plugin log has no `warning: an unreadable node record was
skipped` lines; a record the plugin could not read is left in place and
named there. Remove it by
hand once the volume it names is not in use.

## A volume stuck staging

**Symptoms.** Pods stay `ContainerCreating`; their events say
`MountVolume.MountDevice failed … Unavailable desc = volume <pv>'s history
service is starting (journal recovery or patch fetch); still waiting after
60s` and repeat.

The service is still starting: NodeStage waits `--stage-wait`, answers
`UNAVAILABLE` and the kubelet retries, while the service keeps running.
It is usually:

- **Journal recovery** of a large journal (`[service <pv>] recovering …`).
  Wait.
- **Applying queued patches** (design §12), bounded by
  `agentfs.patchFetchTimeout`. Wait, or check the queue with
  [patch-queue.md](patch-queue.md).
- **A service that exits** at start: the event then says
  `FailedPrecondition desc = volume <pv>'s history service exited (…)`
  with its last log lines (a profile that
  does not exist, a state area of another volume, a full disk). Fix the
  cause; the kubelet's next attempt starts it again.
