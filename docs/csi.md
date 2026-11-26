# The agentfs CSI driver

`agentfs-csi` gives Kubernetes workloads agentfs volumes as ordinary
PersistentVolumeClaims: a pod mounts a claim, and every change it makes is
recorded as git history by a process it cannot see or configure. Every volume
is a tree volume ([trees](trees.md)) on the disk of one node. The code's API is
in [library/kubernetes](library/kubernetes.md); this page is the operator's
guide and reference.

- [1. What it is](#1-what-it-is)
- [2. Architecture](#2-architecture)
- [3. Requirements](#3-requirements)
- [4. Install](#4-install)
- [5. Parameters](#5-parameters)
- [6. Using a volume](#6-using-a-volume)
- [7. What each CSI call does](#7-what-each-csi-call-does)
- [8. Recovery](#8-recovery)
- [9. Operations](#9-operations)
- [10. Security](#10-security)
- [11. Where volumes live](#11-where-volumes-live)
- [12. Limitations](#12-limitations)
- [13. How it is tested](#13-how-it-is-tested)
- [14. Forks](#14-forks)

## 1. What it is

There are two ways to run agentfs on Kubernetes:

| | Sidecar ([deployment.md](deployment.md)) | CSI driver (this page) |
|---|---|---|
| Who runs agentfs | a privileged native sidecar in every workload pod | the driver's node plugin, in its own namespace |
| Workload pods | privileged (`Bidirectional` propagation); their namespace must allow `privileged` | unprivileged; their namespace can enforce `restricted` |
| Pods per volume | one | one per claim; a fork claim gives a second pod its own fork of the volume (§14) |
| Where the files live | the pod's volume | a tree volume on the disk of the node that created the claim, for good |
| Settings | a ConfigMap in the workload's namespace | profiles chosen by StorageClass, in the driver's namespace only |
| Crash recovery | the sidecar's restart | automatic: the plugin restarts a crashed volume host |

Both run the same engine, the tree views. The image is the same
(`ghcr.io/mcurcio/agentfs`): it ships `agentfs`, `agentfs-csi` and `tini`.
The driver's name is `agentfs.mcurcio.github.io`.

**What is not built yet**, and says so: push, the patch queue and LFS. A
profile's remotes, push settings and patch settings are accepted, and each
volume host logs them at every start as `not built: <key> (<capability>):
...` and counts them in the `agentfs.not_built` gauge (§9). The history stays
in the volume's state area on its node. Deleting a volume, several pods on one
volume, moving a volume and forks of forks are refused with an error that
says they are not built (§5, §7, §14).

## 2. Architecture

```
 node DaemonSet (every node)
 ┌──────────────────────────────────────────────────────────────────────┐
 │ csi-provisioner (--node-deployment)  node-driver-registrar            │
 │ livenessprobe                                                         │
 │ tini ── agentfs-csi              Identity, Controller and Node services│
 │          ├─ agentfs init          (per CreateVolume, then exits)       │
 │          └─ agentfs host          (per volume in use on the node)      │
 │               └─ one view per published pod, at the kubelet's target   │
 └──────────────────────────────────────────────────────────────────────┘
```

There is no controller Deployment, no attach step and no stage step. **Every
node plugin** serves all three CSI services, beside its own csi-provisioner in
`--node-deployment` mode:

- **csi-provisioner** provisions the claims whose pods the scheduler put on its
  node (`WaitForFirstConsumer`), and deletes the PVs of its node's volumes. It
  is the only container with an API token.
- **CreateVolume** runs `agentfs init` for the claim's volume under this
  node's base path, and answers this node's topology
  (`topology.agentfs.mcurcio.github.io/node`): the PV's node affinity keeps
  every later pod of the claim on this node.
- **The volume host.** For each volume in use on its node, the plugin runs one
  child process:

  ```sh
  agentfs --state <base>/<pv>/state host --launcher csi --volume <name> \
    --namespace <plugin namespace> --pod <plugin pod> --node <node>
  ```

  It is the volume's view host ([trees](trees.md#serving-views-on-request)).
  The plugin drives it over the child's standard input, one end of a Unix
  socket pair whose other end only the plugin holds (the control protocol is
  in [library/filesystem](library/filesystem.md#the-view-host-command)). It
  opens and closes views and creates and ends keyed forks on request. When the
  plugin closes its end, the host drains and exits 0 once Volume releases the
  volume, so a host never outlives its plugin.
- **NodePublish** opens a view of the claim's tree (a FUSE mount, source
  `agentfs:<volume>`) directly at the kubelet's target path. The view is a
  client of its tree with the publishing pod's context and the label
  `csi-<target hash>` (16 hex digits of the target path's SHA-256), so every
  save it makes carries `Agentfs-Pod`, `Agentfs-Namespace`, `Agentfs-Pod-Uid`
  and `Agentfs-Node` trailers. **NodeUnpublish** closes the view, and a host
  with no views left stops.
- **DeleteVolume** of a root claim starts the volume's host with no view and
  stops it: the host's exit 0 is Volume's verdict that the volume is released.
  The directory is then kept (§7).

A crashed host does not take the plugin down: one child per volume follows
the FUSE CSI drivers' practice, and the plugin restarts it (§8). `tini` reaps
what the children leave: the plugin shares the host's pid namespace
(`hostPID: true`), so `tini` is not PID 1 and runs as a subreaper (`-s`). The
hosts need that namespace: FUSE reports a caller in a pod's own pid namespace
as pid 0, and its supplementary groups (the pod's `fsGroup`) are then unknown.

**On disk.** Under `--base-path` (default `/var/lib/agentfs-csi/volumes`, a
hostPath on each node), one directory per claim, named after the PV:

```
<base>/<pv>/
  state/              the tree state area; its history is state/history.git
  csi.json            the claim record (CreateVolume, or a static PV)
  csi.retained.json   the record once DeleteVolume kept the volume
<base>/<fork-pv>/
  csi.json            a fork claim: its fork lives in its source's volume
```

Under `--node-state` (default `/var/lib/agentfs-csi/node`, a hostPath that
survives plugin restarts), one record per published claim, from which a
restarted plugin opens every view again:

```
<nodeState>/published/<pv>.json   target, tree, read-only flag, pod context, label
```

The profiles are read from `--profiles-dir` (default
`/etc/agentfs-csi/profiles`). Every flag is in
[configuration](configuration.md#the-csi-driver-agentfs-csi).

## 3. Requirements

- **FUSE** on every node (`/dev/fuse`; `modprobe fuse`).
- **The base path on each node's own disk**: any local directory, ideally a
  disk of its own (§11). A network filesystem is refused at start.
- **One node per volume, for good.** Every multi-node access mode
  (`ReadWriteMany`, `ReadOnlyMany`) is refused.
- **A privileged namespace for the driver** (`agentfs-csi`, Pod Security
  `privileged`). Workloads need no privilege.
- The CSI sidecars from `registry.k8s.io/sig-storage`, pinned by digest in
  `deploy/csi/kustomization.yaml`: csi-provisioner, csi-node-driver-registrar
  and livenessprobe.

## 4. Install

```sh
# 1. Edit the profiles (deploy/csi/profiles).
kubectl apply -f deploy/csi/namespace.yaml
# 2. Install the driver.
kubectl apply -k deploy/csi
# 3. A workload: a StorageClass, a claim and one pod.
kubectl apply -k deploy/csi/example
```

With Helm instead: `charts/agentfs-csi`, published at
`oci://ghcr.io/mcurcio/charts/agentfs-csi` ([helm.md](helm.md)).

`deploy/csi` installs only the driver; `deploy/csi/example` is a workload. Pin
the agentfs image to a commit tag or digest in production (the
kustomization's `images`).

**Profiles.** A profile is a set of files in the `agentfs-csi-profiles`
ConfigMap, mounted into the driver's pods at `/etc/agentfs-csi/profiles`:

| File | Becomes | Required |
|---|---|---|
| `<profile>.config` | the volume's operator settings: `agentfs init --include` makes it an include of `<state>/config` ([trees](trees.md#operator-settings)) | yes |
| `<profile>.exclude` | the volume's `state/history.git/info/exclude` ([trees](trees.md#rules-and-the-untracked-area)) | no |
| `<profile>.attributes` | the volume's `state/history.git/info/attributes` | no |

A fragment must not set `agentfs.volume` or `agentfs.branch`. The ConfigMap has
no hash suffix, so edits reach the running pods without restarts: a host reads
the included config at each start, and installs the rule files again each time
it starts. A setting that tree volumes do not build yet (remotes, push and
patch settings, LFS) is logged as not built at each start, and the volume
serves anyway.

**Credentials.** The `agentfs-csi-credentials` Secret is mounted into the node
plugin at `/etc/agentfs-csi/credentials` (mode 0400), for a profile's
`agentfsCredentials` and SSH keys. Push is not built on tree volumes, so
nothing reads them yet.

**The chart's git server.** With `server.enabled=true`, the chart runs a
minimal git server in its namespace and mounts its password into the node
plugin as the key `agentfs-server` ([helm.md](helm.md#the-git-server)). A
profile may name it as a remote; the host then logs `not built:
remote.server.url (push): ...`, and nothing is pushed to it.

## 5. Parameters

A StorageClass's `parameters`:

| Parameter | Meaning | Default |
|---|---|---|
| `profile` | required: the profile (`<profile>.config` must exist); a DNS label | |
| `volumeName` | the agentfs volume name (the views' mount source, the volume in every commit's trailers), a template over `{pvcNamespace}`, `{pvcName}` and `{pvName}` that must contain `{pvName}` | `{pvcNamespace}.{pvcName}.{pvName}` (the provisioner runs with `--extra-create-metadata`), else the PV name |
| `onDelete` | `retain` only: DeleteVolume keeps the volume's directory. `delete` is refused: `onDelete: delete is not built on tree volumes: a deleted claim's volume is kept, because its history is pushed nowhere yet` | `retain` |
| `fork` | `"true"` makes a fork class (§14) | `"false"` |

Refused as not built, each with `InvalidArgument` naming it:

- `startFrom`: `startFrom is not built on tree volumes: a volume starts empty;
  moving a volume between nodes is not built (docs/runbooks/moving.md)`;
- `workloadMounts`: `workloadMounts is not built on tree volumes: tree volumes
  keep nested repositories untracked`.

Keys under `csi.storage.k8s.io/` are the provisioner's and are otherwise
ignored; any other key is refused (`InvalidArgument`), `branch` included. The
requested size is recorded, not enforced. Every class of the driver must have
**`reclaimPolicy: Delete`**, so that DeleteVolume always runs. The
`agentfs-class` admission policy (`deploy/csi/class-admission.yaml`, chart
value `classAdmission.enabled`) refuses any other.

The name must contain `{pvName}`, which the provisioner never reuses, so every
claim is a volume of its own: a claim deleted and created again is a new
volume, and the old one stays kept on its node.

**Static PVs.** A kept volume on a node can be mounted again through a PV
whose `csi.volumeHandle` is the directory's name, with node affinity to that
node, and whose `volumeAttributes` hold:

| Attribute | Meaning | Default |
|---|---|---|
| `profile` | required, as above | |
| `volumeName` | the agentfs volume name, literally (not a template); `volume` is the same key (it is what CreateVolume returns) | the volume handle |
| `onDelete` | as above | `retain` |

Any other key, except the CO's own (`csi.storage.k8s.io/…`,
`storage.kubernetes.io/…`), is refused (`InvalidArgument`), so a typo fails
rather than naming the volume after its handle. NodePublish adopts the volume
(writes `csi.json`) only when a tree volume of that name exists in the
directory on that node (another name: `AlreadyExists`); it never creates one.
Copy `name` from the volume's `csi.retained.json`.

```yaml
apiVersion: v1
kind: PersistentVolume
metadata:
  name: research-notes
spec:
  capacity: { storage: 1Gi }
  accessModes: [ReadWriteOncePod]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: ""
  nodeAffinity:
    required:
      nodeSelectorTerms:
        - matchExpressions:
            - key: topology.agentfs.mcurcio.github.io/node
              operator: In
              values: [<the node that holds it>]
  csi:
    driver: agentfs.mcurcio.github.io
    volumeHandle: pvc-0b6f...      # the directory under that node's base path
    volumeAttributes:
      profile: example
      volumeName: research.notes.pvc-0b6f...   # csi.retained.json's claim.name
```

## 6. Using a volume

See `deploy/csi/example/deployment.yaml` and `pvc.yaml`.

- **One pod per claim.** A claim's view serves one pod. A second pod's
  NodePublish of the same claim is refused (`FailedPrecondition`, a
  `FailedMount` event: `volume <pv> is published at <target> on this node: one
  pod per volume; several pods on one volume are not built`). Use
  **`ReadWriteOncePod`**, so the scheduler itself keeps a second pod off the
  claim, and a Deployment **`strategy: Recreate`**: a RollingUpdate starts the
  new pod before the old one stops, and on a `ReadWriteOnce` claim it stalls on
  that refusal. To give another pod the same files, use a fork claim (§14).
- **Access modes.** `ReadWriteOncePod` (`SINGLE_NODE_SINGLE_WRITER`) or
  `ReadWriteOnce` (`SINGLE_NODE_WRITER`, `SINGLE_NODE_MULTI_WRITER`); a
  read-only mount of either is `SINGLE_NODE_READER_ONLY`, and its view is
  read-only. Every multi-node mode is refused: the claim stays Pending with a
  `ProvisioningFailed` event that says `multi-node access modes are not
  supported`.
- **One node.** With `WaitForFirstConsumer` (the default), the first pod is
  scheduled first, and its node creates the volume. The PV's node affinity
  then pins every later pod of the claim to that node: draining it leaves the
  pods `Pending` until its plugin returns. Moving a volume is not built
  ([moving](runbooks/moving.md)).
- **fsGroup.** The CSIDriver's `fsGroupPolicy: File` lets the kubelet apply the
  pod's `fsGroup` through the mount. Use `fsGroupChangePolicy:
  OnRootMismatch`, so the walk happens once, not at every pod start (each chown
  is a recorded change). A view gives each new entry its caller's uid and gid,
  and a set-group-ID directory's group.
- **mountPropagation: HostToContainer.** Set it on the volume mount. When a
  host crashes, its restart stacks a new view over the dead one at the
  kubelet's target; with `HostToContainer` the container sees the new mount at
  once, without it only after the container restarts (§8). It needs no
  privilege and is allowed under `restricted`.
- **Identity.** `podInfoOnMount` gives each view its pod's name, namespace and
  uid, and every save carries them as trailers, with the node
  ([trees](trees.md#volume-identity-and-trailers)).

## 7. What each CSI call does

| Call | Does | Fails with |
|---|---|---|
| CreateVolume | Validates the parameters and capabilities; runs `agentfs --state <base>/<pv>/state init --volume <name> --include <profiles>/<profile>.config` (Volume's ensure: it creates the tree volume or finds it under the same name), installs `<profile>.exclude` and `.attributes` as `state/history.git/info/{exclude,attributes}`, writes `csi.json`, and answers this node's topology. Idempotent for the same name, capacity and parameters. A fork class with a `dataSource` claim makes a fork claim (§14) | `InvalidArgument` (bad parameters: a `volumeName` without `{pvName}`, an unknown key, `onDelete: delete`, `startFrom`, `workloadMounts`; a multi-node access mode, block volumes, mount flags; a content source on an ordinary class, a snapshot), `ResourceExhausted` (the accessibility requirements exclude this node), `AlreadyExists` (same name, other capacity or parameters; a recording-format volume in the directory) |
| DeleteVolume | A root claim: starts the volume's host with no view and stops it, so Volume drains the volume to Released; then renames `csi.json` to `csi.retained.json` and logs `kept volume <pv> at <dir>: ...`. The directory and its history stay on the node: deleting a tree volume is not built, because its history is pushed nowhere yet. A fork claim: ends its fork by key (§14). An unknown volume is a success | `FailedPrecondition` while any claim of the volume is published on this node (`volume <pv> is in use on this node (published at <target>); nothing deleted`), and for a recording-format volume (below) |
| ControllerPublish, ControllerUnpublish | Not offered (`Unimplemented`): there is no attach step (`attachRequired: false`) | |
| NodeStage, NodeUnstage | Not offered (`Unimplemented`): the plugin does not advertise `STAGE_UNSTAGE_VOLUME`, and NodePublish opens the view | |
| NodePublish | Resolves the claim (its `csi.json`, or a static PV over a kept volume), starts the volume's host if none runs, asks it to open a view of the claim's tree at the target with the pod's context (read-only for a read-only mount), and records it in `<nodeState>/published/<pv>.json` | `FailedPrecondition` (another pod has the claim: `several pods on one volume are not built`; a recording-format volume), `AlreadyExists` (the target is published with another read-only flag; a static PV naming another volume than the directory's), `NotFound` (no volume in the directory on this node: the pod waits), `InvalidArgument` (an inline ephemeral volume; a multi-node access mode), `Aborted` (another publish or unpublish of the claim is still running on this node), `FailedPrecondition` (the volume's host speaks another host protocol version; the restarted host refused a view opened on the host before it) |
| NodeUnpublish | Asks the host to close the view (or detaches the target when no host runs), removes the record and the target directory, and stops the host once the volume is idle (no view recorded and no open in flight). Unknown targets are a success | `Aborted` (another publish or unpublish of the claim is still running on this node, such as a close that waits for a held file) |
| ValidateVolumeCapabilities | Confirms single-node mount capabilities; a multi-node mode is not confirmed, with the same message | `NotFound` for an unknown volume |
| NodeGetInfo | The node id and its topology (`topology.agentfs.mcurcio.github.io/node`), which the kubelet puts on the node as a label | |
| NodeGetCapabilities, Probe, GetPluginInfo, GetPluginCapabilities | as the spec says; the plugin offers the Controller service and volume accessibility constraints | |

Snapshots, expansion, volume stats and listing are not offered; clones only
into a fork class (§14). No call pushes, fetches or lands patches: there is no
release scan, no push and no patch landing.

**Recording-format volumes are never served.** A directory under the base path
whose state area is the recording engine's format (a volume made by an older
driver) is found by that format. NodePublish and DeleteVolume answer
`FailedPrecondition`:

```text
volume <pv> is a recording-format volume (<base>/<pv>): this driver never serves or deletes it; archive it and remove the directory by hand (docs/runbooks/moving.md)
```

and the plugin logs each such directory at start. Its pods wait in
`ContainerCreating`; its PV stays `Released` with `VolumeFailedDelete` warning
events, and kube-state-metrics' `kube_persistentvolume_status_phase` can alert
on a PV left `Released`. Once the operator has archived the directory and
removed it by hand ([moving](runbooks/moving.md)), the provisioner's next retry
of DeleteVolume succeeds.

## 8. Recovery

- **A host crashes** (`kill -9`, a bug): its pods' mounts answer `ENOTCONN`.
  The plugin starts it again with backoff (1 s, doubling to `--backoff-max`,
  30 s by default). The new host replays every published view of the volume
  whose target is still there: it stacks each view over the stale mount at the
  kubelet's target (the dead mount cannot be unmounted under a running
  container), and rebinds each view's client (same launcher, tree, label and
  access), so a fork keeps its one writer. It then reconciles: what no view
  uses any more is ended. Containers with `mountPropagation: HostToContainer`
  see the new mount at once; others after a container restart. The crashed
  host's git children share the volume's lease until each dies of its
  parent-death signal, so the new host may exit 75 (busy): the plugin then
  waits for the lease to be free, not a timer, and starts it again. A restart
  that cannot start otherwise (a full disk, PID pressure) is tried again with
  the same backoff while the volume has records.
- **The node plugin restarts** (an upgrade, a crash): its hosts' control
  channels close, whether it stopped on SIGTERM or died. A host with no view
  open drains and exits 0. A host with views open ends its git children, so
  the volume's lease is free when it exits, and exits with status 3, leaving
  each view's mount stale and its client attached, as a crash does:
  the pods' mounts answer `ENOTCONN` meanwhile, and a fork claim keeps its one
  writer. Every change a view took is already in the volume's journal. The new
  plugin reconciles from `<nodeState>/published/`: records whose target the
  kubelet removed are dropped, and every other volume gets a host that replays
  its views, stacked over the stale mounts, as for a crashed host; one that
  exits 75 is started again once the volume's lease is free. Because this interrupts the node's mounts, the
  DaemonSet uses `updateStrategy: OnDelete`: roll it node by node
  ([runbook](runbooks/csi-recovery.md)).
- **The node reboots**: the kubelet publishes the volumes of the pods it
  restarts; the records left from before the reboot are reconciled.
- **The node is lost**: its volumes stay on its disk and its pods cannot run
  elsewhere. Moving a volume is not built: copy what you need out of a mount
  while you can ([moving](runbooks/moving.md)).
- **A volume's directory is gone** (a wiped disk): NodePublish answers
  `NotFound`, and the pod waits, its events naming the node.

What a crash loses is what the tree engine says
([trees](trees.md#recording-and-durability)): every call that returned was
recorded in the journal first.

## 9. Operations

**Finding a volume.** `kubectl get pv <pv> -o yaml` names its node in
`nodeAffinity`, and its agentfs volume name in `spec.csi.volumeAttributes`.
Run commands in that node's plugin pod:

```sh
NODE_POD=$(kubectl -n agentfs-csi get pod -l app.kubernetes.io/name=agentfs-csi-node \
  --field-selector spec.nodeName=<node> -o name)
```

**The history.** `git` is in the image. The volume's history is
`<base>/<pv>/state/history.git`; the root's branch is `root`, a fork's
`fork/<n>` ([trees](trees.md#what-a-commit-holds)):

```sh
kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- \
  git --git-dir /var/lib/agentfs-csi/volumes/<pv>/state/history.git log root
```

**The host process.** Each volume in use has one `agentfs ... host` process in
the plugin's container:

```sh
kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- pgrep -af ' host --launcher csi'
kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- agentfs probe <target>
```

A view at a kubelet target has the mount source `agentfs:<volume>` (`findmnt`
on the node).

**Logs.** `kubectl logs` of the node plugin's `agentfs-csi` container holds the
plugin's own lines and every child's, prefixed with `[host <pv>]` or
`[init <pv>]`: `published volume <pv> at <target>`, `unpublished volume ...`,
`[host <pv>] exited: ...; restarting in 1s`, `kept volume ...`, and at each host
start one `not built: <key> (<capability>): ...` line per setting not built.
The provisioner's container, `csi-provisioner`, logs each CreateVolume and
DeleteVolume answer.

**Telemetry.** The plugin passes its `OTEL_*` variables on to every host,
which exports its metrics, the `agentfs.not_built` gauge per volume among
them ([telemetry](telemetry.md)); the alert `AgentfsNotBuilt` fires on it.
`deploy/csi` sets `OTEL_SDK_DISABLED=true`: point the variables at your
collector to turn it on. The plugin itself exports nothing.

**Kept volumes.** A deleted root claim's directory stays on its node with
`csi.retained.json` beside its state. Mount it again through a static PV
(§5), or, once its history is archived (a `git bundle` of
`state/history.git`), remove `<base>/<pv>` by hand.

**Upgrading the node plugin.** A node plugin's restart stops that node's hosts
(their FUSE sessions cannot be handed to another process), so the DaemonSet is
`updateStrategy: OnDelete`: upgrade node by node, stopping the node's agentfs
pods first ([runbook](runbooks/csi-recovery.md#upgrading-the-node-plugin)).

## 10. Security

**RBAC inventory** (`deploy/csi/rbac.yaml`). Nothing reads Secrets, pods or
snapshots through the API.

| Subject | Role | Resources | Verbs | Why |
|---|---|---|---|---|
| `agentfs-csi-node` (csi-provisioner in each node plugin pod) | ClusterRole `agentfs-csi-provisioner` | persistentvolumes | get, list, watch, create, patch, delete | create PVs for claims, delete them after DeleteVolume |
| | | persistentvolumeclaims | get, list, watch, update | watch claims, set annotations and finalizers |
| | | storageclasses | get, list, watch | read the parameters |
| | | events | list, watch, create, update, patch | report provisioning |
| | | csinodes, nodes | get, list, watch | `WaitForFirstConsumer` and per-node topology |

The node plugin pod sets `automountServiceAccountToken: false` and mounts a
projected token only into csi-provisioner; the `agentfs-csi` container never
holds one. RBAC cannot scope PVs by driver or node: the ClusterRole alone would
let a compromised node create, patch or delete **any** PV in the cluster,
other drivers' included. The admission policy `agentfs-provisioner`
(`deploy/csi/provisioner-admission.yaml`, always installed) narrows it. For
requests from the node plugin's account:

| Request | Admitted only when |
|---|---|
| create, change or delete a PV | the PV (before and after) is this driver's (`spec.csi.driver`), and its node affinity pins it with `topology.agentfs.mcurcio.github.io/node In [<node>]`, where `<node>` is the node the pod-bound token names (`authentication.kubernetes.io/node-name`, Kubernetes 1.30 and later) |
| update a claim | its `volume.kubernetes.io/storage-provisioner` (or the beta) annotation names this driver |

What is left: a compromised node can still create and delete this driver's PVs
on its own node (whose volumes it holds anyway) and change this driver's claims
anywhere. Keep the node plugin's namespace to cluster administrators.

**Pod Security inventory.**

| Namespace | Pod | Level | Why |
|---|---|---|---|
| `agentfs-csi` (enforce `privileged`, warn and audit `baseline`) | node plugin `agentfs-csi` | privileged, root, `hostPID` | FUSE `mount(2)`, the workloads' real pids so views know their callers' supplementary groups, `Bidirectional` propagation into `/var/lib/kubelet`, stacking a view over a stale mount (`CAP_SYS_ADMIN`), hostPath volumes, giving new files to their callers (`CAP_CHOWN`, `CAP_FOWNER`) |
| | csi-provisioner, node-driver-registrar, livenessprobe | root, no capabilities, read-only root filesystem | reach the root-owned 0660 CSI socket |
| workloads (e.g. `agentfs-example`, enforce `restricted`) | anything | restricted | they only mount a claim |

**The control channel.** A host takes requests only on its standard input,
one end of a socket pair the plugin made when it started the host. It has no
socket path, so nothing else on the node, and nothing in a pod, can reach it.
A pod sees only its view: no state area, history, journal or credential is
reachable through the mount.

**Who can change what.**

- Operators (who can edit the `agentfs-csi` namespace and StorageClasses)
  choose profiles, rules and credentials.
- Pod authors choose only a StorageClass and their pods' names: they see no
  state area, no credentials and no settings, and cannot read or rewrite
  history.
- Anyone who can create pods with a hostPath, or exec into the node plugin,
  can read every volume's history and credentials on that node: keep the
  `agentfs-csi` namespace to cluster administrators.

**Restricting who uses a class.** A StorageClass is cluster-wide. Limit a
namespace with a ResourceQuota on
`<class>.storageclass.storage.k8s.io/persistentvolumeclaims` (for example `0`
where agentfs volumes are not allowed).

## 11. Where volumes live

Each node keeps its volumes under its own `--base-path`, on its own disk.

- **Any local directory.** The chart's `basePath.path` is a hostPath on every
  node. Prefer a disk of its own: sizes are not enforced, and a full disk makes
  the kubelet evict every pod on the node.
- **No network filesystem.** `agentfs-csi` refuses a base path on NFS (any
  version), SMB, CephFS, GlusterFS, Lustre, GPFS or AFS before it serves
  anything, naming the filesystem type.
- **A volume never moves.** Moving a tree volume to another node is not built,
  and a claim on a class with `startFrom` is refused. Copying a volume's files
  into a new claim on another node is a manual procedure, and the history does
  not continue ([moving](runbooks/moving.md)).

## 12. Limitations

- One node per volume, for good; every multi-node access mode is refused.
- One pod per claim. A second pod on the same claim is refused (`several pods
  on one volume are not built`); a fork claim serves one pod for that pod's
  life (§14). A RollingUpdate on a `ReadWriteOnce` claim stalls on the refusal:
  use `ReadWriteOncePod` and `strategy: Recreate`.
- Not built, and refused: deleting a volume (`onDelete: delete`), `startFrom`,
  `workloadMounts`, unlinked forks, landing approvals and forks of forks.
- Not built, logged and gauged as not built at each host start: push, the
  patch queue and LFS.
- Kept volumes use disk until an operator removes them by hand.
- Draining a node leaves its agentfs pods `Pending` until its plugin returns.
  A fork claim's pod needs affinity to its source's pod (§14).
- No capacity enforcement, snapshots or expansion.
- A volume whose clients another launcher attached (an operator's
  `agentfs mount` of the volume that crashed, launcher `cli`) cannot be
  released by the driver: its host's drain ends only launcher `csi`'s
  clients. DeleteVolume, and a NodeUnpublish that stops the volume's last
  view, wait on that host until the CO's deadline; the host logs
  `tree DrainBlocked` with the clients it waits for
  ([runbook](runbooks/csi-recovery.md#a-host-never-exits)).
- Restarting the node plugin interrupts that node's mounts until it has opened
  them again (seconds); containers without `HostToContainer` propagation need a
  restart to see the new mount.
- No inline ephemeral volumes: a pod author would choose the parameters.
- Recording-format volumes made by an older driver are never served
  ([moving](runbooks/moving.md)).
- With `classAdmission.enabled` off, a class with `Retain` never calls
  DeleteVolume: its PVs stay `Released` for ever and fork claims never end.

## 13. How it is tested

- **Unit and integration tests** (`cargo nextest run -p agentfs-csi`): parameters
  (static PV attributes and every refusal), layout, records, the host client
  and the supervisor with a fake launcher and mounter (one pod per claim,
  restarts with backoff and views replayed, reconcile after a plugin restart).
- **The real binary** (`crates/agentfs-cli/tests/csi_trees.rs`): the Controller and
  Node services over a gRPC socket with real `agentfs init` and `agentfs host`
  children. Without root: CreateVolume makes a tree volume with the templated
  name, its profile included and its rules installed; DeleteVolume drains it
  through a host and keeps it; a recording-format volume is refused. With root
  and FUSE (run by `ci/fuse-tests.sh`): NodePublish mounts a view at the target,
  a write is committed with the pod's trailers, a second pod is refused, a
  killed host comes back with the view live, NodeUnpublish leaves nothing
  behind.
- **csi-sanity** (`ci/csi-sanity.sh`, the `csi-sanity` job of the ci workflow):
  csi-test against `agentfs-csi` with real `agentfs init` and `agentfs host`
  children and FUSE views, then no agentfs mount, published record or host left
  behind, every deleted volume kept, and each host's `not built:` line.
- **kind** (`ci/e2e-csi.sh`, the `e2e-csi` job of the image workflow, and
  against the chart in the helm workflow): the RBAC inventory with `kubectl
  auth can-i` and no token in the `agentfs-csi` container, the node's topology
  label and the PV's node affinity, the class and provisioner admission
  policies, a restricted writer whose `/data` is a view (source
  `agentfs:<volume>`) of its volume's one host, its files in the history with
  its pod's trailers, the profile's remote logged as not built and the git
  server given no ref, a second pod refused, `kill -9` of the host and deletion
  of the node plugin pod each recovered with a new write recorded, the refusals
  of inline volumes, `ReadWriteMany`, `onDelete: delete` and `startFrom`, and
  teardown: no view and no host left, the PV deleted and the directory kept
  with its history.
- **Fork claims on kind** (`ci/e2e-csi-forks.sh`, the `e2e-csi-forks` job of
  the image workflow): a fork claim of a source claim in use, served in the
  source's volume (its own directory holds only `csi.json`); the fork pod's
  write landing in the source with the fork pod's trailer, seen by the source's
  pod; the class admission policy; `link: unlinked` and `landing: approval`
  refused; deleting the fork claim ending the fork while the source keeps its
  files.
- **The f8 workflow, on kind:** `ci/e2e-csi-move.sh` (two workers: a volume
  recorded on its node only, a `startFrom` class refused, the deleted claim's
  directory kept with its history); `ci/e2e-chart-server.sh` (the chart's git
  server: the volume's remote logged as not built and no agentfs ref on the
  server, while the server itself takes a push); `ci/e2e-csi-fork-of-fork.sh`
  (a fork claim of a fork claim refused with `forks of forks are not built`,
  while the first fork keeps landing in its source).
- **Hermes** (`ci/e2e-hermes-kind.sh csi`, the hermes workflow): upstream
  Hermes on a claim of the driver with a `hermes` profile, its remote and patch
  source logged as not built, and a pod restart onto a new host that reads what
  Hermes wrote.
- **NFS**: the unit tests of `agentfs_os::shared_fs` check that a base path
  on a network filesystem is recognised from the mount table and refused,
  naming the type.
- **Not tested**: a base path on a real NFS export, Talos and other hardened
  node images, node reboots, the node plugin upgrade procedure, and the moving
  runbook as written.

## 14. Forks

A **fork claim** gives a second pod its own fork of a volume, whose saves land
in the source automatically ([trees](trees.md#forks-keys-end-and-the-drop)).
It is a claim whose `dataSource` is the source's claim, on a **fork class**: a
StorageClass with `fork: "true"` and reclaim policy `Delete`
([example](../deploy/csi/example/fork-storageclass.yaml)).

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata: { name: tool-call, namespace: agents }
spec:
  storageClassName: agentfs-fork
  dataSource: { kind: PersistentVolumeClaim, name: workspace }
  accessModes: [ReadWriteOncePod]
  resources: { requests: { storage: 1Gi } }
```

- **Parameters.** `fork: "true"`, and `landing`: `automatic` (the default) or
  `approval`, which Trees refuses (`landing approvals are not built`). `link:
  unlinked` and `remotes` are refused (`unlinked forks are not built`). A fork
  class refuses `profile`, `volumeName`, `onDelete`, `startFrom`, `branch` and
  `workloadMounts`: a fork claim has its source's volume and settings.
- **Create, on the source's node.** CreateVolume makes an in-volume fork of the
  source's root, owned by Trees under the fork claim's PV name as its key, at
  that point in time. The fork claim's own directory holds only `csi.json`.
  The source must be on the node the fork claim's pod is scheduled to: give
  that pod affinity to its source's pod. Its untracked area starts as a copy of
  the source's ([trees](trees.md#rules-and-the-untracked-area)). Kubernetes'
  documentation says a clone's source claim must not be in use; the
  external-provisioner this driver pins checks only that it is bound, not being
  deleted and of the same driver, and the kind job clones a claim a running pod
  writes to.
- **Served beside its source.** The source volume's host serves the fork's
  view, beside the source's pod, in one process. Each save of the fork pod
  carries that pod's trailers and lands in the source automatically, so the
  source's pod sees it.
- **One pod, for that pod's life.** The fork pod's view is the fork's one
  writer. A restarted host rebinds it, so a host crash or a node plugin
  restart keeps it (§8). Once that
  writer detaches (its pod ended), a new pod of the same fork claim is refused:
  the fork `has had its writer`.
- **Delete.** Deleting the fork claim (a pod-scoped ephemeral claim goes with
  its pod) calls DeleteVolume, which ends the fork by key; it drops once its
  last save has landed. Its directory goes with it.
- **Forks of forks** are refused: a fork claim whose `dataSource` is another
  fork claim gets `forks of forks are not built`.
- **Admission.** The class admission policy (`deploy/csi/class-admission.yaml`)
  refuses any StorageClass of this driver, a fork class too, whose reclaim
  policy is not `Delete`: with `Retain`, deleting a claim would never call
  DeleteVolume, and the fork would never end.
