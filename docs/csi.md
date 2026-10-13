# The agentfs CSI driver

`agentfs-csi` gives Kubernetes workloads agentfs volumes as ordinary
PersistentVolumeClaims: the pods mount a claim, and every change they
make is recorded and pushed as git history, by a recorder they cannot
see or configure. It is phase 4 of the design (design §14.2, §17.2); this
page is the operator's guide and reference.

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
- [11. Shared filesystems](#11-shared-filesystems)
- [12. Limitations](#12-limitations)
- [13. How it is tested](#13-how-it-is-tested)

## 1. What it is

There are two ways to run agentfs on Kubernetes:

| | Sidecar ([deployment.md](deployment.md)) | CSI driver (this page) |
|---|---|---|
| Who runs the recorder | a privileged native sidecar in every workload pod | the driver's node plugin, in its own namespace |
| Workload pods | privileged (`Bidirectional` propagation); their namespace must allow `privileged` | unprivileged; their namespace can enforce `restricted` |
| Pods per volume | one (`ReadWriteOncePod`) | several on one node (`ReadWriteOnce`), each committing as itself |
| Settings | a ConfigMap and Secret in the workload's namespace | profiles chosen by StorageClass, in the driver's namespace only |
| Crash recovery | a pod restart, by hand | automatic: the plugin restarts the crashed front or service |

The image is the same (`ghcr.io/mcurcio/agentfs`): it ships `agentfs`,
`agentfs-csi` and `tini`. The driver's name is `agentfs.mcurcio.github.io`.

## 2. Architecture

```
 controller Deployment (1 replica)          node DaemonSet (every node)
 ┌──────────────────────────────┐           ┌──────────────────────────────────────────────┐
 │ csi-provisioner  csi-attacher│           │ node-driver-registrar  livenessprobe         │
 │        │             │        │           │ tini ── agentfs-csi --mode node              │
 │ agentfs-csi --mode controller│           │          ├─ agentfs serve-history   (per volume)
 │   CreateVolume: basePath/<pv>│           │          ├─ agentfs mount --history-service (per pod)
 └──────────────────────────────┘           │          └─ ...                              │
                                            └──────────────────────────────────────────────┘
```

**The controller** turns a claim into a volume directory and tracks which
node a volume is attached to:

- `csi-provisioner` calls CreateVolume and DeleteVolume;
- `csi-attacher` calls ControllerPublish and ControllerUnpublish;
- `agentfs-csi --mode controller` does the work, with no API access of its
  own.

**The node plugin** runs on every node, as root and privileged, and talks
only to the kubelet. It supervises child processes of the `agentfs`
binary, exactly the ones the single-node tests exercise:

- one `agentfs serve-history` per staged volume (the volume's single writer
  of history, design §2.3), started at NodeStage;
- one front, `agentfs mount --history-service`, per pod using the volume,
  started at NodePublish with that pod's name, namespace and uid. The front
  mounts FUSE at a private source directory, which is bind-mounted onto the
  kubelet's target.

A crashed child does not take the plugin down; the plugin restarts it
(§8). `tini` is the container's PID 1 and reaps what the children leave.

**On disk.** Under `--base-path` (default `/var/lib/agentfs-csi/volumes`),
one directory per volume, named after the PV:

```
<basePath>/<pv-name>/
  backing/        the files the pods see (owned by the pods' uids)
  state/          the agentfs state area: history, journal, pending/
  csi.json        the volume's parameters and the node it is attached to
                  (csi.retained.json once deleted with onDelete: retain)
<basePath>/.nodes/<node>   written by each node plugin at start: ControllerPublish
                           refuses nodes without one (NotFound)
```

Under `--node-state` (default `/var/lib/agentfs-csi/node`, a hostPath
that survives plugin restarts), the node's records, from which a
restarted plugin rebuilds everything:

```
<nodeState>/run/<pv>/history.sock        the volume's history service
<nodeState>/staged/<pv>.json             a staged volume
<nodeState>/published/<pv>/<key>.json    a published target: pod, slot, front pid
```

`--mount-dir` (default `/run/agentfs-csi/mounts`, an emptyDir) holds the
fronts' FUSE mounts, `<mountDir>/<pv>/<key>`, where `<key>` is a hash of
the target path.

## 3. Requirements

- **FUSE** on every node (`/dev/fuse`; `modprobe fuse`). Kernel 6.9 or later
  for FUSE passthrough (`--passthrough auto` falls back without it).
- **basePath on one filesystem**, seen by the controller and every node
  at the same path. The default hostPath is right for a single-node
  cluster, or when the controller is pinned to the node that runs the
  workloads. With several nodes, mount one shared filesystem at basePath
  on every node (§11).
- **One node per volume at a time.** `ReadWriteOnce` and
  `ReadWriteOncePod` only; `ReadWriteMany` is refused. Several pods on one
  node share a volume.
- **A privileged namespace for the driver** (`agentfs-csi`, Pod Security
  `privileged`). Workloads need no privilege.
- The CSI sidecars from `registry.k8s.io/sig-storage` (pinned by digest in
  `deploy/csi/kustomization.yaml`): csi-provisioner v6.4.0, csi-attacher
  v4.13.0, csi-node-driver-registrar v2.18.0, livenessprobe v2.20.0.

## 4. Install

```sh
# 1. Edit the profiles (deploy/csi/profiles) and create the credentials.
kubectl apply -f deploy/csi/namespace.yaml
kubectl -n agentfs-csi create secret generic agentfs-csi-credentials \
  --from-literal=example-origin=$'username=agentfs\npassword=<token>'
# 2. Install the driver (drop credentials.yaml from the kustomization when
#    the Secret is created out of band).
kubectl apply -k deploy/csi
# 3. A workload: a StorageClass, a claim and two replicas.
kubectl apply -k deploy/csi/example
```

`deploy/csi` installs only the driver; `deploy/csi/example` is a
workload. Pin the agentfs image to a commit tag or digest in production
(the kustomization's `images`).

**Profiles.** A profile is a set of files in the `agentfs-csi-profiles`
ConfigMap, mounted into the driver's pods at `/etc/agentfs-csi/profiles`:

| File | Becomes | Required |
|---|---|---|
| `<profile>.config` | a git-config fragment included by every volume's state area (design §4.3): settings and remotes | yes |
| `<profile>.exclude` | the volume's `info/exclude` (design §4.1) | no |
| `<profile>.attributes` | the volume's `info/attributes` (design §4.2) | no |

A fragment must not set `agentfs.volume` or `agentfs.branch`; the driver
sets them per volume. The ConfigMap has no hash suffix, so edits reach
the running pods without restarts: a state area reads its included
config at every open, and NodeStage reinstalls the rules each time a
volume is staged.

**Credentials.** The `agentfs-csi-credentials` Secret is mounted into the
node plugin at `/etc/agentfs-csi/credentials` (mode 0400). A profile names
a key: `agentfsCredentials = /etc/agentfs-csi/credentials/<key>` (or
`agentfsSshKey`, `agentfsKnownHosts`; docs/configuration.md). The kubelet
refreshes the files when the Secret changes, and agentfs re-reads them at
each push: rotation needs no restart.

## 5. Parameters

A StorageClass's `parameters`:

| Parameter | Meaning | Default |
|---|---|---|
| `profile` | required: the profile (`<profile>.config` must exist); a DNS label | |
| `volumeName` | the agentfs volume name (commit trailers, identity templates), a template over `{pvcNamespace}`, `{pvcName}` and `{pvName}` | `{pvcNamespace}.{pvcName}.{pvName}` (the provisioner runs with `--extra-create-metadata`), else the PV name |
| `branch` | the history branch | `agentfs/<volumeName>` |
| `onDelete` | what DeleteVolume does: `retain` keeps the directory (renaming `csi.json` to `csi.retained.json`), `delete` removes it, and refuses while a remote lacks commits of the branch (§9) | `retain` |

Keys under `csi.storage.k8s.io/` are the provisioner's and are otherwise
ignored; any other key is refused (`InvalidArgument`). The requested size
is recorded, not enforced.

**Volume names must be unique over time** (design §11.4). Each new
volume starts its branch with a fresh root commit, so a volume named
after one that already pushed to the same remote finds the remote branch
unrelated: every push is non-fast-forward, the service saves its head
under `refs/agentfs/<volume>/diverged/<timestamp>` and raises
`PushFailed` until an operator intervenes
([divergence runbook](runbooks/divergence.md)). That is what a claim
deleted and created again under the same name would do with
`{pvcNamespace}.{pvcName}`, whatever `onDelete` says, so the default
includes `{pvName}`, which the provisioner never reuses. A template
without `{pvName}` (or a fixed `branch`) is safe only if no claim of that
name is ever created twice; to carry a claim's history on, mount the
retained volume through a static PV instead.

**Static PVs.** An existing volume directory (one kept by `onDelete:
retain`, or prepared by hand) can be mounted through a PV whose
`csi.volumeHandle` is the directory's name and whose `volumeAttributes`
hold:

| Attribute | Meaning | Default |
|---|---|---|
| `profile` | required, as above | |
| `volumeName` | the agentfs volume name, literally (not a template); `volume` is the same key (it is what CreateVolume returns) | the volume handle |
| `branch` | the history branch | `agentfs/<volumeName>` |
| `onDelete` | as above | `retain` |

Any other key, except the CO's own (`csi.storage.k8s.io/…`,
`storage.kubernetes.io/…`), is refused (`InvalidArgument`), so a typo
fails rather than naming the volume after its handle. The first
ControllerPublish or NodeStage adopts the directory: it initialises the
state area if needed and writes `csi.json`. An existing state area must
hold the same volume and branch (else `AlreadyExists`): for a retained
volume, copy `volume` and `branch` from its `csi.retained.json`
(`params`).

```yaml
apiVersion: v1
kind: PersistentVolume
metadata:
  name: research-notes
spec:
  capacity: { storage: 1Gi }
  accessModes: [ReadWriteOnce]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: ""
  csi:
    driver: agentfs.mcurcio.github.io
    volumeHandle: pvc-0b6f...      # the directory under basePath
    volumeAttributes:
      profile: example
      volumeName: research.notes.pvc-0b6f...   # csi.retained.json's params.volume
```

## 6. Using a volume

See `deploy/csi/example/deployment.yaml`.

- **Access modes.** `ReadWriteOnce` (several pods on one node) or
  `ReadWriteOncePod`. Kubernetes enforces one node per volume through the
  attach step (`attachRequired: true`): a second node's VolumeAttachment
  is refused with a Multi-Attach error while the first node holds it.
- **fsGroup.** The CSIDriver's `fsGroupPolicy: File` lets the kubelet apply
  the pod's `fsGroup` through the mount. Use `fsGroupChangePolicy:
  OnRootMismatch`, so the walk happens once, not at every pod start (each
  chown is a recorded metadata change).
- **mountPropagation: HostToContainer.** Set it on the volume mount. When a
  front crashes, the plugin mounts its replacement over the dead one; with
  `HostToContainer` the container sees the new mount at once, without it
  only after the container restarts (§8). It needs no privilege and is
  allowed under `restricted`.
- **Several pods.** Every pod on the node gets its own front and pending
  slot; one history service orders their changes. Pods share the files with
  the backing filesystem's semantics (design §14): two pods writing one
  file is last-writer-wins, and POSIX locks are not shared between pods,
  so a SQLite database must be written from one pod.
- **Identity.** `podInfoOnMount` gives each front its pod's name,
  namespace and uid. With the default templates (design §9) the committer
  is the pod name (`{replica}`), the committer email
  `<pod uid>@<volume>.agentfs.invalid`, the author `uid-<uid>` of the
  writing process, and every commit carries `Agentfs-Pod`,
  `Agentfs-Namespace` and `Agentfs-Node` trailers. A commit never mixes
  two pods' changes.

## 7. What each CSI call does

| Call | Does | Fails with |
|---|---|---|
| CreateVolume | Validates the parameters and capabilities; creates `<basePath>/<pv>/{backing,state}`, initialises the state area with the profile's config included and its rules installed, writes `csi.json`. Idempotent for the same name and capacity | `InvalidArgument` (bad parameters, `ReadWriteMany`, block volumes, mount flags), `AlreadyExists` (same name, other capacity or parameters; a state area of another volume) |
| DeleteVolume | `onDelete: delete` removes the directory, `retain` keeps it as `csi.retained.json`. An unknown volume is a success | `FailedPrecondition` while the volume is attached, or its state area's lock is held (a history service runs), or (`delete`) while a remote lacks commits of the branch (§9) |
| ControllerPublish | Records the node in `csi.json`; adopts a static PV | `NotFound` (unknown volume or node), `FailedPrecondition` (attached to another node) |
| ControllerUnpublish | Clears the node. An unknown volume is a success | |
| NodeStage | Reinstalls the profile's rules, starts the volume's history service, and waits up to `--stage-wait` for it to listen (it recovers the journal and applies queued patches first) | `Unavailable` when the service is still starting (the kubelet retries; the service keeps running), `FailedPrecondition` when it exited (with its last log lines), `NotFound` for an unknown volume |
| NodePublish | Starts a front for the pod (its slot, identity and passthrough), waits up to `--publish-wait` for its mount, binds it onto the target (read-only for read-only mounts), records it | `FailedPrecondition` (not staged; a `SINGLE_NODE_SINGLE_WRITER` volume published elsewhere), `AlreadyExists` (the target is published with another read-only flag or access mode), `InvalidArgument` (inline ephemeral volume) |
| NodeUnpublish | Unmounts the target, stops the front (SIGTERM, SIGKILL after `--unpublish-grace`), removes the target directory and the record. Unknown targets are a success | |
| NodeUnstage | Stops the history service, which makes its final commit and pushes it once more to every remote that lacks commits (within `--push-grace`, 5 s), removes the socket and the record | `FailedPrecondition` while a target is still published |
| NodeGetInfo, NodeGetCapabilities, Probe, GetPluginInfo, GetPluginCapabilities, ValidateVolumeCapabilities | as the spec says | |

Snapshots, clones, expansion, volume stats and listing are not offered.

**Little network work in a call.** No call clones or fetches, and the
driver itself never pushes. The history service pushes in the background
on its interval (design §11.2), and once more as it stops, so NodeUnstage
waits up to `--push-grace` (5 s) for that final push. NodeStage only
waits, at most `--stage-wait`, for the service to listen; before
listening the service fetches the patch inbox (design §12), bounded by
`agentfs.patchFetchTimeout`, and applies queued patches.

## 8. Recovery

The plugin specifies and tests each of these (§13; design §14.2):

- **A front crashes** (`kill -9`, a bug): its pod's mount answers `ENOTCONN`.
  The plugin restarts the front with backoff (1 s, doubling to
  `--backoff-max`, reset after a minute of uptime) **in the same pending
  slot** and with the same pod identity, so it drains the slot's repair
  queue as that pod. The stale source mount is detached, and the new
  mount is bound **over** the stale one at the kubelet's target (the dead
  mount cannot be unmounted under a running container). Containers with
  `mountPropagation: HostToContainer` see the new mount at once; others
  after a container restart. Other pods on the volume are unaffected.
- **The history service crashes**: the fronts' boundaries fail with `EIO`
  and alarm (design §3.5) until it is back; the plugin restarts it with
  backoff, it recovers its journal (journaled records are committed under
  their original pod's committer) and the fronts reconnect by themselves.
- **A restart cannot start** (a failed fork under PID or memory pressure,
  a full disk): the plugin tries again with the same backoff until it
  starts, keeping the volume's records, so the volume recovers once the
  pressure is gone.
- **The node plugin restarts** (an upgrade, a crash): on SIGTERM it stops
  every front and service, which make their final commits; the mounts on
  the node answer `ENOTCONN` meanwhile. The new plugin reconciles from
  `--node-state`: it starts the staged volumes' services and recovers each
  published target as for a crashed front. Targets the kubelet removed
  meanwhile are forgotten. Because this interrupts the node's mounts, the
  DaemonSet uses `updateStrategy: OnDelete`: roll it node by node
  ([runbook](runbooks/csi-recovery.md)).
- **The node reboots**: the kubelet re-stages and re-publishes the
  volumes of the pods it restarts; the records left from before the
  reboot are reconciled (services restarted, targets that no longer exist
  forgotten).

What is lost in a crash is the same as for a single process (design
§3.5): boundaries that failed with `EIO` keep their final state through
the repairs and the sweep, not their intermediate states.

## 9. Operations

**Status.** In the node plugin pod:

```sh
NODE_POD=$(kubectl -n agentfs-csi get pod -l app.kubernetes.io/name=agentfs-csi-node \
  --field-selector spec.nodeName=<node> -o name)
kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- \
  agentfs --state /var/lib/agentfs-csi/volumes/<pv>/state status
```

shows the service (`history:   serving fronts at unix:…`), each front's
slot, pod and pid, the remotes, and the journal. `git` is in the image:
`git --git-dir /var/lib/agentfs-csi/volumes/<pv>/state/git log`.

**Logs.** `kubectl logs` of the node plugin's `agentfs-csi` container
holds the plugin's own lines and every child's, prefixed with
`[service <pv>]` or `[front <namespace>/<pod> <key>]`. Restarts are logged
(`front for … exited (…); restarting in 2s in slot 3`).

**Restores** (design §15) run live, through the volume's history
service, from the node plugin pod of the node that stages it:

```sh
kubectl -n agentfs-csi exec <node plugin pod> -c agentfs-csi -- \
  agentfs --state /var/lib/agentfs-csi/volumes/<pv>/state restore <path> <commit>
```

The service has every pod's front record what is dirty in it, refuses a
path any pod holds open for writing (unless `--force`), writes the
volume and has every front drop its kernel's caches of what it wrote, so
each pod sees the restored files at once. Exit status 75 means a front
is not listening (an older plugin image, or one restarting): retry, or
restart that pod. With the volume unstaged, `agentfs restore` runs
directly instead, with `--root <basePath>/<pv>/backing` as well.

**Pushes after the last pod.** The service pushes on its interval while
the volume is staged. When the last pod goes, NodeUnstage stops it: it
makes its final commit and pushes once more to every remote that lacks
commits, within `--push-grace` (5 s). If that push fails (the remote is
down, the grace runs out), nothing pushes until the volume is staged
again. The volume's history is still in its state area, and:

- `onDelete: retain` keeps it, as always;
- `onDelete: delete` refuses DeleteVolume (`FailedPrecondition`, naming
  each remote and how many commits it lacks; the provisioner retries)
  rather than delete history no remote holds.

To finish such a deletion, push by hand while the volume is unstaged, in
the node plugin pod, which holds the credentials:

```sh
agentfs --state /var/lib/agentfs-csi/volumes/<pv>/state push
```

and the provisioner's next retry deletes the volume. To discard the
history instead, remove `<basePath>/<pv>` by hand. A diverged remote
(design §11.3) never takes the commits; resolve it first
([runbook](runbooks/divergence.md)).

## 10. Security

**RBAC inventory** (`deploy/csi/rbac.yaml`). Nothing reads Secrets, pods
or snapshots through the API.

| Subject | Role | Resources | Verbs | Why |
|---|---|---|---|---|
| `agentfs-csi-controller` (csi-provisioner) | ClusterRole `agentfs-csi-provisioner` | persistentvolumes | get, list, watch, create, patch, delete | create PVs for claims, delete them after DeleteVolume |
| | | persistentvolumeclaims | get, list, watch, update | watch claims, set annotations and finalizers |
| | | storageclasses | get, list, watch | read the parameters |
| | | events | list, watch, create, update, patch | report provisioning |
| | | csinodes, nodes | get, list, watch | `WaitForFirstConsumer` topology |
| `agentfs-csi-controller` (csi-attacher) | ClusterRole `agentfs-csi-attacher` | persistentvolumes | get, list, watch, patch | the attachment's PV and finalizer |
| | | csinodes | get, list, watch | node id of a node |
| | | volumeattachments | get, list, watch, patch | the attachments |
| | | volumeattachments/status | patch | report attach results |
| `agentfs-csi-controller` | Role `agentfs-csi-leader-election` (namespace `agentfs-csi`) | leases | get, watch, list, delete, update, create | sidecar leader election |
| `agentfs-csi-node` | none | | | the node plugin talks only to the kubelet; its token is not mounted |

The controller pod sets `automountServiceAccountToken: false` and mounts a
projected token only into the two sidecars that use the API; the
`agentfs-csi` container never holds one.

**Pod Security inventory.**

| Namespace | Pod | Level | Why |
|---|---|---|---|
| `agentfs-csi` (enforce `privileged`, warn and audit `baseline`) | node plugin `agentfs-csi` | privileged, root | FUSE `mount(2)`, `Bidirectional` propagation into `/var/lib/kubelet`, hostPath volumes, passthrough (`CAP_SYS_ADMIN`), giving files to their owners (`CAP_CHOWN`, `CAP_FOWNER`) |
| | node-driver-registrar, livenessprobe | root, no capabilities, read-only root filesystem | reach the root-owned 0660 CSI socket |
| | controller `agentfs-csi` | root with `DAC_OVERRIDE` and `FOWNER` only, read-only root filesystem, hostPath basePath | create volume directories; delete files the workloads' uids own |
| | csi-provisioner, csi-attacher, livenessprobe | root, no capabilities, read-only root filesystem, RuntimeDefault seccomp | reach the CSI socket |
| workloads (e.g. `agentfs-example`, enforce `restricted`) | anything | restricted | they only mount a claim |

**Who can change what.**

- Operators (who can edit the `agentfs-csi` namespace and StorageClasses)
  choose remotes, rules, credentials and identity templates.
- Pod authors choose only a StorageClass and their pods' names: they see
  no state area, no credentials and no settings, and cannot read or
  rewrite history.
- Anyone who can create pods with a hostPath, or exec into the node
  plugin, can read every volume's history and credentials: keep the
  `agentfs-csi` namespace to cluster administrators.

**Restricting who uses a class.** A StorageClass is cluster-wide. Limit a
namespace with a ResourceQuota on
`<class>.storageclass.storage.k8s.io/persistentvolumeclaims` (for example
`0` where agentfs volumes are not allowed).

## 11. Shared filesystems

Untested; what agentfs needs from a filesystem at basePath shared between
nodes:

- **Working `flock`** for the state-area lock and the front slot leases
  (design §2.3, §14). On NFS that means NFSv4, or NFSv3 with NLM; never the
  `nolock` mount option, which makes every lock succeed and would let two
  services write one history.
- **No reflinks across nodes**: freezes fall back to copies (design §3.2).
- **Passthrough** is decided per inode: a backing file the kernel refuses
  to register falls back to ordinary FUSE I/O for that inode (design
  §17.1).

The driver still runs one node per volume; a shared filesystem only lets
a volume move between nodes (with the pods that use it). A cross-node
transport for fronts on several nodes is open (design §19.2).

## 12. Limitations

- One node per volume at a time; no `ReadWriteMany`.
- No capacity enforcement, snapshots, clones or expansion.
- basePath must be one filesystem visible at the same path to the
  controller and every node.
- Restarting the node plugin interrupts that node's mounts until it has
  recovered them (seconds); containers without `HostToContainer`
  propagation need a restart to see the recovered mount.
- A restore runs on the node that stages the volume, from its node plugin
  pod (§9).
- No inline ephemeral volumes: a pod author would choose the parameters.
- The plugin's flags (`--stage-wait`, `--publish-wait`, the graces) are
  per node, not per volume.

## 13. How it is tested

- **Unit and integration tests** (`cargo test`): parameters (static PV
  attributes included), layout and records; the Identity and Controller
  services over a real gRPC socket, including the adoption of a retained
  volume through the documented attributes and DeleteVolume's refusal of
  unpushed history (`crates/agentfs-csi/tests/grpc.rs`); the supervisor
  with a fake launcher and mounter: staging, publishing, restarts with
  backoff in the same slot, restarts whose spawn fails and are tried
  again, bind stacking, reconcile after a restart (also when a service
  cannot start yet), a pending publish not holding up another pod's
  recovery, a republish with other flags, unpublish and unstage
  ordering, shutdown (`tests/supervisor.rs`); and the supervisor driving
  a real `agentfs serve-history` without FUSE: two pods' commits, a
  `kill -9` recovered with its journaled commit, the final commit at
  unstage and its final push (`crates/agentfs/tests/csi_service.rs`).
- **csi-sanity** (`ci/csi-sanity.sh`, the `csi-sanity` job): csi-test
  v5.6.0 against `agentfs-csi --mode all` with real history services and
  FUSE fronts, then no mount, record or volume directory left behind.
- **kind** (`ci/e2e-csi.sh`, the `e2e-csi` job in the image workflow, with
  the image just built): the RBAC inventory with `kubectl auth can-i`, two
  restricted pods on one claim with distinct committers and pushes to a
  git server, `kill -9` of a front, of the service and deletion of the
  node plugin pod, each recovered, the refusals of inline volumes and
  `ReadWriteMany`, and teardown: the last commit reaches the remote by
  the final push at NodeUnstage, then DeleteVolume removes the volume.
- **Not tested**: several nodes, shared filesystems (NFS and others),
  Talos and other hardened node images, node reboots.
