# Deploying agentfs on Kubernetes

agentfs runs as a **privileged native sidecar** in the workload's
own pod: the sidecar mounts the FUSE filesystem into an
`emptyDir`, and the workload's containers use it like any directory. This
guide covers the container image, the example manifests in
[`deploy/sidecar`](../deploy/sidecar), why the pod is laid out the way it
is, configuration, operations, what happens when the daemon crashes, the
security cost of the privileged sidecar, and Hermes as upstream ships it.
The sidecar runs the tree engine ([trees](trees.md)), as the CSI driver
([csi.md](csi.md)) does.
Section numbers (§) on this page are this page's own. What agentfs
promises is in [guarantees](guarantees.md). Running a mount
outside Kubernetes is in the [operator guide](operator-guide.md); every
setting is in the [configuration reference](configuration.md).

## 1. The image

`ghcr.io/mcurcio/agentfs`, built from the repository's
[`Dockerfile`](../Dockerfile) and published by the `image` workflow:

| Tag | What |
|---|---|
| `<40-character commit sha>` | The image built from that commit of `main` |
| `latest` | The newest `main` |

An image is published only after the kind end-to-end test (§8 below) has
passed with exactly that image; pull requests and other branches build it
without publishing. **Pin a commit tag or a digest** in production
(`images:` in your kustomization); `latest` moves with `main`.

- **Contents:** the `agentfs` binary built with the `https`, `ssh`
  and `otlp-tls` features (so every remote transport, and TLS to the
  collector, works), and the CSI driver `agentfs-csi`
  ([csi.md](csi.md)), on `debian:trixie-slim`
  with `fuse3` (`fusermount3`), `libssl3t64`, `ca-certificates` (the TLS
  trust store), `git`, so the runbooks' `git log` and `git show` work
  through `kubectl exec`, and `tini`, PID 1 of the CSI node plugin. Both
  base images are pinned by digest.
- **The release of `git`:** the build fails unless Debian's `git` is the
  release in `ci/git-release`, the one CI runs. In tree views ([trees](trees.md)) an upgrade
  changes how later saves align repeated lines, so a concurrent edit of a
  repeated line can land elsewhere or collide. Stored bytes and line
  identities do not change.
- **Platform:** `linux/amd64` only.
- **Entrypoint:** `agentfs`; the default command is `--help`.
- **User:** root, on purpose (`USER 0`). The sidecar must be root and
  privileged (§7 below), and libgit2's repository ownership check wants
  every process that touches the state area to be its owner. The init
  container also runs as root, but with every capability dropped. Your
  workload containers do not use this image and should not run as root.

Build it yourself:

```sh
docker build -t agentfs:dev .
docker run --rm agentfs:dev --version
```

The build caches dependencies in their own layer, so a source change
rebuilds only the agentfs crates.

## 2. Quick start

1. Edit [`deploy/sidecar/config/exclude`](../deploy/sidecar/config/exclude)
   and `attributes` next to it: the paths not to record. `config` holds the
   operator settings; the remote it names is logged as not built until push
   is built for tree volumes (§4).
2. Create the namespace (the sidecar is privileged), and the credentials
   Secret out of band for when push is built:

   ```sh
   kubectl create namespace agents
   kubectl label namespace agents pod-security.kubernetes.io/enforce=privileged
   kubectl -n agents create secret generic agentfs-credentials \
     --from-literal=origin=$'username=agentfs\npassword=<token>'
   ```

   and drop `secret.yaml` from the kustomization (or keep it for a first
   try).
3. Replace the `workload` container in
   [`deployment.yaml`](../deploy/sidecar/deployment.yaml) with yours, or
   patch it from an overlay.
4. Apply:

   ```sh
   kubectl -n agents apply -k deploy/sidecar
   kubectl -n agents get pods -l app.kubernetes.io/name=agentfs-example
   kubectl -n agents logs deploy/agentfs-example -c agentfs | grep -E '^(mounted|not built) '
   ```

The workload sees the volume at `/agentfs/workspace`. The sidecar runs the
tree engine ([trees](trees.md)): every change the workload makes there is
recorded before the call returns, and lands in the volume's history,
`/data/state/history.git` (branch `root`), on the claim. Push is not built
on tree volumes yet: the history stays on the claim, and the sidecar logs
each remote of its settings as not built at every boot.

With Helm, the `agentfs-sidecar` library chart renders the same pod
pieces into your workload's own chart ([helm.md](helm.md#4-the-sidecar-helpers)).

## 3. The pod, piece by piece

```
pod
├── init container  agentfs-init   agentfs init --include /etc/agentfs/config
├── native sidecar  agentfs        agentfs mount /agentfs/workspace
└── containers      workload       uses /agentfs/workspace

volumes
├── data           PVC (ReadWriteOncePod)   /data: state/, the volume's state area
├── agentfs-mount  emptyDir                 /agentfs; the FUSE mount is /agentfs/workspace
├── config         ConfigMap                /etc/agentfs, and history.git/info/exclude, info/attributes
└── credentials    Secret                   /run/secrets/agentfs
```

