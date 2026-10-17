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
- [11. Where worktrees live](#11-where-worktrees-live)
- [12. Limitations](#12-limitations)
- [13. How it is tested](#13-how-it-is-tested)
- [14. Forks](#14-forks)

## 1. What it is

There are two ways to run agentfs on Kubernetes:

| | Sidecar ([deployment.md](deployment.md)) | CSI driver (this page) |
|---|---|---|
| Who runs the recorder | a privileged native sidecar in every workload pod | the driver's node plugin, in its own namespace |
| Workload pods | privileged (`Bidirectional` propagation); their namespace must allow `privileged` | unprivileged; their namespace can enforce `restricted` |
| Pods per volume | one (`ReadWriteOncePod`) | several on one node (`ReadWriteOnce`), each committing as itself; never several nodes |
| Where the files live | the pod's volume | a **worktree** on the disk of the node that created the claim, for good: the volume's files and its own branch `agentfs/<volume>` |
| Settings | a ConfigMap and Secret in the workload's namespace | profiles chosen by StorageClass, in the driver's namespace only |
| Crash recovery | a pod restart, by hand | automatic: the plugin restarts the crashed front or service |

One branch per worktree, for good (design §19.1, 2026-10-16): a worktree
writes only its own branch, creating it or fast-forwarding it, and a
claim made again is a new worktree with a new branch. Continuing a
volume's work on another node is a new volume **started from** the old
branch ([moving](runbooks/moving.md)); which branch is current is the
operator's choice.

The image is the same (`ghcr.io/mcurcio/agentfs`): it ships `agentfs`,
`agentfs-csi` and `tini`. The driver's name is `agentfs.mcurcio.github.io`.

## 2. Architecture

```
 node DaemonSet (every node)
 ┌──────────────────────────────────────────────────────────────────┐
 │ csi-provisioner (--node-deployment)  node-driver-registrar        │
 │ livenessprobe                                                     │
 │ tini ── agentfs-csi        Identity, Controller and Node services │
 │          ├─ agentfs serve-history          (per staged volume)    │
 │          ├─ agentfs mount --history-service (per pod)             │
 │          └─ agentfs push / fork finish     (the release scan)     │
 └──────────────────────────────────────────────────────────────────┘
```

There is no controller Deployment and no attach step. **Every node
plugin** serves all three CSI services, beside its own csi-provisioner in
`--node-deployment` mode (design §19.1, 2026-10-16, R8):

- **csi-provisioner** provisions the claims whose pods the scheduler put
  on its node (`WaitForFirstConsumer`), and deletes the PVs of its node's
  worktrees. It is the only container with an API token.
- **CreateVolume** makes the worktree under this node's base path and
  answers this node's topology (`topology.agentfs.mcurcio.github.io/node`):
  the PV's node affinity keeps every later pod of the claim on this node.
  A start class fetches its start branch once (§5).
- **DeleteVolume** asks the one release decider (design §14.2): it lists
  each remote once and removes or keeps the worktree only when nothing
  would be lost (§7).
- **The node services** supervise child processes of the `agentfs`
  binary, exactly the ones the single-node tests exercise: one `agentfs
  serve-history` per staged volume (the volume's single writer of
  history, design §2.3), started at NodeStage; one front, `agentfs mount
  --history-service`, per pod using the volume, started at NodePublish
  with that pod's name, namespace and uid. The front mounts FUSE at a
  private source directory, which is bind-mounted onto the kubelet's
  target.
- **The release scan** pushes every worktree no pod stages on this node
  (between pods, after its claim was deleted, or kept), until a push
  exits 0 at its head, and finishes the forks whose deletion asked for it
  (§9, §14).

A crashed child does not take the plugin down; the plugin restarts it
(§8). `tini` reaps what the children leave: the plugin shares the host's
pid namespace (`hostPID: true`), so `tini` is not PID 1 and runs as a
subreaper (`-s`). The fronts need that namespace: FUSE reports a caller
in a pod's own pid namespace as pid 0, and its supplementary groups (the
pod's `fsGroup`) are then unknown (design §6).

**On disk.** Under `--base-path` (default `/var/lib/agentfs-csi/volumes`,
a hostPath on each node), one directory per worktree, named after the
PV:

```
<basePath>/<pv-name>/
  backing/        the files the pods see (owned by the pods' uids)
  state/          the agentfs state area: history, journal, pending/
  csi.json        the volume's parameters (csi.retained.json once
                  DeleteVolume kept the worktree)
<basePath>/.deleting.<pv-name>/   a worktree DeleteVolume is removing
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
- **basePath on each node's own disk**: any local directory, ideally a
  disk of its own (§11). A network filesystem is refused at start.
- **One node per volume, for good.** Several pods on one node share a
  volume (`ReadWriteOnce`, or `ReadWriteOncePod` for one pod). Every
  multi-node access mode (`ReadWriteMany`, `ReadOnlyMany`) is refused
  (design §19.1, 2026-10-15): pods on other nodes read the volume's
  history from the remote it pushes to.
- **A remote to move by.** A volume continues on another node only
  through a remote holding its branch ([moving](runbooks/moving.md)); a
  volume without one dies with its node's disk.
- **A privileged namespace for the driver** (`agentfs-csi`, Pod Security
  `privileged`). Workloads need no privilege.
- The CSI sidecars from `registry.k8s.io/sig-storage` (pinned by digest in
  `deploy/csi/kustomization.yaml`): csi-provisioner v6.4.0,
  csi-node-driver-registrar v2.18.0, livenessprobe v2.20.0.

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

With Helm instead: `charts/agentfs-csi`, published at
`oci://ghcr.io/mcurcio/charts/agentfs-csi` ([helm.md](helm.md)).

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

A fragment must not set `agentfs.volume`, `agentfs.branch` or the start
keys (`agentfs.start.*`); the driver sets a volume's identity in its own
state area, once. The ConfigMap has no hash suffix, so edits reach the
running pods without restarts: a state area reads its included config at
every open, and NodeStage reinstalls the rules each time a volume is
staged.

**Credentials.** The `agentfs-csi-credentials` Secret is mounted into the
node plugin at `/etc/agentfs-csi/credentials` (mode 0400). A profile names
a key: `agentfsCredentials = /etc/agentfs-csi/credentials/<key>` (or
`agentfsSshKey`, `agentfsSshKeyPassphrase`, `agentfsKnownHosts`;
docs/configuration.md). A passphrase-protected SSH key keeps its
passphrase file in the same Secret, beside the key. The kubelet
refreshes the files when the Secret changes, and agentfs re-reads them at
each push: rotation needs no restart.

**Pushing to the chart's git server.** With `server.enabled=true`, the
chart runs a minimal git server in its namespace and mounts its password
into the node plugin as the key `agentfs-server` ([helm.md](helm.md#the-git-server)).
A profile pushes every volume's branch there with:

```ini
[remote "server"]
    url = http://agentfs-git-server.agentfs-csi.svc:8080/git/history.git
    agentfsCredentials = /etc/agentfs-csi/credentials/agentfs-server
```

This is an ordinary remote: a profile may list it beside others (GitHub,
say) and may use it as its start remote when a volume moves
([runbooks/moving.md](runbooks/moving.md)).

## 5. Parameters

A StorageClass's `parameters`:

| Parameter | Meaning | Default |
|---|---|---|
| `profile` | required: the profile (`<profile>.config` must exist); a DNS label | |
| `volumeName` | the agentfs volume name (commit trailers, identity templates), a template over `{pvcNamespace}`, `{pvcName}` and `{pvName}` that must contain `{pvName}`. The branch is always `agentfs/<volumeName>` | `{pvcNamespace}.{pvcName}.{pvName}` (the provisioner runs with `--extra-create-metadata`), else the PV name |
| `onDelete` | what DeleteVolume does once nothing would be lost: `retain` keeps the worktree on its node (renaming `csi.json` to `csi.retained.json`), `delete` removes it. Either way DeleteVolume waits while a remote lacks the head (§7) | `retain` |
| `startFrom` | `<remote>:<branch>@<commit>`: a **start class**. A claim on it is a new worktree whose history begins at that commit of another worktree's branch (§11, [moving](runbooks/moving.md)) | none |
| `workloadMounts` | where pods mount the volume, comma-separated `PATH` or `PATH=DIR` (a `subPath` of the tree's directory `DIR`): passed to every front as `--workload-mount`, so the absolute `gitdir:` that `git worktree add` writes maps exactly to its repository (design §5). The node plugin cannot learn a container's `mountPath` otherwise; without it worktree paths are resolved by their suffixes and counted in `agentfs.nested.gitdir_heuristic` ([runbook](runbooks/worktree-heuristic.md)) | none |

Keys under `csi.storage.k8s.io/` are the provisioner's and are otherwise
ignored; any other key is refused (`InvalidArgument`), `branch` included.
The requested size is recorded, not enforced. Every class of the driver
must have **`reclaimPolicy: Delete`**: DeleteVolume must run for the
driver to decide what a released worktree needs, and `onDelete: retain`
is what keeps one. The `agentfs-class` admission policy
(`deploy/csi/class-admission.yaml`, chart value `classAdmission.enabled`)
refuses any other.

**One branch per worktree** (design §11.4, §14.2). The name must contain
`{pvName}`, which the provisioner never reuses, so every claim is a new
worktree with a new branch: a claim deleted and created again never
writes the old claim's branch. A worktree writes its branch only where
the remote has none, or holds a commit already on the worktree's own
history, as the remote reports it in the push's own connection; a branch
another writer moved is never overwritten: the head is saved under
`refs/agentfs/<volume>/diverged/<timestamp>` and `PushFailed` is raised
until an operator acts ([divergence runbook](runbooks/divergence.md)).

**Start classes.** `startFrom: origin:agentfs/<old volume>@<commit>`
names a push remote of the profile, a branch (never a `refs/...` name:
a `diverged/*` ref is refused, "not a branch"), and a full commit id that
is the branch's tip or an ancestor of it. CreateVolume fetches the branch
once from that remote, with the profile's credentials, writes the
commit's tree into the new worktree and points its own branch at it; the
start point is kept in the worktree's identity (`agentfs.start.*`,
[configuration](configuration.md)) and never changes. Its errors:
`InvalidArgument` (a bad `startFrom`, a remote the profile lacks, the new
volume's own branch), `FailedPrecondition` (no such branch, a commit not
on it), `Unavailable` (the remote could not be fetched, an LFS object
could not be downloaded; retried). A started volume then watches its
start branch at every push to that remote: commits it has not taken in
raise `StartBranchMoved` ([runbook](runbooks/start-branch-moved.md)).

**Static PVs.** An existing worktree on a node (one kept by `onDelete:
retain`) can be mounted through a PV whose `csi.volumeHandle` is the
directory's name, with node affinity to that node, and whose
`volumeAttributes` hold:

| Attribute | Meaning | Default |
|---|---|---|
| `profile` | required, as above | |
| `volumeName` | the agentfs volume name, literally (not a template); `volume` is the same key (it is what CreateVolume returns) | the volume handle |
| `onDelete` | as above | `retain` |
| `workloadMounts` | as above | none |

Any other key, except the CO's own (`csi.storage.k8s.io/…`,
`storage.kubernetes.io/…`), is refused (`InvalidArgument`), so a typo
fails rather than naming the volume after its handle. NodeStage adopts
the worktree (writes `csi.json`) only when it exists on that node and its
state area names the same volume (else `AlreadyExists`): for a retained
worktree, copy `volume` from its `csi.retained.json` (`params`). It never
creates one.

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
  nodeAffinity:
    required:
      nodeSelectorTerms:
        - matchExpressions:
            - key: topology.agentfs.mcurcio.github.io/node
              operator: In
              values: [<the node that holds it>]
  csi:
    driver: agentfs.mcurcio.github.io
    volumeHandle: pvc-0b6f...      # the directory under that node's basePath
    volumeAttributes:
      profile: example
      volumeName: research.notes.pvc-0b6f...   # csi.retained.json's params.volume
```

A static PV's own reclaim policy is not a class's: with `Retain` its PV
is never deleted by Kubernetes, and the release scan pushes its worktree
while no pod stages it.

## 6. Using a volume

See `deploy/csi/example/deployment.yaml`.

- **Access modes.** `ReadWriteOnce` (several pods on one node,
  `SINGLE_NODE_WRITER` or `SINGLE_NODE_MULTI_WRITER`) or
  `ReadWriteOncePod` (`SINGLE_NODE_SINGLE_WRITER`); a read-only mount of
  either is `SINGLE_NODE_READER_ONLY`. Every multi-node mode
  (`ReadWriteMany`, `ReadOnlyMany`) is refused with one message, which
  names the decision (design §19.1, 2026-10-15): the claim stays Pending
  with a `ProvisioningFailed` event that says `multi-node access modes are
  not supported`. A pod on another node that needs the files reads the
  volume's history from the remote it pushes to.
- **One node.** With `WaitForFirstConsumer` (the default), the first pod
  is scheduled first, and its node creates the worktree. The PV's node
  affinity then pins every later pod of the claim to that node: draining
  it leaves the pods `Pending` until its plugin returns, and moving the
  work elsewhere is a new volume ([moving](runbooks/moving.md)).
- **fsGroup.** The CSIDriver's `fsGroupPolicy: File` lets the kubelet apply
  the pod's `fsGroup` through the mount. Use `fsGroupChangePolicy:
  OnRootMismatch`, so the walk happens once, not at every pod start (each
  chown is a recorded metadata change).
  Pods whose primary group is not their `fsGroup` create files through a
  supplementary group: the node plugin's `hostPID` lets their fronts see
  it (design §6). A plugin deployed without it still works, but such
  creates fall back to root-then-chown
  (`agentfs.fuse.create_as_caller_fallbacks`), and two pods creating the
  same new name at once can see `EACCES`.
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
| CreateVolume | Validates the parameters and capabilities; creates `<basePath>/<pv>/{backing,state}` on this node, initialises the state area with the profile's config included (a start class: fetched and written from its start point, §5) and its rules installed, writes `csi.json`, and answers this node's topology. Idempotent for the same name, capacity and parameters. With a fork class and a `dataSource` claim, creates a fork of that volume (§14) and returns the content source | `InvalidArgument` (bad parameters: a `volumeName` without `{pvName}`, `branch`, a bad `startFrom`, an unknown key; any multi-node access mode, block volumes, mount flags; a content source on an ordinary class, a fork class without one, a snapshot; a start class naming a remote the profile lacks, or the volume's own branch), `ResourceExhausted` (the accessibility requirements exclude this node), `AlreadyExists` (same name, other capacity or parameters; a state area of another volume or start point), `FailedPrecondition` (a start branch missing, or not reaching the pinned commit; for a fork, a source not on this node, with no history, a fork, with no fork remote, a missing LFS object), `Unavailable` (the start remote could not be fetched; a fork's source could not be read; retried) |
| DeleteVolume | Asks the release decider over facts gathered here: whether the branch exists, each pushed remote's branch in a listing made for this call, `onDelete`, and for a fork its state. **Delete** (every remote holds the head, or there is no history, and `onDelete: delete`) moves the directory to `<basePath>/.deleting.<handle>` under the state lock, then removes it; **Keep** (`onDelete: retain` once every remote holds the head, or a profile with no remote) renames `csi.json` to `csi.retained.json` and leaves the worktree; **Wait** answers `Unavailable` naming the remote and why (it lacks the head, it could not be listed, its branch diverged; every remote section invalid; a fork not yet published) and wakes the release scan, which pushes it. A fork's deletion is its request to be finished (`finish_requested`, §14). DeleteVolume never pushes. An unknown volume is a success | `FailedPrecondition` while the volume is staged on this node, or its state lock is held (a history service, a push), or its settings cannot be read (nothing is deleted or released); `Unavailable` (Wait; a fork not yet published is answered so before the lock, which its finisher holds while it publishes) |
| ControllerPublish, ControllerUnpublish | Not offered (`Unimplemented`): there is no attach step (`attachRequired: false`) | |
| NodeStage | Checks the worktree: it never creates one. A worktree missing from this node's disk is `NotFound` naming the node; a static PV is adopted only over an existing worktree of its volume. Then sets the profile's include, reinstalls its rules, starts the volume's history service, and waits up to `--stage-wait` for it to listen (it recovers the journal and applies queued patches first) | `NotFound` (`no worktree on node <n> under <base>; the pod waits`), `Unavailable` when the service is still starting (the kubelet retries; the service keeps running), `FailedPrecondition` when it exited (with its last log lines), `AlreadyExists` (a static PV naming another volume than the worktree's), `InvalidArgument` (a multi-node access mode) |
| NodePublish | Starts a front for the pod (its slot, identity and passthrough), waits up to `--publish-wait` for its mount, binds it onto the target (read-only for read-only mounts), records it | `FailedPrecondition` (not staged; a `SINGLE_NODE_SINGLE_WRITER` volume published elsewhere), `AlreadyExists` (the target is published with another read-only flag or access mode), `InvalidArgument` (inline ephemeral volume; a multi-node access mode) |
| NodeUnpublish | Unmounts the target, stops the front (SIGTERM, SIGKILL after `--unpublish-grace`), removes the target directory and the record. Unknown targets are a success | |
| NodeUnstage | Stops the history service, which makes its final commit and pushes it once more to every remote that lacks commits (within `--push-grace`, 5 s), removes the socket and the record; the release scan pushes the worktree from then on | `FailedPrecondition` while a target is still published |
| ValidateVolumeCapabilities | Confirms single-node mount capabilities; a multi-node mode is not confirmed, with the same message | `NotFound` for an unknown volume |
| NodeGetInfo | The node id and its topology (`topology.agentfs.mcurcio.github.io/node`), which the kubelet puts on the node as a label | |
| NodeGetCapabilities, Probe, GetPluginInfo, GetPluginCapabilities | as the spec says; the plugin offers the Controller service and volume accessibility constraints | |

Snapshots, expansion, volume stats and listing are not offered; clones
only into a fork class (§14).

**Little network work in a call.** CreateVolume of a start class fetches
its start branch once; DeleteVolume lists each remote once. No call
pushes. A fork's CreateVolume copies a local volume's history and writes
its tree, with no network and no credentials. The history service pushes
in the background on its interval (design §11.2), and once more as it
stops, so NodeUnstage waits up to `--push-grace` (5 s) for that final
push; the release scan pushes an unstaged worktree after that. NodeStage
only waits, at most `--stage-wait`, for the service to listen; before
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
  meanwhile are forgotten, and its release scan pushes every worktree no
  pod stages once more. Because this interrupts the node's mounts, the
  DaemonSet uses `updateStrategy: OnDelete`: roll it node by node
  ([runbook](runbooks/csi-recovery.md)).
- **The node reboots**: the kubelet re-stages and re-publishes the
  volumes of the pods it restarts; the records left from before the
  reboot are reconciled (services restarted, targets that no longer exist
  forgotten).
- **The node is lost** (powered off, its disk gone, or being retired):
  its worktrees stay on its disk and its pods cannot run elsewhere. To go
  on, start a new volume from the old branch on another node
  ([moving runbook](runbooks/moving.md)). If the node comes back, its
  release scan pushes what it had not pushed to the old branch, and the
  new volume's start-branch alarm names those commits
  ([runbook](runbooks/start-branch-moved.md)). Nothing on two nodes ever
  writes one branch.
- **A worktree's directory is gone** (a wiped disk): NodeStage refuses
  the missing worktree with `NotFound`, and the pod waits, its events
  naming the node.

What is lost in a crash is the same as for a single process (design
§3.5): boundaries that failed with `EIO` keep their final state through
the repairs and the sweep, not their intermediate states.

## 9. Operations

**Status.** In the node plugin pod of the node that holds the worktree
(`kubectl get pv <pv> -o yaml` shows it in `nodeAffinity`):

```sh
NODE_POD=$(kubectl -n agentfs-csi get pod -l app.kubernetes.io/name=agentfs-csi-node \
  --field-selector spec.nodeName=<node> -o name)
kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- \
  agentfs --state /var/lib/agentfs-csi/volumes/<pv>/state status
```

shows the service (`history:   serving fronts at unix:…`), each front's
slot, pod and pid, the remotes, the journal, and for a started volume
its start point (`start:`) and, while it is raised, the start-branch
alarm (`start branch moved: <pinned>..<tip>`). `git` is in the image:
`git --git-dir /var/lib/agentfs-csi/volumes/<pv>/state/git log`.

**The LFS cache** (design §7) is the volume's
`<basePath>/<pv>/state/lfs`. `agentfs lfs fetch` and `agentfs lfs prune`
run in the same container, beside the history service, with `--state`
as above (the [LFS storage runbook](runbooks/lfs-storage.md)):

```sh
kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- \
  agentfs --state /var/lib/agentfs-csi/volumes/<pv>/state lfs prune --dry-run
```

**Logs.** `kubectl logs` of the node plugin's `agentfs-csi` container
holds the plugin's own lines and every child's, prefixed with
`[service <pv>]`, `[front <namespace>/<pod> <key>]`, `[push <pv>]` (the
release scan) or `[fork <pv>]`. Restarts are logged (`front for …
exited (…); restarting in 2s in slot 3`). The provisioner's container,
`csi-provisioner`, logs each CreateVolume and DeleteVolume answer.

**Restores** (design §15) run live, through the volume's history
service, from the node plugin pod of the node that stages it:

```sh
kubectl -n agentfs-csi exec <node plugin pod> -c agentfs-csi -- \
  agentfs --state /var/lib/agentfs-csi/volumes/<pv>/state restore <path> <commit>
```

The service has every pod's front record what is dirty in it, refuses a
path any pod holds open for writing (unless `--force`; the fronts are
asked again just before each path is written, so a file opened during a
long restore is refused at its own step), writes the
volume and has every front drop its kernel's caches of what it wrote, so
each pod sees the restored files at once. Exit status 75 means a front
is not listening (an older plugin image, or one restarting) or could not
record what is dirty in it: retry, or restart that pod. With the volume
unstaged, `agentfs restore` runs
directly instead, with `--root <basePath>/<pv>/backing` as well.

**Upgrading the node plugin.** The DaemonSet is `updateStrategy:
OnDelete`. A node plugin's restart stops that node's fronts and services
(its FUSE mounts are the plugin's children; a FUSE session cannot be
handed to another process), so upgrade node by node, draining first:

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

With the node's pods gone first, nothing is interrupted: their volumes
are unstaged (final commit and push) before the plugin restarts, and
their pods come back on this node once it is uncordoned. This procedure
is not run in CI.

**Pushes after the last pod.** The service pushes on its interval while
the volume is staged. When the last pod goes, NodeUnstage stops it: it
makes its final commit and pushes once more to every remote that lacks
commits, within `--push-grace` (5 s). From then on the node plugin's
**release scan** pushes the worktree (`agentfs push`), at once and every
`--release-scan-interval` (30 s), backing off after a failure, until a
push exits 0 at the branch's head; every failed attempt raises
`PushFailed` through that push's own alarms and telemetry. That covers a
worktree between pods, one whose claim was deleted (DeleteVolume waits
meanwhile, and wakes the scan), and one kept on the node. A diverged
remote (design §11.3) never takes the commits; resolve it first
([runbook](runbooks/divergence.md)); DeleteVolume keeps the worktree
until then. To discard a worktree's unpushed history instead, remove
`<basePath>/<pv>` on its node by hand.

## 10. Security

**RBAC inventory** (`deploy/csi/rbac.yaml`). Nothing reads Secrets, pods
or snapshots through the API.

| Subject | Role | Resources | Verbs | Why |
|---|---|---|---|---|
| `agentfs-csi-node` (csi-provisioner in each node plugin pod) | ClusterRole `agentfs-csi-provisioner` | persistentvolumes | get, list, watch, create, patch, delete | create PVs for claims, delete them after DeleteVolume |
| | | persistentvolumeclaims | get, list, watch, update | watch claims, set annotations and finalizers |
| | | storageclasses | get, list, watch | read the parameters |
| | | events | list, watch, create, update, patch | report provisioning |
| | | csinodes, nodes | get, list, watch | `WaitForFirstConsumer` and per-node topology |

The node plugin pod sets `automountServiceAccountToken: false` and
mounts a projected token only into csi-provisioner; the `agentfs-csi`
container never holds one. With each node's provisioner holding PV and
claim rights cluster-wide, a compromised node can create or delete PVs
of this driver's claims anywhere: keep the node plugin's namespace to
cluster administrators.

**Pod Security inventory.**

| Namespace | Pod | Level | Why |
|---|---|---|---|
| `agentfs-csi` (enforce `privileged`, warn and audit `baseline`) | node plugin `agentfs-csi` | privileged, root, `hostPID` | FUSE `mount(2)`, the workloads' real pids so fronts create files as their callers with their supplementary groups (design §6), `Bidirectional` propagation into `/var/lib/kubelet`, hostPath volumes, passthrough (`CAP_SYS_ADMIN`), giving files to their owners (`CAP_CHOWN`, `CAP_FOWNER`), removing a released worktree |
| | csi-provisioner, node-driver-registrar, livenessprobe | root, no capabilities, read-only root filesystem | reach the root-owned 0660 CSI socket |
| workloads (e.g. `agentfs-example`, enforce `restricted`) | anything | restricted | they only mount a claim |

**Who can change what.**

- Operators (who can edit the `agentfs-csi` namespace and StorageClasses)
  choose remotes, rules, credentials, identity templates and start
  points.
- Pod authors choose only a StorageClass and their pods' names: they see
  no state area, no credentials and no settings, and cannot read or
  rewrite history.
- Anyone who can create pods with a hostPath, or exec into the node
  plugin, can read every volume's history and credentials on that node:
  keep the `agentfs-csi` namespace to cluster administrators.

**Restricting who uses a class.** A StorageClass is cluster-wide. Limit a
namespace with a ResourceQuota on
`<class>.storageclass.storage.k8s.io/persistentvolumeclaims` (for example
`0` where agentfs volumes are not allowed). A start class in particular:
until it is deleted, any claim on it, in any namespace, starts from its
pinned commit ([moving](runbooks/moving.md)).

## 11. Where worktrees live

Each node keeps its worktrees under its own `--base-path`, on its own
disk (design §19.1, 2026-10-16, R1: "use local disk for working code").
The optional shared storage the owner described is an ordinary git
remote (a server on a claim of any StorageClass, NFS 4.0 or iSCSI
included), never the base path.

- **Any local directory.** The chart's `basePath.path` is a hostPath on
  every node. Prefer a disk of its own: sizes are not enforced, and a
  full disk makes the kubelet evict every pod on the node.
- **No network filesystem.** `agentfs-csi` refuses a base path on NFS
  (any version), SMB, CephFS, GlusterFS, Lustre, GPFS or AFS before it
  serves anything, naming the filesystem type. (A CLI or sidecar state
  area may still live on NFS 4.1 or later, whose locks the state lock
  checks: design §2.3.)
- **Moving is a new volume.** A worktree never leaves its node. To go
  on elsewhere, a start class (§5) makes a new worktree on the new node
  from a pinned commit of the old branch on a remote
  ([moving](runbooks/moving.md)). What a move costs: a new branch that
  readers and iac's patches must be pointed at; the old queue and
  `diverged/*` refs stay with the old branch; files history does not
  record (ignored or oversize ones, ownership, modes beyond the
  executable bit, timestamps, xattrs, empty directories) are not carried
  over; a volume without a remote cannot move.
- **The old node coming back** pushes its own branch as usual (its
  release scan, since no pod stages the old worktree): its late commits
  reach the old branch, never the new one, and the new volume raises
  `StartBranchMoved` for them at its next push to that remote. They are
  saved, and alarmed, never overwritten.

## 12. Limitations

- One node per volume, for good; every multi-node access mode
  (`ReadWriteMany`, `ReadOnlyMany`) is refused (design §19.1,
  2026-10-15). Pods on other nodes read the volume's history from its
  remote. A volume continues elsewhere only as a new volume started from
  its branch (§11).
- Draining a node leaves its agentfs pods `Pending` until its plugin
  returns. Fork pods need affinity to their source's pods, written by
  their author (§14).
- The start-branch alarm waits for the new volume's next push and
  watches only the start remote; a pin older than the branch's tip raises
  it at once.
- No capacity enforcement, snapshots or expansion; clones only into a
  fork class (§14).
- Restarting the node plugin interrupts that node's mounts until it has
  recovered them (seconds); containers without `HostToContainer`
  propagation need a restart to see the recovered mount. Drain the node's
  agentfs pods before an upgrade (§9, "Upgrading the node plugin").
- A restore runs on the node that stages the volume, from its node plugin
  pod (§9).
- No inline ephemeral volumes: a pod author would choose the parameters.
- The plugin's flags (`--stage-wait`, `--publish-wait`, the graces) are
  per node, not per volume.
- Worktree `gitdir:` paths map exactly only through `workloadMounts`,
  which is per StorageClass or PV, not per pod: pods of one volume that
  mount it at different paths need each path listed.
- With `classAdmission.enabled` off, a class with `Retain` loses nothing
  (the release scan and the decider still act), but its PVs stay
  `Released` for ever.

## 13. How it is tested

- **Unit and integration tests** (`cargo test`): parameters (static PV
  attributes and start classes included), layout and records; the
  Identity, Controller and Node services over a real gRPC socket: the
  topology, CreateVolume on a start class against a `file://` remote with
  each refusal's code, DeleteVolume for every release answer against
  prepared `file://` remotes (held, behind, absent, diverged,
  unreachable, no remote declared), the adoption of a retained worktree
  through the documented attributes, and every multi-node access mode
  refused with the decision (`crates/agentfs-csi/tests/grpc.rs`); the
  release scan with a fake launcher: it pushes an unstaged worktree,
  backs off after a failure, skips a held one, finishes a fork that
  asked (`release_scan.rs`); the supervisor with a fake launcher and
  mounter: NodeStage never creating a worktree, staging, publishing,
  restarts with backoff in the same slot, restarts whose spawn fails and
  are tried again, bind stacking, reconcile after a restart (also when a
  service cannot start yet), a pending publish not holding up another
  pod's recovery, a republish with other flags, unpublish and unstage
  ordering, shutdown (`tests/supervisor.rs`); and with the real `agentfs`
  binary, without FUSE: the supervisor driving `serve-history` (two
  pods' commits, a `kill -9` recovered with its journaled commit, the
  final commit at unstage and its final push), and the release scan
  pushing an unstaged worktree until its remote holds it
  (`crates/agentfs/tests/csi_service.rs`).
- **csi-sanity** (`ci/csi-sanity.sh`, the `csi-sanity` job): csi-test
  v5.6.0 against `agentfs-csi` with real history services and FUSE
  fronts, then no mount, record or volume directory left behind. Its
  clone tests are skipped: the driver clones only into a fork class.
- **Moving between nodes on kind** (`ci/e2e-csi-move.sh`, the
  `e2e (CSI move between nodes)` job of the f8 workflow, two workers):
  node-local worktrees, a pod on one worker whose late commit stays on
  its disk while the git server is down, the worker stopped and the
  claim deleted (the PV stays Released), a start class continuing the
  volume on the other worker from the pinned commit, the first worker's
  return pushing its late commit to the old branch by its release scan
  (`PushFailed` while the server is down, DeleteVolume `UNAVAILABLE`,
  then the PV and the worktree gone), the new volume's start-branch
  alarm and `start accept`, a CLI volume reusing the old name diverged
  and saved, a start class on a `diverged/*` ref refused, and a missing
  worktree making a pod wait.
- **Forks on kind** (`ci/e2e-csi-forks.sh`, the `e2e-csi-forks` job in
  the image workflow): a sandbox's ephemeral claim cloning a source claim
  in use, its pushes to the fork remote only, the class admission
  policy, a DeleteVolume that waits across a node-plugin restart until
  the release scan, once the fork remote is reachable again, has
  published the fork; a second fork refused by the remote's hook,
  read-only when mounted again, and published by `fork finish --retry`;
  and the change submitted twice and applied once (§14).
- **kind** (`ci/e2e-csi.sh`, the `e2e-csi` job in the image workflow, with
  the image just built): the RBAC inventory with `kubectl auth can-i`
  and no token in the `agentfs-csi` container, the node's topology label
  and the PV's node affinity, the class admission policy, two restricted
  pods on one claim with distinct committers and pushes to a git server,
  `kill -9` of a front, of the service and deletion of the node plugin
  pod, each recovered, the refusals of inline volumes and
  `ReadWriteMany`, and teardown: the last commit reaches the remote, then
  DeleteVolume removes the volume.
- **NFS** (`ci/e2e-nfs.sh`, the `e2e-nfs` job): `agentfs-csi` refuses a
  base path on an NFS export (4.2 and 4.0), naming the type; the state
  areas of `serve-history` and its fronts on NFS are covered there too.
- **Not tested**: a node whose disk is lost for good (the finalizer
  removal of [moving](runbooks/moving.md)), Talos and other hardened node
  images, node reboots, and the moving runbook as written.

## 14. Forks

A **fork** is a volume started from another volume's latest commit
(design §11.5). A sandbox pod gets one through a claim whose `dataSource`
is the source's claim, on a **fork class**: a StorageClass with
`fork: "true"` and reclaim policy `Delete`
([example](../deploy/csi/example/fork-storageclass.yaml)). The fork has the
source's profile; it pushes `refs/heads/agentfs/<fork>` to its fork remote
(the one remote with `agentfsForkRemote = true` in the profile, else the
source's patch remote), never the source's branch, or with
`agentfs.forkRefs = hidden` in the profile `refs/agentfs/<fork>/history`,
off every branch ([configuration](configuration.md#agentfs)).

- **Create, on the source's node.** CreateVolume clones the source
  locally: its settings and branch head read without its lock (the
  source may be in use), its history up to that head copied, and the
  head's tree written into the fork's backing tree, with no credentials.
  The source must be on the node the fork's pod is scheduled to: give
  the sandbox pod affinity to its source's pods (elsewhere CreateVolume
  answers `FailedPrecondition`, naming the remedy). Kubernetes'
  documentation says a clone's source claim must not be in use, but the
  external-provisioner does not check it, and the kind test
  (`ci/e2e-csi-forks.sh`) clones a claim a running pod writes to (design
  §19.2, question 8).
- **Admission.** The class admission policy
  (`deploy/csi/class-admission.yaml`) refuses any StorageClass of this
  driver, a fork class too, whose reclaim policy is not `Delete`: with
  `Retain`, deleting a claim would never call DeleteVolume, and the fork
  would never finish.
- **Finish.** Deleting the claim (a pod-scoped ephemeral claim goes with
  its pod) calls DeleteVolume, which records `finish_requested` in
  `csi.json` and answers `UNAVAILABLE` until the fork is published, so the
  PV stays `Released` meanwhile. The node plugin's release scan runs at
  start and every `--release-scan-interval` (30 s); for each fork asked to
  finish that no node stages and that is neither refused nor abandoned,
  it runs `agentfs fork finish --timeout 300s`, which commits what is
  journaled, writes the change and finish commits, and pushes the
  history, then the change and finish refs create-only, retrying with
  backoff until the fork remote holds them. The next DeleteVolume deletes
  the volume (`onDelete: delete` by default). Restarts of the node plugin
  carry on: the request is in `csi.json`.
- **Receiving.** Whatever reads the fork remote gates the change: a PR on
  the fork's branch, or `agentfs patch submit <change>` into the source's
  inbox, which applies a given change commit at most once.
- **Read-only once finished.** A finished fork takes no changes. Staged
  again (a static PV of its directory, while DeleteVolume waits), its
  history service starts, its fronts FUSE-mount it read-only and every
  NodePublish is bind-mounted read-only, whatever the pod asked: writes
  fail with `EROFS`.
- **Refused.** A fork remote that refuses for good (another writer's
  finish or change ref, a hook refusing `refs/agentfs/*`, a commit over
  `agentfsMaxPushSize`, or a fork remote no longer in the profile) makes
  the fork `refused`: the alarm `ForkRefused` is raised
  once, the release scan starts no further finisher, and DeleteVolume
  keeps answering `UNAVAILABLE`. Each reason and its fix:
  [the refused-publish runbook](runbooks/fork-refused.md).
- **Stuck.** A fork whose remote stays unreachable, or refuses, keeps its
  PV `Released`; see [the CSI recovery runbook](runbooks/csi-recovery.md).
  To look, and to act, run agentfs in the node plugin pod that holds the
  volume (`<base>` is the plugin's `--base-path`):

  ```sh
  kubectl -n agentfs-csi exec <node-plugin-pod> -c agentfs-csi -- \
    agentfs --state <base>/<pv>/state fork status
  # Fixed the cause of a refusal (removed the other writer's ref, the hook):
  kubectl -n agentfs-csi exec <node-plugin-pod> -c agentfs-csi -- \
    agentfs --wait-lock 6m --state <base>/<pv>/state --root <base>/<pv>/backing \
    fork finish --retry --timeout 2m
  # Or discard it on record, so DeleteVolume applies onDelete:
  kubectl -n agentfs-csi exec <node-plugin-pod> -c agentfs-csi -- \
    agentfs --wait-lock 6m --state <base>/<pv>/state fork abandon --reason "<why>"
  ```

  `--wait-lock` waits for the release scan's finisher, which frees the
  fork's lock at its 5-minute timeout. Abandon is refused for a
  published fork; nothing agentfs runs pushes an abandoned one again,
  though a push already running finishes.
