# Runbook: recovering CSI volumes

What to do when an agentfs CSI volume misbehaves: a crashed volume host, a
node plugin upgrade, a node reboot, a pod that will not mount, a claim that
will not delete. The node plugin recovers crashes by itself; this runbook
is for checking that it did and for the cases it cannot handle. Library:
[kubernetes](../library/kubernetes.md); operator guide:
[csi.md](../csi.md#8-recovery).

Throughout, `$NODE_POD` is the node plugin pod on the node running the
workload, and `$PV` the volume's PersistentVolume name:

```sh
NODE=$(kubectl -n <ns> get pod <pod> -o jsonpath='{.spec.nodeName}')
NODE_POD=$(kubectl -n agentfs-csi get pod -l app.kubernetes.io/name=agentfs-csi-node \
  --field-selector spec.nodeName=$NODE -o name)
PV=$(kubectl -n <ns> get pvc <claim> -o jsonpath='{.spec.volumeName}')
plugin_log() { kubectl -n agentfs-csi logs $NODE_POD -c agentfs-csi "$@"; }
in_plugin() { kubectl -n agentfs-csi exec $NODE_POD -c agentfs-csi -- "$@"; }
```

## The host per volume

Each volume in use on a node has one `agentfs host` process in that node's
plugin container: the volume's view host. Every pod's mount of the volume
is a view it serves at the kubelet's target, and a fork claim's pod is
served by its source volume's host.

```sh
in_plugin pgrep -af ' host --launcher csi'
plugin_log | grep "\[host $PV\]"
```

A host logs its settings that are not built (`not built: <key>
(<capability>): ...`) at each start; that is expected, not a fault.

## A crashed host

**Symptoms.** File calls in the volume's pods fail with "Transport
endpoint is not connected" (`ENOTCONN`) for a few seconds.

The plugin starts the host again with backoff (1 s, doubling to
`--backoff-max`, back to 1 s after a minute of uptime). The new host opens
every published view of the volume again, stacked over the dead mount at
each kubelet target, rebinding each view's client so a fork keeps its one
writer, then reconciles.

1. Check the log: `[host <pv>] exited: <exit>; restarting in 1s`, then
   `published volume <pv> at <target>, over a stale mount`.
2. A container without `mountPropagation: HostToContainer` keeps seeing
   the dead mount: restart that container (or its pod).
3. Repeated restarts with growing delays mean the host fails at start: its
   last lines, prefixed `[host <pv>]`, say why (a full disk, a refused
   setting). Fix the cause; the next attempt picks it up.

To test the recovery, kill a host: `in_plugin pkill -9 -f "volumes/$PV/state
host"`.

## Upgrading the node plugin

A node plugin's restart stops that node's hosts: on SIGTERM it closes each
host's control channel, and each host drains (its last saves made) and
exits, within the pod's 60 s grace. Their FUSE sessions cannot be handed to
another process, so the node's mounts answer `ENOTCONN` until the new
plugin opens them again from `<nodeState>/published/`. The DaemonSet is
`updateStrategy: OnDelete`: roll it node by node, stopping the node's
agentfs pods first, so nothing is interrupted:

```sh
kubectl -n agentfs-csi set image daemonset/agentfs-csi-node agentfs-csi=<new image>
for node in <nodes>; do
  kubectl cordon "$node"
  # Stop the node's pods that use agentfs volumes; they wait for this
  # node (their PVs are pinned to it).
  kubectl delete pod -A --field-selector spec.nodeName="$node" -l <your agentfs workloads>
  kubectl -n agentfs-csi delete pod -l app.kubernetes.io/name=agentfs-csi-node \
    --field-selector spec.nodeName="$node"
  kubectl -n agentfs-csi wait pod -l app.kubernetes.io/name=agentfs-csi-node \
    --field-selector spec.nodeName="$node" --for=condition=Ready --timeout=5m
  kubectl uncordon "$node"
done
```

With Helm, `helm upgrade` changes the DaemonSet the same way, and each
node takes the new plugin only when its pod is deleted. Check each node's
new plugin log for `reconciled: N host(s) started, M view(s) opened
again, ...`. Deleting a plugin pod with the workloads still running also
works: they see `ENOTCONN` for seconds, and containers without
`HostToContainer` need a restart.

## A node reboot

The kubelet publishes the volumes of the pods it restarts, and the
plugin's records from before the reboot are reconciled: those whose target
is gone are dropped (`reconciled: ... N record(s) dropped`).

## A pod that will not mount

**Symptoms.** A pod waits in `ContainerCreating` with a `FailedMount`
event:

- `several pods on one volume are not built`: another pod has the claim.
  Stop it first; use `ReadWriteOncePod` and `strategy: Recreate`
  ([csi.md](../csi.md#6-using-a-volume)).
- `has had its writer`: a fork claim serves one pod for that pod's life.
  Delete the fork claim and make a new one ([csi.md](../csi.md#14-forks)).
- `is a recording-format volume`: a volume an older driver made, never
  served: [moving.md](moving.md).
- `no volume <pv> on node <node> under <base>; the pod waits`: the
  directory is gone from the node's disk (a wiped disk). The volume is
  lost; make a new claim.
- `the host of volume <pv> refused to start` or `exited before it served`,
  with its last lines: fix what they name (a full disk, a profile setting
  refused at boot).

## A claim that will not delete

**Symptoms.** A deleted claim's PV stays `Released`, with
`VolumeFailedDelete` events (or lines in the csi-provisioner's log):

- `volume <pv> is in use on this node (published at <target>); nothing
  deleted`: a pod of the volume, or of one of its fork claims, still runs.
  Stop it; the provisioner retries.
- `is a recording-format volume`: archive it and remove its directory by
  hand ([moving.md](moving.md)); the next retry succeeds.
- `the host of volume <pv> did not release it`: the drain failed; its
  lines say why.

A deleted root claim's volume is kept, never removed: the plugin logs
`kept volume <pv> at <dir>: ...`, and the directory stays with
`csi.retained.json`. Mount it again with a static PV
([csi.md](../csi.md#5-parameters)), or remove `<base>/<pv>` by hand once
its history is archived (`git --git-dir <base>/<pv>/state/history.git
bundle create <file> --all`).

## Related

- [moving.md](moving.md): recording-format volumes, and copying a volume
  to another node.
- [recording-failures.md](recording-failures.md): writes failing with
  `EIO` or `ENOSPC`.