**The PVC is mounted once, at `/data`, in the agentfs containers only.**
The state area, `/data/state`, holds everything of the volume: its files,
its untracked area, its journal and its history (`history.git`). There is
no backing tree: the workload's files are the volume's, reached only
through the mount.

**The mount point is a subdirectory of the `emptyDir`,** `/agentfs/workspace`,
not the `emptyDir`'s root. If the daemon dies, only that subdirectory
answers `ENOTCONN`; the volume directory itself stays a normal directory.
With the mount on the root, the volume directory itself would answer
`ENOTCONN`: the container runtime, which stats a bind-mount source, could
fail to create the restarted sidecar, and the kubelet could fail to remove
the pod's volumes, leaving the pod stuck Terminating. (This is reasoned,
not tested; the subdirectory layout is what the kind test exercises.)
`agentfs mount` creates the subdirectory when it is missing, and refuses
one that holds files.

**Mount propagation.** The sidecar mounts the `emptyDir` with
`Bidirectional` propagation, so the FUSE mount it makes inside it
propagates to the host and from there to the other containers, which
mount the same `emptyDir` with `HostToContainer`. Kubernetes allows
`Bidirectional` only in privileged containers.

**The workload mounts the `emptyDir` read-only** (`readOnly: true`,
`recursiveReadOnly: Disabled`). The FUSE mount propagated into it is a
mount of its own and stays writable; the bare directory underneath is
not. That is the fence for the moments no view is mounted (a sidecar
that stopped and is being restarted, §7): writes fail with `EROFS`
instead of landing where nothing records them. A container that mounts
the workspace itself through `subPath` cannot be fenced this way (a
read-only subPath would make the FUSE mount read-only); for it the
sidecar **seals the mount point** (`AGENTFS_SEAL_MOUNTPOINT=true`): the
bare directory under the mount is root's with mode 0000, so a non-root
container (or root without `CAP_DAC_OVERRIDE`) that binds it while no
view is mounted gets `EACCES`. Give it the liveness probe commented in
the example too (its mount point must be a FUSE mount: `stat -f -c %T`
prints `fuseblk`), so it is restarted onto the new mount after the daemon
has gone.

**Crash recovery.** `AGENTFS_ON_STALE_MOUNT=recover`: when the kubelet
restarts a crashed sidecar, it stacks its view over the stale mount the
dead daemon left, and mount propagation carries the new mount into the
workload containers (§7).

**Startup order.** The sidecar is a native sidecar: an init container
with `restartPolicy: Always`. The kubelet starts it after `agentfs-init`
and starts the workload containers only once its **startup probe**
(`agentfs probe --allow-stale /agentfs/workspace`) passes, that is, once
the mount is live. The probe allows 5 minutes (`periodSeconds: 2`,
`failureThreshold: 150`) for the volume's recovery at start. Its
**readiness probe** (`agentfs probe /agentfs/workspace`) follows the
mount: a dead daemon, or a drained one whose view is unmounted, shows as a
NotReady pod. There is **no liveness probe**: the kubelet already restarts
a daemon that exits or crashes, and killing a slow but live one (a long
recovery) would only interrupt it.

**One writer.** `ReadWriteOncePod` lets only one pod in the cluster use
the volume, and the Deployment's `Recreate` strategy stops the old pod
before the new one starts, so a rollout never has two pods (and two
daemons) on one state area. The state area's lock is the last line of
defence: a second mount exits 75. Most clusters need a CSI driver for
`ReadWriteOncePod`.

**Termination.** Native sidecars are stopped after the workload
containers. At SIGTERM the sidecar drains the volume
([lifecycle](trees.md#volume-lifecycle)): it detaches the view's client
(files still open are saved as they stand), unmounts the view lazily, and
exits 0 once the volume is released and the view has ended. A clean stop
leaves history built through its last record. A workload container that
mounts the view itself through a `subPath` (Hermes, §10) gets a kubelet
bind of the mount that stays until the kubelet tears the pod's volumes
down, after the sidecar has exited. The view cannot end while that bind
holds it, so the sidecar completes its drain and then waits: the kubelet
kills it at `terminationGracePeriodSeconds`. Nothing is lost, since the
drain finished first, but every stop of such a pod takes the whole grace.
Keep the grace long enough for the drain (60 s in the example).

**`agentfs-init`** runs `agentfs init` at every start. On the first start
it creates the volume (`<volume> <id> created`); afterwards it finds it
(`found`). A state area of another volume, or of the recording engine,
exits 1 with nothing written, which stops the pod before anything is
mounted; for the latter it names
[upgrading-the-sidecar.md](runbooks/upgrading-the-sidecar.md). It also
makes the volume's operator settings file include `/etc/agentfs/config`
(§4). With `AGENTFS_OWNER=UID:GID` a new volume's root directory is
recorded as that user's (§5).

## 4. Configuration

**Operator settings** live in the ConfigMap key `config`, a git-config
file mounted at `/etc/agentfs/config`. `agentfs init --include
/etc/agentfs/config` makes it the include of the volume's operator
settings file, `/data/state/config`, which every mount reads
([operator settings](trees.md#operator-settings), [the
keys](configuration.md#tree-volumes-the-operator-settings-file)). It is
kept out of `history.git/config`, which the engine rewrites. An include
under the mount point exits 1: the workload could change it.

**What is not built yet.** Tree volumes do not build push, the patch
queue or LFS yet. A key of one of them (a `[remote]` section,
`agentfs.pushInterval`, `http.proxy`, `agentfs.patch*`, `lfs.*`, ...) does
nothing: the sidecar serves anyway, and logs each such key at every
boot, one line per key:

```text
not built: remote.origin.url (push): this volume pushes to no remote yet; its history stays in the state area
not built: agentfs.patchremote (patches): queued patches are not applied
```

and exports the `agentfs.not_built` gauge, by `capability`
([telemetry](telemetry.md)); the `AgentfsNotBuilt` alert fires on it
(below). `agentfs.nested = exclude` is built and not listed: nested
repositories stay out of history.

**Rules.** The ConfigMap keys `exclude` and `attributes` are mounted
read-only over `/data/state/history.git/info/exclude` and
`info/attributes`, in the sidecar only (`subPath` mounts). They are read
when the volume opens, before anything is written, and decide only for new
names: a name they leave out lives in the volume's untracked area, never
in history ([rules](trees.md#rules-and-the-untracked-area)). A rule tree
volumes do not build (an `agentfs.<name>` attribute, `filter=lfs`)
refuses the volume, naming the line.

**Changing settings or rules** is a rollout: the kustomization's
`configMapGenerator` puts a hash of the content in the ConfigMap's name,
so a change gives the pod template a new ConfigMap and rolls the pod. A
ConfigMap edited in place would not reach the `subPath` mounts.

**Credentials** come from the Secret mounted at `/run/secrets/agentfs`
(mode `0400`), named in the settings (`remote.origin.agentfsCredentials =
/run/secrets/agentfs/origin`). Nothing reads them until push is built;
keeping the Secret in the pod means the remote works without a pod change
then. The keys of each kind of remote are in the
[configuration reference](configuration.md#remote-name).

**The downward API** fills `AGENTFS_POD`, `AGENTFS_POD_UID`,
`AGENTFS_NAMESPACE` and `AGENTFS_NODE`: the pod context of every commit
(`Agentfs-Pod: ...` and the other trailers,
[trees](trees.md#volume-identity-and-trailers)) and the telemetry
resource ([telemetry](telemetry.md)).

**Telemetry.** Set `OTEL_EXPORTER_OTLP_ENDPOINT` to your collector (OTLP
http/protobuf by default; `OTEL_EXPORTER_OTLP_PROTOCOL=grpc` with the
collector's port 4317, or `http/json`; `https://` with `OTEL_EXPORTER_OTLP_CERTIFICATE` and, for
mutual TLS, the client certificate and key, mounted from a Secret), or
`OTEL_SDK_DISABLED=true`. The sidecar's mount exports metrics only:
`agentfs.not_built` and the count of failed exports. See
[telemetry.md](telemetry.md).

## 5. Users and permissions

Any user can use the mount (`allow_other`), and the kernel checks the
ownership and modes the volume records. Files belong to the user who
created them, with the mode they asked for, and history records each
file's and directory's owner and mode ([what a commit
holds](trees.md#what-a-commit-holds)).

- **The root of the volume** is recorded at `init` as owned by the user
  that ran it (root in the example), mode `0755`, or by
  `AGENTFS_OWNER=UID:GID` (`init --owner`; the chart's `owner` value). A
  found volume is never changed. A non-root workload therefore needs either
  that, or a directory of its own made as root through the mount (`mkdir
  /agentfs/workspace/app && chown 10000:10000 /agentfs/workspace/app`,
  from a root container or `kubectl exec`).
- **`fsGroup` is not used.** The kubelet applies it by recursively
  changing the group of the volume, which would also hit the state area,
  and FUSE mounts do not take part in it anyway. Use explicit ownership.
- **Workload containers** should run as non-root with every capability
  dropped and `RuntimeDefault` seccomp, as in the example. They never see
  `/data`, the state area or the credentials.

## 6. Operations

Everything runs in the sidecar with `kubectl exec`; its environment
already names the state area. History is git, read with the `git` the
image ships:

```sh
POD=$(kubectl get pod -l app.kubernetes.io/name=agentfs-example -o name | head -n1)
kubectl exec "$POD" -c agentfs -- git --git-dir /data/state/history.git log --stat root
kubectl exec "$POD" -c agentfs -- agentfs probe /agentfs/workspace
```

A commit keeps a file's bytes at `files/<aa>/<bb>/<path>`, where `aa` and
`bb` are the first hex digits of `git hash-object --stdin` of the path
([what a commit holds](trees.md#what-a-commit-holds)), so `git log --
files/<aa>/<bb>/<path>` is one file's history.

**Not built on tree volumes yet:** `status`, `push`, `restore`, `revert`,
`flush` and the other commands of the recording engine refuse a tree
volume with a message naming what is not built. Undo is reading the old
bytes from history and writing them back through the mount.

- **Upgrading agentfs:** change the image tag; the rollout (Recreate)
  stops the old pod, whose sidecar drains, and the new pod mounts the
  same volume
  ([upgrading-the-sidecar.md](runbooks/upgrading-the-sidecar.md)). A
  volume of the recording engine (a `/data/backing` beside `/data/state`)
  is moved to the tree engine once, by that runbook; the new image's
  `init` refuses it until then.
- **Rolling back** to an image of the recording engine is restoring the
  archive the move kept
  ([the runbook](runbooks/upgrading-the-sidecar.md#rolling-back)).
- **Changing rules or settings:** edit `config/`, apply; the pod rolls
  (§4 above).
- **Recording failing** (`EIO` in the workload, `tree HistoryBuildFailed`
  in the sidecar's log):
  [recording-failures.md](runbooks/recording-failures.md).

### Alerts

[`deploy/alerts`](../deploy/alerts) has example alert rules, each with a
runbook link:

| File | What |
|---|---|
| `agentfs.rules.yml` | The rules, as a plain Prometheus rules file (`rule_files:`). The one file to edit |
| `prometheusrule.yaml` | The same groups as a Prometheus Operator `PrometheusRule`, generated by `ci/alerts-prometheusrule.sh`; add the label your operator's `ruleSelector` wants, then `kubectl apply -f` it |
| `agentfs.rules.test.yml` | promtool unit tests of the rules |

The CSI Helm chart installs the same `PrometheusRule` with
`alerts.enabled=true` (off by default; [helm.md](helm.md#3-values)).

For the sidecar, alert on `AgentfsNotBuilt` and `AgentfsHistoryUnverified`:
the sidecar's mount exports `agentfs.not_built`, `agentfs.history.unverified`
and the count of failed exports, as each CSI volume's `agentfs host` does
([csi.md](csi.md#9-operations)). The other rules are on the recording
engine's metrics, which no launcher emits any more.

| Alert | Severity | Fires when |
|---|---|---|
| `AgentfsNotBuilt` | warning | a tree volume's operator settings name a key whose capability is not built yet (`agentfs.not_built`), for as long as the key stays |
| `AgentfsHistoryUnverified` | warning | a tree volume's history has commits flagged `Agentfs-Unverified` that no later check has verified or acknowledged (`agentfs.history.unverified`, read at each start; [trees](trees.md#when-history-cannot-check-a-save)) |
| `AgentfsTelemetryExportFailing` | warning | a telemetry export failed in the last 15 minutes |
| `AgentfsRecordFailed` | critical | a record failed in the last 10 minutes (`agentfs.record.failures`) |
| `AgentfsCommitsFailing` | critical | commits failed in every 5 minutes for 10 minutes while the journal holds records |
| `AgentfsRepackFailed` | warning | a periodic repack failed in the last 3 hours |
| `AgentfsReconcileFailed` | warning | a moved directory was given up on in the last hour |
| `AgentfsHistoryServiceUnreachable` | critical | a front's request to its history service failed in the last 5 minutes |
| `AgentfsPushBacklogOld`, `AgentfsPushBacklogStale` | warning, critical | a remote's oldest unpushed commit is over 1 hour, 6 hours old |
| `AgentfsPushDiverged` | critical | a remote has diverged |
| `AgentfsRemoteMisconfigured` | warning | a remote section is invalid, so never pushed |
| `AgentfsPatchQueueBlocked` | warning | the patch queue is blocked |
| `AgentfsWorktreeHeuristic` | info | a worktree's gitdir was found by the suffix heuristic in the last hour |

The thresholds and windows are examples. Logged alarms (the sidecar's
`tree HistoryBuildFailed`, `tree HistoryLagging`, ...) need a log-based
alert; a dead daemon is the sidecar's readiness and restart count, from
kube-state-metrics.

**Names and labels.** agentfs exports OpenTelemetry metrics over OTLP.
The rules use the names Prometheus gives them with its default OTLP
translation (`UnderscoreEscapingWithSuffixes`), in both the Prometheus
OTLP receiver and the collector's Prometheus exporters: dots become
underscores, counters end in `_total`, a seconds or bytes unit becomes a
suffix (`agentfs_push_oldest_unpushed_age_seconds`), and `service.name`
and `service.instance.id` (the pod name, or `AGENTFS_REPLICA`) become
`job` and `instance`. Other resource attributes are not labels unless
promoted (Prometheus: `otlp.promote_resource_attributes`). Promote
`agentfs.volume`, which becomes `agentfs_volume`, wherever one
`instance` runs several volumes. **Under the CSI driver it is
required:** every volume's `agentfs host` on a node reports the node
plugin's pod as `instance`, so without it the volumes' series merge, and their
interleaved counters look like resets. `AgentfsCommitsFailing` joins a
volume's failures to its backlog on `job`, `instance` and
`agentfs_volume`. A sidecar runs one volume, so there it only names the
volume. Another translation, such as `NoTranslation`, needs the names in
the rules changed.

**Counters** appear at their first count, so `increase()` alone, which
needs two samples, would miss a process's first failure. Each counter
rule also matches a series above zero that had no sample one window
earlier.

**How it is tested.** The `alerts` workflow
([`.github/workflows/alerts.yml`](../.github/workflows/alerts.yml)) runs
`promtool check rules` and the unit tests, checks that the
`PrometheusRule` is the rules file's output, and runs
[`ci/e2e-alerts.sh`](../ci/e2e-alerts.sh) against a real Prometheus with
the rules loaded (its header says what it causes). The `PrometheusRule`
is not applied to a Prometheus Operator in CI.

## 7. A crashed daemon

Every change the workload saw succeed was journaled first, so a crash
loses nothing that returned. What happens to the dead mount, with no
manual step ([restarting after a crash](trees.md#restarting-after-a-crash)):

1. The workload's file calls under `/agentfs/workspace` fail with
   `ENOTCONN` ("Transport endpoint is not connected"). agentfs never
   uses `auto_unmount`, so nothing can be written to the empty directory
   underneath, where it would not be recorded.
2. The kubelet restarts the sidecar container (`restartPolicy: Always`;
   after repeated crashes with its usual backoff). It runs with
   `AGENTFS_ON_STALE_MOUNT=recover`: it sees that the mount point is a
   stale mount of its own volume (`agentfs probe` reports `stale agentfs
   mount (agentfs:<volume>, daemon gone)`), takes the state area's lock
   (the kernel released it with the dead process), recovers the volume,
   rebinds the view's client (the same writer, with the new pod context),
   mounts FUSE at a private directory under `/data/state/stack/`, and
   **bind-mounts it over the stale mount**. Detaching the dead mount
   instead would expose the bare directory to the workload; stacking over
   it never does.
3. Mount propagation carries the new layer into every copy of the stale
   mount point itself: the workload containers' `HostToContainer` mounts
   and a kubelet `subPath` bind of the mount point (`subPath: workspace`).
   The workload's next calls reach the new daemon; no container restarts.
   A `subPath` that points deeper into the view (`subPath:
   workspace/sub`) is a bind of a directory inside the dead mount, which
   no new layer reaches: it keeps answering `ENOTCONN` until its
   container restarts, which is the liveness probe's job. The sidecar logs `stacked over a stale
   mount at /agentfs/workspace left by a crashed daemon`, and the pod is
   Ready again once its readiness probe passes.
4. When that view ends, the stale layers below it are detached (`tree
   DetachedStale: /agentfs/workspace: N layer(s)`).

`--on-stale-mount hold` is the manual alternative: the restarted sidecar
mounts nothing and takes no lock, the pod stays NotReady with the
workload failing with `ENOTCONN`, and deleting the pod recovers. The
[crashed sidecar runbook](runbooks/crashed-sidecar.md) covers both, and
when to step in. The kind end-to-end test checks recovery after `kill -9`
of the daemon from the node.

**A clean stop mid-pod** (an operator sends the sidecar SIGTERM, or
something unmounts the view) leaves no stale mount: the drain unmounts the
view (§3, termination) and the pod turns NotReady. In the gap the
workload's `/agentfs` is the bare `emptyDir`, which it mounts
**read-only** (`readOnly: true`, with `recursiveReadOnly: Disabled` so
the FUSE mount propagated into it stays writable): writes fail with
`EROFS` ("Read-only file system") instead of landing where nothing
records them. Once the process has exited, the kubelet restarts it into a
fresh mount on the bare mount point.

**A sidecar stopped alone while a `subPath` consumer holds the mount**
completes its drain but does not exit: the consumer's bind still holds
the drained view, whose writes fail with `EROFS`, and the view ends only
when the bind goes. The process exits only when it is killed (the pod's
grace at a pod stop, or `kill -9`). The consumer's bind then fails with
`ENOTCONN`, and its liveness probe restarts it onto the restarted
sidecar's mount. If the consumer restarts before the sidecar has mounted
again, its `subPath` binds the bare directory, which the sidecar sealed: a
non-root container's writes fail with `EACCES` ("Permission denied"). A
root container with `CAP_DAC_OVERRIDE` (the default capabilities) still
writes there: those writes are not recorded, and the restarted sidecar
exits 1 on a mount point that holds files until they are removed. Run
`subPath` consumers as non-root, or with `capabilities: {drop: [ALL]}`,
or use the read-only `emptyDir`, which has no such gap.

**Limits:**

- A hung daemon (alive, not answering) is not detected: there is no
  liveness probe on the sidecar, since killing a slow but live daemon
  would interrupt its recovery. See the runbook.
- Repeated crashes put the sidecar in `CrashLoopBackOff` (10 s, 20 s, ...
  up to 5 minutes between restarts); alert on its restart count.
- A view stacked over a stale mount detaches the layers below only when
  it ends. When a `subPath` consumer makes the pod's stop end in the
  kubelet's kill, the stacked view never ends, and the dead layer is left
  under the `emptyDir` for the kubelet to tear down. Not tested: the kind
  test does not delete a pod after a stacked recovery.

## 8. How it is tested

The `e2e-kind` job of [`.github/workflows/image.yml`](../.github/workflows/image.yml)
runs [`ci/e2e-kind.sh`](../ci/e2e-kind.sh) against a kind cluster with
the image just built, deploying [`ci/k8s`](../ci/k8s) (this example with a
remote on the runner's git server, and two writers: root, and uid 10000
through a `subPath` mount of the view at `/workspace`). It checks writes
as both users with normal permissions; that the history holds them with
their owners and the `Agentfs-Pod`/`-Pod-Uid`/`-Namespace`/`-Node`
trailers; that the operator rules keep an excluded and a `-agentfs` file
out of history; that the state area is invisible; that the remote is
logged as not built at every boot and nothing reaches the server; a
replaced pod finding the volume; the sealed mount point (checked from the
node); the clean stop of §7 (the pod turns NotReady, `EROFS` in the gap,
the drained sidecar waiting for the `subPath` bind until killed, the
subPath writer restarted inside the gap refused with `EACCES` or
recorded, then restarted onto the new mount by its liveness probe); and
the in-place recovery after `kill -9` of the daemon from the node (both
writers carry on without a restart, and their writes are recorded).
`kubectl apply --dry-run=server` validates `deploy/sidecar` itself. It
runs locally on any Linux host with docker, kind, kubectl, nginx and
fcgiwrap (see the script's header).

## 9. Security trade-offs of the privileged sidecar

**What `privileged: true` grants:** every capability, every host device
(including `/dev/fuse`), no seccomp or AppArmor confinement, and
writable `/sys`. A compromised agentfs binary or image could take over
the node. The namespace has to allow it (Pod Security Admission
`privileged`), which also allows any other privileged pod there.

**Why it needs it:**

- `mountPropagation: Bidirectional` is only allowed in privileged
  containers, and it is how a mount made in one container reaches the
  others;
- `mount(2)` and opening `/dev/fuse` need `CAP_SYS_ADMIN` (and the
  device), and so does stacking a view over a stale mount (§7);
- giving untracked files to their creators needs `CAP_CHOWN` and
  `CAP_FOWNER`.

**Mitigations in the example:**

- a single-purpose image pinned by digest, root filesystem read-only;
- no service account token (`automountServiceAccountToken: false`) and
  no network listener;
- the init container runs with every capability dropped and no privilege
  escalation;
- workload containers run as non-root, capabilities dropped,
  `RuntimeDefault` seccomp, and never see `/data`, the state area or the
  credentials (an include under the mount point is refused);
- a dedicated namespace for agentfs workloads, so `privileged` PSA does
  not widen what other teams can run.

**Against the threat model ([guarantees](guarantees.md)):** agentfs protects against an agent's
mistakes, not a malicious agent. The privilege sits in the sidecar, not
in the agent's containers: the agent gets an ordinary directory and no
new privilege. The risk it adds is to the node, if agentfs itself is
compromised, so treat its image like any privileged node component:
pinned, scanned, updated.

**The CSI driver moves the privilege out of workload pods** ([csi.md](csi.md#10-security)): its node plugin, in the driver's own
`agentfs-csi` namespace, is the only privileged container, and workload
namespaces can enforce `restricted`. Settings and credentials move with
it, into that namespace.

## 10. Hermes

Hermes is an example consumer: an agent whose definitions (skills,
persona, profiles) are worth recording, next to state that is not
(secrets, databases, sessions, logs, checkpoints).

### What is recorded

The operator files are in [`deploy/examples/hermes`](../deploy/examples/hermes).
The rule files become `/data/state/history.git/info/exclude` and
`info/attributes`:

- [`exclude`](../deploy/examples/hermes/exclude) is an allowlist: every
  new path at the top of Hermes' home is excluded except `skills/`,
  `SOUL.md`, `config.yaml` and `profiles/`; inside each profile, only
  `SOUL.md`, `config.yaml`, `profile.yaml` and `skills/`.
- [`attributes`](../deploy/examples/hermes/attributes) marks secrets
  (`.env`, `auth.json`), databases (`*.db`, `*.sqlite*` and their
  journals), `sessions/`, `logs/` and `checkpoints/` `-agentfs`
  everywhere: never recorded, and a tracked path they mark leaves history
  when the volume opens. Upstream Hermes v0.22.5 keeps its sessions in
  `state.db`, not `sessions/`; both are covered.
- [`config`](../deploy/examples/hermes/config), the operator settings,
  keeps nested repositories out (`nested = exclude`, built), and names a
  remote and a patch source (`patchRemote`), which the sidecar logs as not
  built at every boot.

What the rules leave out stays in the volume, in its untracked area:
Hermes reads and writes it like any other file. `crates/agentfs/tests/cli/deploy_examples.rs`
checks these files against a representative layout in every CI run,
through the tree engine's own rule set. Check the profile layout against
the Hermes version you run.

### The pod changes

Starting from Hermes' own workload:

- **Add `agentfs-init` and the `agentfs` sidecar** from
  `deploy/sidecar/deployment.yaml`, with the volumes `data`,
  `agentfs-mount`, `config` and `credentials`, the Hermes settings and
  rule files as the ConfigMap, and `AGENTFS_VOLUME=hermes`.
- **The Hermes container** keeps its data path, `/opt/data`, by mounting
  the FUSE mount itself there:

  ```yaml
  volumeMounts:
    - name: agentfs-mount
      mountPath: /opt/data
      subPath: workspace
      mountPropagation: HostToContainer
  ```

  Give it the liveness probe a subPath consumer needs (§3, with
  `/opt/data`) so it is restarted onto the new mount after the daemon has
  gone. Hermes' start-up runs as root, which the sealed mount point does
  not stop (§7). The `subPath` binds make every stop of this pod, and of
  the sidecar alone, wait until the kubelet kills the sidecar (§3, §7).

  The alternative, which the read-only fence covers: mount the `emptyDir`
  read-only at `/agentfs`, as `deploy/sidecar`'s workload does, and set
  `HERMES_HOME=/agentfs/workspace` and
  `HERMES_WRITE_SAFE_ROOT=/agentfs/workspace`. The image sets the second
  to `/opt/data`, and Hermes' file tools refuse writes outside it
  (upstream `agent/file_safety.py`; the test sets it but does not
  exercise those tools). The image's entrypoint still sets
  `HOME=/opt/data`, the image's own directory: what Hermes keeps under
  `$HOME` stays in the container.
- **The docker-in-docker sidecar** that runs Hermes' file and terminal
  tools mounts it the same way, and Hermes' `terminal.docker_volumes`
  setting passes the path into tool containers, so they see the same
  files. Whether a sandbox may edit its agent's configuration is a policy
  choice for whoever runs Hermes. Leaving the setting empty (upstream's
  default) keeps the tool containers' writes to that path out of
  Hermes' files, but not every tool container write off the volume:
  upstream still binds directories of Hermes' home into each tool
  container, read-only (`skills/`, `cache/`, `images/`,
  `attachments/`, `composer-pastes/`), and the tool container's home and
  `/workspace` (`container_persistent`, default true) read-write from
  `$HERMES_HOME/sandboxes/`, all resolved by the docker-in-docker
  daemon, so those writes land in the volume (never recorded under these
  rules).
- **Root init.** Hermes' start-up runs as root; root reads and writes the
  mount like any other directory, and files keep their owners, in Hermes
  and in history.
- **The volume** changes from `ReadWriteOnce` to `ReadWriteOncePod`
  (§3 above), with the Deployment's `Recreate` strategy.

### Migrating the existing data

Hermes' current volume holds its files at the root. A tree volume keeps
them in its state area, so they are copied in through the mount, once,
by a Job with Hermes scaled to zero and the same PVC mounted at `/data`:

<!-- ci: hermes-migration -->
```sh
set -eu
cd /data
mkdir import
# The volume root keeps the claim root's owner and mode: cp -a below copies
# import's onto it.
chown --reference=/data import
chmod --reference=/data import
# Move everything except the new directory into it.
find . -mindepth 1 -maxdepth 1 ! -name import -exec mv -t import {} +
agentfs --state /data/state init --volume hermes --include /etc/agentfs/config
mkdir -p /data/state/history.git/info
cp /etc/agentfs/exclude /data/state/history.git/info/exclude
cp /etc/agentfs/attributes /data/state/history.git/info/attributes
mkdir -p /mnt/hermes
# The import, through the mount. Each nested repository's .git first, so
# the files beside it are made inside a repository and stay untracked.
agentfs --state /data/state mount /mnt/hermes -- sh -ec '
  cd /data/import
  find . -name .git -prune -print0 | xargs -0 -r cp -a --parents -t /mnt/hermes
  cp -a . /mnt/hermes/'
```

Run it with the agentfs image, privileged (FUSE needs `/dev/fuse` and
`CAP_SYS_ADMIN`), as root, with the Hermes ConfigMap at `/etc/agentfs`.
The rule files go in before the mount: the volume reads them when it
opens. `mount -- COMMAND` runs the copy once the view is up and drains
when it exits ([running a command](trees.md#running-a-command-in-the-mount)),
so the Job ends with the import recorded and its history built. `cp -a`
keeps each file's owner and mode, and history records them; the volume
root takes `/data/import`'s, which the block sets from `/data`.

The block is not resumable: give the Job `backoffLimit: 0`, so a failure
stops it instead of a retry stopping at `mkdir import`. After a failure,
`/data/import` holds the original files. Move a partial `/data/state`
aside (`mv /data/state /data/state.failed`; never into `/data/import`),
fix the cause from the Job's log, and run the block again from its
`agentfs ... init` line. Files the
rules leave out (`.env`, the databases, sessions, logs, nested
repositories) are copied into the volume's untracked area, never into
history.

Then check the history, and remove the import:

```sh
git --git-dir /data/state/history.git log --stat root
rm -rf /data/import
```

and deploy the new pod spec; from then on every change is recorded
through the mount.

### Restarts and patches

The patch queue is not built on tree volumes yet: iac's patches to
Hermes' files are not applied. The Hermes settings name the patch source
(`agentfs.patchRemote`), so every boot of the sidecar logs `not built:
agentfs.patchremote (patches): queued patches are not applied` and the
`AgentfsNotBuilt` alert fires, instead of the queue being silently
ignored.

A restart of the sidecar alone under running Hermes (a crash, or a
SIGTERM to it) is §7's: after a crash the restarted sidecar stacks over
the stale mount and Hermes carries on; after a SIGTERM the sidecar drains
and waits, held by the `subPath` binds, until it is killed, and Hermes'
liveness probe then restarts it onto the new mount.

### How it is tested

Upstream Hermes is tested in four configurations, with the checks of
[`ci/hermes/lib.sh`](../ci/hermes/lib.sh). The `docker` workflow runs
this setup under Docker Compose: [`ci/e2e-hermes.sh`](../ci/e2e-hermes.sh) with
[`ci/hermes/compose.yaml`](../ci/hermes/compose.yaml) over the compose
example ([docker.md](docker.md)), with this section's settings and rule
files. Hermes is the upstream image, pinned by digest and started as
shipped (`gateway run`, root start-up), with the mount at `/opt/data`;
its tool containers run on a docker-in-docker service that has the mount
too, and `terminal.docker_volumes` passes it into them. A scripted
stand-in for the model API makes every turn deterministic. The job checks
that:

- Hermes' root start-up runs on the mount and files keep their owners, in
  Hermes and in history;
- a skill Hermes creates with its own skill tool is recorded as its
  user's;
- a tool container's edit, made through Hermes' terminal tool, reaches
  the Hermes container and is recorded as root's;
- `.env`, `state.db` (which holds the sessions), `response_store.db`,
  `logs/`, and an `auth.json` and a log a tool container writes inside
  `skills/`, are in the tree but never in history;
- the remote and the patch source are logged as not built at each boot,
  across a restart of agentfs with Hermes and docker-in-docker stopped,
  after which Hermes reads the files it had;
- with `terminal.docker_volumes` emptied, a tool container's write to
  `/opt/data` stays in the tool container, reaching neither Hermes nor
  history; Hermes' `skills/` is read-only there; a file in the tool
  container's home lands in Hermes' `sandboxes/` and is never recorded;
  Hermes' own edits are still recorded.

The `hermes` workflow runs [`ci/e2e-hermes-kind.sh`](../ci/e2e-hermes-kind.sh)
on kind, one job per configuration, with the same checks and the
manifests in [`ci/hermes/k8s`](../ci/hermes/k8s):

- **sidecar**: this pod, from `deploy/sidecar` with the Hermes settings
  and rule files, `subPath` mounts and their liveness probes,
  docker-in-docker as a native sidecar. First upstream Hermes runs on the
  plain volume, then the migration Job above runs as written: the import
  records Hermes' files with their owners, and `.env`, the databases, the
  logs and a nested repository are in the volume but not in history. Then
  a SIGTERM to the sidecar alone: the pod turns NotReady, the sidecar
  waits for the `subPath` binds until it is killed, its restart logs the
  not built lines again, and Hermes' liveness probe restarts it onto the
  new mount, where it records again. The import also records what Hermes
  keeps in `skills/` beside the skills: the bundled skills it syncs there
  and its own `.usage.json`, `.curator_ledger.jsonl` and
  `.bundled_manifest`.
- **fence**: the read-only fence alternative, on a fresh volume, and a
  pod restart that logs the not built lines again.
- **csi**: Hermes and docker-in-docker in one pod on a claim of the CSI
  driver ([csi.md](csi.md)), with a `hermes` profile holding these
  rules: the volume's `agentfs host` in the node plugin, the import
  recorded, the remote and the patch source logged as not built, then a
  pod restart onto a new host where Hermes reads what it had and records
  a new skill. The claim is `ReadWriteOncePod` with `strategy:
  Recreate`: one pod per claim.

Not tested: a profile's files, a real model, tool containers given only
part of the volume, docker-in-docker without the mount, the fence pod
across a restart of the sidecar alone, and Hermes' file tools under the
fence's `HERMES_WRITE_SAFE_ROOT`.

## 11. Limitations

- `linux/amd64` images only.
- Push, the patch queue and LFS are not built on tree volumes yet: the
  history stays on the claim, and their settings are logged and alarmed
  (§4). `status`, `restore`, `revert` and `flush` refuse a tree volume
  (§6).
- Recording's cost per save is in [performance.md](performance.md); keep
  bulk trees such as `node_modules/` off the mount, or out of history
  with the rules.
- A hung daemon is not detected (§7).
- A `subPath` consumer makes every stop of the pod, and of the sidecar
  alone, end in the kubelet's kill; the drain finishes first (§3, §7).
- A `subPath` consumer that runs as root with `CAP_DAC_OVERRIDE`,
  restarted while the sidecar is being restarted after a clean stop, can
  write to the bare directory, unrecorded; the sealed mount point stops
  every other one, and the read-only `emptyDir` has no such gap (§7).
- The privileged sidecar (§9); the CSI driver ([csi.md](csi.md)) needs no
  privilege in workload pods.
- One pod per volume, as under the CSI driver, where a fork claim gives a
  second pod its own fork of the volume ([csi.md](csi.md#14-forks)).
- The image is not bit-reproducible (Debian packages are not pinned),
  and is not yet signed or shipped with an SBOM.

## 12. The sandbox overlay

The sandbox overlay, a second mount point showing sandboxes only the
files the rules would record, is not built on the tree engine. The
sidecar's manifests and chart do not offer it, and `agentfs mount` has no
flag for it. A container that starts sandboxes is given the mount itself,
as Hermes' docker-in-docker sidecar is (§10).
