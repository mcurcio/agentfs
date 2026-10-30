# Deploying agentfs on Kubernetes

Phase 1 runs agentfs as a **privileged native sidecar** in the workload's
own pod (design §17.1): the sidecar mounts the FUSE filesystem into an
`emptyDir`, and the workload's containers use it like any directory. This
guide covers the container image, the example manifests in
[`deploy/sidecar`](../deploy/sidecar), why the pod is laid out the way it
is, configuration, operations, what happens when the daemon crashes, the
security cost of the privileged sidecar, and Hermes, the first consumer
(design §16).

Section numbers (§) refer to [design.md](design.md). Running a mount
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
- **Platform:** `linux/amd64` only for now.
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

1. Edit [`deploy/sidecar/config/config`](../deploy/sidecar/config/config):
   your remote's URL (and `pushInterval`). Edit `exclude` and
   `attributes` next to it for the paths not to record.
2. Create the credentials Secret out of band (a token with push access to
   the history repository):

   ```sh
   kubectl create namespace agents
   kubectl label namespace agents pod-security.kubernetes.io/enforce=privileged
   kubectl -n agents create secret generic agentfs-credentials \
     --from-literal=origin=$'username=agentfs\npassword=<token>'
   ```

   and drop `secret.yaml` from the kustomization (or keep it for a first
   try and edit the password).
3. Replace the `workload` container in
   [`deployment.yaml`](../deploy/sidecar/deployment.yaml) with yours, or
   patch it from an overlay.
4. Apply:

   ```sh
   kubectl -n agents apply -k deploy/sidecar
   kubectl -n agents get pods -l app.kubernetes.io/name=agentfs-example
   kubectl -n agents logs deploy/agentfs-example -c agentfs | grep '^mounted '
   ```

The workload sees the volume at `/agentfs/workspace`. Every change it
makes there is recorded before the call returns, committed to the branch
`agentfs/<volume>` and pushed every `pushInterval`.

With Helm, the `agentfs-sidecar` library chart renders the same pod
pieces into your workload's own chart ([helm.md](helm.md#4-the-sidecar-helpers)).

## 3. The pod, piece by piece

```
pod
├── init container  agentfs-init   agentfs init --include /etc/agentfs/config
├── native sidecar  agentfs        agentfs mount /data/backing /agentfs/workspace
└── containers      workload       uses /agentfs/workspace

volumes
├── data           PVC (ReadWriteOncePod)   /data: backing/ and state/
├── agentfs-mount  emptyDir                 /agentfs; the FUSE mount is /agentfs/workspace
├── config         ConfigMap                /etc/agentfs, and info/exclude, info/attributes
└── credentials    Secret                   /run/secrets/agentfs
```

**The PVC is mounted once, at `/data`.** The backing tree
(`/data/backing`) and the state area (`/data/state`) are siblings on one
filesystem, never two `subPath` mounts of the volume: recording freezes a
file's content with a `FICLONE` reflink into `state/pending`, and reflinks
fail with `EXDEV` across mounts (§3.2). Siblings also keep the state area
out of the backing tree, which `mount` checks. XFS (or btrfs) is
recommended for the volume: a reflink costs the same whatever the file's
size; on ext4 it is a copy.

**The mount point is a subdirectory of the `emptyDir`,** `/agentfs/workspace`,
not the `emptyDir`'s root. If the daemon dies, only that subdirectory
answers `ENOTCONN`; the volume directory itself stays a normal directory.
With the mount on the root, the volume directory itself would answer
`ENOTCONN`: the container runtime, which stats a bind-mount source, could
fail to create the restarted sidecar, and the kubelet could fail to remove
the pod's volumes, leaving the pod stuck Terminating. (This is reasoned,
not tested; the subdirectory layout is what the kind test exercises.) `agentfs mount` creates the subdirectory when it is missing.

**Mount propagation.** The sidecar mounts the `emptyDir` with
`Bidirectional` propagation, so the FUSE mount it makes inside it
propagates to the host and from there to the other containers, which
mount the same `emptyDir` with `HostToContainer`. Kubernetes allows
`Bidirectional` only in privileged containers.

**The workload mounts the `emptyDir` read-only** (`readOnly: true`,
`recursiveReadOnly: Disabled`). The FUSE mount propagated into it is a
mount of its own and stays writable; the bare directory underneath is
not. That is the fence for the moments no daemon is mounted (a sidecar
that exited and is being restarted, §7): writes fail with `EROFS`
instead of landing where nothing records them. A container that mounts
the workspace itself through `subPath` cannot be fenced this way (a
read-only subPath would make the FUSE mount read-only); for it the
sidecar **seals the mount point** (`AGENTFS_SEAL_MOUNTPOINT=true`): the
bare directory under the mount is root's with mode 0000, so a non-root
container (or root without `CAP_DAC_OVERRIDE`) that binds it while no
daemon is mounted gets `EACCES`. Give it the liveness probe commented in
the example too (its mount point must be a FUSE mount: `stat -f -c %T`
prints `fuseblk`), so it is restarted onto the new mount after a clean
exit of the daemon.

**Crash recovery.** `AGENTFS_ON_STALE_MOUNT=recover`: when the kubelet
restarts a crashed sidecar, it mounts over the stale mount the dead
daemon left, and mount propagation carries the new mount into the
workload containers (§7).

**Startup order.** The sidecar is a native sidecar: an init container
with `restartPolicy: Always`. The kubelet starts it after `agentfs-init`
and starts the workload containers only once its **startup probe**
(`agentfs probe --allow-stale /agentfs/workspace`) passes, that is, once
the mount is live. The probe allows 5 minutes (`periodSeconds: 2`,
`failureThreshold: 150`) for journal recovery at start. Its **readiness
probe** (`agentfs probe /agentfs/workspace`) makes a dead daemon visible
as a NotReady pod. There is **no liveness probe**: the kubelet already restarts a
daemon that exits or crashes, and killing a slow but live one (a long
journal recovery) would only interrupt it.

**One writer.** `ReadWriteOncePod` lets only one pod in the cluster use
the volume, and the Deployment's `Recreate` strategy stops the old pod
before the new one starts, so a rollout never has two pods (and two
daemons) on one state area (§2.3). The state-area lock is the last line
of defence. Most clusters need a CSI driver for `ReadWriteOncePod`.

**Termination.** Native sidecars are stopped after the workload
containers, so the daemon unmounts once nothing uses the mount, runs a
final sweep, makes the final commit and pushes it once more, within
`--push-grace` (5 s; the log shows `final commit:` and a `final push:`
line per remote). A workload container that mounts the FUSE root itself through
a `subPath` (Hermes, §10 below) gets a kubelet bind of the mount that
stays until the kubelet tears the pod's volumes down, after the sidecar
has exited. The daemon's unmount succeeds, but the bind keeps the
filesystem mounted elsewhere, so the daemon waits its `--unmount-grace`
(10 s) for the session to end, says so (`the filesystem was still
mounted elsewhere`), and exits; the bind then fails with `ENOTCONN` until
the kubelet removes it. The stop keeps the attached mark, which the next
pod's start passes over because it was another pod's (design §12). `terminationGracePeriodSeconds: 60`
covers both with room to spare; keep it above `--unmount-grace` plus
`--push-grace` plus a few seconds.

**`agentfs-init`** runs `agentfs init` at every start. On the first start
it creates the state area and `/data/backing`; afterwards it checks that
the state area belongs to the configured volume (`AGENTFS_VOLUME`) and
exits 0, or exits 1 if the PVC holds another volume, which stops the pod
before anything is mounted. It also sets the config include (§4 below).

## 4. Configuration

**Settings and remotes** live in the ConfigMap key `config`, a git-config
fragment mounted at `/etc/agentfs/config`. `agentfs init --include
/etc/agentfs/config` names it in the state area's config as
`include.path`, so the sidecar, `kubectl exec` commands and the `git`
CLI all read it. agentfs checks includes more strictly than git
([configuration reference](configuration.md#where-settings-live)): the
path must be absolute and exist, it must not set `agentfs.volume` or
`agentfs.branch`, and it must be outside the backing tree and the mount
point.

**Egress proxies** are set in the same fragment with git's keys,
`http.proxy` (every remote) or `remote.<name>.proxy` (one remote; empty
for none); the pod's `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY` are never
read ([Proxies](configuration.md#proxies)):

```ini
[http]
    proxy = http://egress-proxy.infra.svc:3128
```

**Rules.** The ConfigMap keys `exclude` and `attributes` are mounted
read-only over `/data/state/git/info/exclude` and `info/attributes`, in
the sidecar only (`subPath` mounts). Rules are read at mount time (§4.1).

**Changing settings or rules** is a rollout: the kustomization's
`configMapGenerator` puts a hash of the content in the ConfigMap's name,
so a change gives the pod template a new ConfigMap and rolls the pod. A
ConfigMap edited in place would not reach the `subPath` mounts.

**Credentials** come from the Secret mounted at `/run/secrets/agentfs`
(mode `0400`): `remote.origin.agentfsCredentials =
/run/secrets/agentfs/origin`. The file is re-read whenever it changes, so
rotating the Secret needs no restart. For an SSH remote, keep the private
key, its passphrase file if it is encrypted, and a known_hosts file in
the Secret and name them with `agentfsSshKey`, `agentfsSshKeyPassphrase`
and `agentfsKnownHosts` (or name an ssh-agent socket with
`agentfsSshAgent` instead of a key)
([SSH remotes](configuration.md#ssh-remotes)); the commented example in
`config/config` shows the keys.
With Git LFS on (`agentfs.lfsThreshold`, design §7), an LFS server on
another host than the git remote takes its own credentials file from the
same Secret (`remote.origin.agentfsLfsCredentials =
/run/secrets/agentfs/origin-lfs`); the LFS endpoint derived from an
`https` remote reuses `agentfsCredentials`. The commented LFS lines in
`config/config` show the keys, and the image is built with the `https`
feature, which https LFS servers need. LFS objects are cached under
`/data/state/lfs` and never pruned: size the PVC for them
([operator guide](operator-guide.md#13-large-files-lfs)).

**The downward API** fills `AGENTFS_POD`, `AGENTFS_POD_UID`,
`AGENTFS_NAMESPACE` and `AGENTFS_NODE`: the commit trailers
(`Agentfs-Pod: ...`), the committer identity and the telemetry resource
(§9, §13).

**Telemetry.** Set `OTEL_EXPORTER_OTLP_ENDPOINT` to your collector (OTLP
http/protobuf by default; `OTEL_EXPORTER_OTLP_PROTOCOL=grpc` with the
collector's port 4317, or `http/json`; `https://` with `OTEL_EXPORTER_OTLP_CERTIFICATE` and, for
mutual TLS, the client certificate and key, mounted from a Secret), or `OTEL_SDK_DISABLED=true`. The example
samples 1% of traces (`parentbased_traceidratio`, `0.01`); metrics and
alarm logs are not sampled. See [telemetry.md](telemetry.md).

## 5. Users and permissions

Any user can use the mount (`allow_other`), and the kernel checks real
ownership and modes from the backing tree (`default_permissions`). Files
belong to the user who created them, with the mode they asked for.

- **The root of the tree** (`/data/backing`) is created by `agentfs-init`
  as root, mode `0755`. A non-root workload therefore needs a directory
  of its own: create it once as root through the mount (`mkdir
  /agentfs/workspace/app && chown 10000:10000 /agentfs/workspace/app`,
  from a root container or `kubectl exec`), or have an init step chown
  the backing tree's root. Directories are not recorded, but their files
  are.
- **`fsGroup` is not used.** The kubelet applies it by recursively
  changing the group of the volume, which would also hit the state area,
  and FUSE mounts do not take part in it anyway. Use explicit ownership.
- **Workload containers** should run as non-root with every capability
  dropped and `RuntimeDefault` seccomp, as in the example. They never see
  `/data`, the state area or the credentials.

## 6. Operations

Everything runs in the sidecar with `kubectl exec`; its environment
already names the state area:

```sh
POD=$(kubectl get pod -l app.kubernetes.io/name=agentfs-example -o name | head -n1)
kubectl exec "$POD" -c agentfs -- agentfs status
kubectl exec "$POD" -c agentfs -- git --git-dir /data/state/git log --oneline agentfs/example -- notes/plan.md
kubectl exec "$POD" -c agentfs -- agentfs restore notes/plan.md <commit>
kubectl exec "$POD" -c agentfs -- agentfs revert <commit>
```

`restore` and `revert` go to the running daemon over its control socket
and write through the mount, so the workload sees the result at once
(§15, [recovery runbook](runbooks/recovery-and-undo.md)).

- **Upgrading agentfs:** change the image tag; the rollout (Recreate)
  stops the old pod, whose sidecar makes its final commit, and the new
  pod mounts the same volume
  ([upgrading-the-sidecar.md](runbooks/upgrading-the-sidecar.md)).
- **Downgrading agentfs:** a newer version can journal changes an older
  one cannot read (a directory move): the older one then refuses to open
  the state area with an error naming `agentfs flush`. Stop the pod
  normally (its final commit empties the journal), or run `agentfs flush`
  with the newer image against the state area, before rolling back
  ([the runbook](runbooks/upgrading-the-sidecar.md#rolling-back)).
- **Changing rules or remotes:** edit `config/`, apply; the pod rolls
  (§4 above).
- **Pushes failing:** [stuck-pushes.md](runbooks/stuck-pushes.md).
  `agentfs push` (run in the sidecar) asks the running mount to push
  now; otherwise the mount's own push loop retries.
- **Recording failing** (`EIO` in the workload, `RecordFailed` or
  `CommitFailed`): [recording-failures.md](runbooks/recording-failures.md).

### Alerts

[`deploy/alerts`](../deploy/alerts) has example alert rules for the
metrics of design §13, each with a runbook link:

| File | What |
|---|---|
| `agentfs.rules.yml` | The rules, as a plain Prometheus rules file (`rule_files:`). The one file to edit |
| `prometheusrule.yaml` | The same groups as a Prometheus Operator `PrometheusRule`, generated by `ci/alerts-prometheusrule.sh`; add the label your operator's `ruleSelector` wants, then `kubectl apply -f` it |
| `agentfs.rules.test.yml` | promtool unit tests of the rules |

The CSI Helm chart installs the same `PrometheusRule` with
`alerts.enabled=true` (off by default; [helm.md](helm.md#3-values)).

| Alert | Severity | Fires when |
|---|---|---|
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
| `AgentfsTelemetryExportFailing` | warning | a telemetry export failed in the last 15 minutes |

The thresholds and windows are examples. Alarms that are logs only
(`PatchFetchFailed`, `ForkRefused`, `IdentityFallback`, ...) need a
log-based alert; a dead daemon is the sidecar's readiness and restart
count, from kube-state-metrics.

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
required:** every history service on a node reports the node plugin's
pod as `instance`, so without it the volumes' series merge, and their
interleaved counters look like resets. `AgentfsCommitsFailing` joins a
volume's failures to its backlog on `job`, `instance` and
`agentfs_volume`. A sidecar runs one volume, so there it only names the
volume. An `agentfs` command run by `kubectl exec` in the sidecar
exports under the daemon's `instance` and volume too, so for its run a
metric both export is one series. Another translation, such as
`NoTranslation`, needs the names in the rules changed.

**Counters** appear at their first count, so `increase()` alone, which
needs two samples, would miss a process's first failure. Each counter
rule also matches a series above zero that had no sample one window
earlier.

**How it is tested.** The `alerts` workflow
([`.github/workflows/alerts.yml`](../.github/workflows/alerts.yml)) runs
`promtool check rules` and the unit tests, checks that the
`PrometheusRule` is the rules file's output, and runs
[`ci/e2e-alerts.sh`](../ci/e2e-alerts.sh): the binary exports into a real
Prometheus with the rules loaded, every metric the rules name must exist
there by that name (except the repack, reconcile and front failure
counters, which `ci/e2e-telemetry.sh` causes), the promoted
`agentfs.volume` must be a label, and a failed record, a failed
telemetry export, a diverged remote and a misconfigured one must fire
their alerts. The `PrometheusRule` is not applied to a Prometheus
Operator in CI.

## 7. A crashed daemon

Every change the workload saw succeed was journaled first, so a crash
loses nothing that returned. What happens to the dead mount, with no
manual step:

1. The workload's file calls under `/agentfs/workspace` fail with
   `ENOTCONN` ("Transport endpoint is not connected"). agentfs never
   uses `auto_unmount`, so nothing can be written to the empty directory
   underneath, where it would not be recorded.
2. The kubelet restarts the sidecar container (`restartPolicy: Always`;
   after repeated crashes with its usual backoff). It runs with
   `AGENTFS_ON_STALE_MOUNT=recover`: it sees that the mount point is an
   agentfs mount whose daemon died, takes the state-area lock (the kernel
   released it with the dead process), recovers the journal (`N
   uncommitted record(s) recovered`), mounts FUSE at a private directory
   of the state area (`meta/stack/mnt`) and **bind-mounts it over the
   stale mount**. Detaching the dead mount instead would expose the bare
   directory to the workload; stacking over it never does.
3. Mount propagation carries the new layer into every copy of the stale
   mount: the workload containers' `HostToContainer` mounts and the
   kubelet's `subPath` binds. The workload's next calls reach the new
   daemon; no container restarts. The sidecar logs `stacked over a stale
   mount left by a crashed daemon at /agentfs/workspace (N stale layer(s)
   below)` and the pod is Ready again once its readiness probe passes.
4. When that sidecar stops, it unmounts its own layer and detaches the
   stale layers beneath it (`detached N stale layer(s)`), so pod deletion
   leaves nothing mounted.

`--on-stale-mount hold` is the manual alternative: the restarted sidecar
mounts nothing, the pod stays NotReady with the workload failing with
`ENOTCONN`, and deleting the pod recovers. The
[crashed sidecar runbook](runbooks/crashed-sidecar.md) covers both, and
when to step in. The kind end-to-end test checks recovery after `kill -9`
of the daemon from the node.

**A clean exit mid-pod** (an operator sends the sidecar SIGTERM,
something unmounts the mount, a `--passthrough on` failure) leaves no
stale mount: the daemon unmounts, makes its final commit and push, and
the kubelet restarts it into a fresh mount. In the gap the workload's
`/agentfs` is the bare `emptyDir`, which it mounts **read-only**
(`readOnly: true`, with `recursiveReadOnly: Disabled` so the FUSE mount
propagated into it stays writable): writes fail with `EROFS` ("Read-only
file system") instead of landing where nothing records them. A workload
that mounts the workspace itself through `subPath` cannot be fenced that
way (a read-only subPath would make the FUSE mount read-only too); it
keeps the dead mount, failing with `ENOTCONN`, until it restarts. Give
it the liveness probe commented in the example (the mount point must be
a live FUSE mount), so the kubelet restarts it onto the new mount. If it
restarts before the sidecar has mounted again, its `subPath` binds the
bare directory, which the sidecar sealed: a non-root container's writes
fail with `EACCES` ("Permission denied"). A root container with
`CAP_DAC_OVERRIDE` (the default capabilities) still writes there: those
writes are not recorded and are hidden once the probe restarts it onto
the new mount (the next mount warns that the mount point is not empty).
Run `subPath` consumers as non-root, or with `capabilities: {drop:
[ALL]}`, or use the read-only `emptyDir`, which has no such gap.

**Limits:**

- Files opened for writing before a crash with kernel passthrough
  (`--passthrough auto` as root; only a writer sharing a reader's
  backing file, design §17.1) keep writing to their backing file after the daemon
  died, unrecorded, for as long as the workload holds them open; a
  recovered sidecar does not notice those changes until the file is
  changed again through the mount or the sweep sees it. With in-place
  recovery the workload keeps running, so such a descriptor can live
  longer than under a pod restart. Use `--passthrough off` where every
  write must be recorded.
- A hung daemon (alive, not answering) is not detected: there is no
  liveness probe on the sidecar, since killing a slow but live daemon
  would interrupt its recovery or commits. See the runbook.
- Repeated crashes put the sidecar in `CrashLoopBackOff` (10 s, 20 s, ...
  up to 5 minutes between restarts); alert on its restart count.

## 8. How it is tested

The `e2e-kind` job of [`.github/workflows/image.yml`](../.github/workflows/image.yml)
runs [`ci/e2e-kind.sh`](../ci/e2e-kind.sh) against a kind cluster with
the image just built, deploying [`ci/k8s`](../ci/k8s) (this example with a
remote on the runner's git server, fast pushes, and two writers: root,
and uid 10000 through a `subPath` mount of the FUSE root at
`/workspace`). It checks writes as both users with normal permissions,
the operator rules, that the state area is invisible, authors and
`Agentfs-Pod`/`-Namespace`/`-Node` trailers in the commits pushed over
HTTP with the credential Secret, a restore through `kubectl exec`, the
final commit and push when the pod is deleted, the in-place recovery of
§7 after `kill -9` of the daemon from the node (both writers carry on
without a restart, and `agentfs push` in the sidecar pushes at once), and
the sealed mount point (checked from the node), and the clean-exit fence
(`EROFS` in the gap; the subPath writer, restarted inside the gap, refused
with `EACCES`, then restarted onto the new mount by its liveness probe),
and the rollback's flush Pod of
[upgrading-the-sidecar.md](runbooks/upgrading-the-sidecar.md#rolling-back)
run as written against the claim. `kubectl apply --dry-run=server`
validates `deploy/sidecar` itself. It runs locally on any Linux host with
docker, kind, kubectl, nginx and fcgiwrap (see the script's header).

## 9. Security trade-offs of the privileged sidecar

**What `privileged: true` grants:** every capability, every host device
(including `/dev/fuse`), no seccomp or AppArmor confinement, and
writable `/sys`. A compromised agentfs binary or image could take over
the node. The namespace has to allow it (Pod Security Admission
`privileged`), which also allows any other privileged pod there.

**Why phase 1 needs it:**

- `mountPropagation: Bidirectional` is only allowed in privileged
  containers, and it is how a mount made in one container reaches the
  others;
- `mount(2)` and opening `/dev/fuse` need `CAP_SYS_ADMIN` (and the
  device);
- FUSE passthrough needs `CAP_SYS_ADMIN` in the initial user namespace;
- creating new files as their creators needs `CAP_SETUID` and `CAP_SETGID`,
  and giving them to their creators `CAP_CHOWN` and `CAP_FOWNER`.

**Mitigations in the example:**

- a single-purpose image pinned by digest, root filesystem read-only;
- no service account token (`automountServiceAccountToken: false`) and
  no network listener (the control socket is a Unix socket in the state
  area, mode `0600`, root or the daemon's user only);
- the init container runs with every capability dropped and no privilege
  escalation;
- workload containers run as non-root, capabilities dropped,
  `RuntimeDefault` seccomp, and never see `/data`, the state area or the
  credentials (`mount` refuses layouts that would expose them);
- a dedicated namespace for agentfs workloads, so `privileged` PSA does
  not widen what other teams can run.

**Against the threat model (§1.1):** agentfs protects against an agent's
mistakes, not a malicious agent. The privilege sits in the sidecar, not
in the agent's containers: the agent gets an ordinary directory and no
new privilege. The risk it adds is to the node, if agentfs itself is
compromised, so treat its image like any privileged node component:
pinned, scanned, updated.

**The CSI driver moves the privilege out of workload pods** (design
§14.2, [csi.md](csi.md#10-security)): its node plugin, in the driver's own
`agentfs-csi` namespace, is the only privileged container, and workload
namespaces can enforce `restricted`. Settings and credentials move with
it, into that namespace.

## 10. Hermes

Hermes is the first consumer (§16): an agent whose definitions (skills,
persona, profiles) are worth recording, next to state that is not
(secrets, databases, sessions, logs, checkpoints).

### What is recorded

The rule files are in [`deploy/examples/hermes`](../deploy/examples/hermes):

- [`exclude`](../deploy/examples/hermes/exclude) is an allowlist: every
  new path at the top of Hermes' home is excluded except `skills/`,
  `SOUL.md`, `config.yaml` and `profiles/`; inside each profile, only
  `SOUL.md`, `config.yaml`, `profile.yaml` and `skills/`.
- [`attributes`](../deploy/examples/hermes/attributes) marks secrets
  (`.env`, `auth.json`), databases (`*.db`, `*.sqlite*` and their
  journals), `sessions/`, `logs/` and `checkpoints/` `-agentfs`
  everywhere: never recorded, even if tracked or re-included by a later
  exclude change. Upstream Hermes v0.22.5 keeps its sessions in
  `state.db`, not `sessions/`; both are covered.
- [`config`](../deploy/examples/hermes/config) keeps nested repositories
  out (`nested = exclude`) and names the push remote.

`crates/agentfs/tests/deploy_examples.rs` checks these files against a
representative layout in every CI run. Check the profile layout against
the Hermes version you run.

### The pod changes

Starting from Hermes' own workload:

- **Add `agentfs-init` and the `agentfs` sidecar** from
  `deploy/sidecar/deployment.yaml`, with the volumes `data`,
  `agentfs-mount`, `config` and `credentials`, the Hermes rule files as
  the ConfigMap, and `AGENTFS_VOLUME=hermes`.
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
  `/opt/data`) so a clean exit of the daemon restarts it onto the new
  mount. Hermes' start-up runs as root, which the sealed mount point
  does not stop (§7). **Queued patches do not land in this pod** (see
  [Restarts and patches](#restarts-and-patches)).

  The alternative, which the read-only fence covers and under which
  patches land: mount the `emptyDir` read-only at `/agentfs`, as
  `deploy/sidecar`'s workload does, and set
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
- **Root init.** Hermes' start-up runs as root; with `default_permissions`
  and a real backing tree, root reads and writes the mount like any other
  directory, and files keep their owners.
- **The volume** changes from `ReadWriteOnce` to `ReadWriteOncePod`
  (§3 above), with the Deployment's `Recreate` strategy.

### Migrating the existing data

Hermes' current volume holds its files at the root. agentfs needs them
under `backing/`, next to `state/`. A one-time Job, with Hermes scaled to
zero and the same PVC mounted at `/data`:

<!-- ci: hermes-migration -->
```sh
set -eu
cd /data
mkdir backing
# Move everything except the new directory into it.
find . -mindepth 1 -maxdepth 1 ! -name backing -exec mv -t backing {} +
agentfs --state /data/state --root /data/backing init --volume hermes --include /etc/agentfs/config
cp /etc/agentfs/exclude /data/state/git/info/exclude
cp /etc/agentfs/attributes /data/state/git/info/attributes
# The initial import: record every file the rules accept. The list is
# null-separated so any name passes whole, and written first so a failing
# find stops the script.
cd /data/backing
list=$(mktemp)
find . -name .git -prune -o \( -type f -o -type l \) -printf '%P\0' >"$list"
xargs -0 -r -a "$list" agentfs --state /data/state --root /data/backing record
agentfs --state /data/state push
```

Run it with the agentfs image, as root, with the Hermes ConfigMap at
`/etc/agentfs` and the credentials Secret at `/run/secrets/agentfs`.
`record` reports each path it skips and why. Then deploy the new pod
spec; from then on every change is recorded through the mount.

### Restarts and patches

iac's patches to Hermes' files (the patch queue, §12) apply only at
mount, which for Hermes means a pod restart: iac queues them with
`agentfs patch submit` (or plain git) on the remote the sidecar pushes
to, then restarts the pod (`kubectl delete pod`, or a rollout). The
sidecar's startup probe allows for the fetch, which waits at most
`agentfs.patchFetchTimeout` (30s). A blocked queue does not stop the pod;
alert on `agentfs.patch.blocked`. See
[patch-queue.md](runbooks/patch-queue.md).

**With Hermes' data path mounted through `subPath`**, the kubelet still
holds the mount when the sidecar stops, through the subPath binds it made
for Hermes and docker-in-docker, so the stop keeps the attached mark
(design §12). The mark names the pod (`AGENTFS_POD_UID`), and the next
pod lands the queue over it: the old pod's binds cannot reach the new
pod's `emptyDir`. A restart of the sidecar alone under running Hermes
defers the queue (`PatchQueueDeferred`, reason `unclean_stop`), as it
should; the patches land at the next pod restart.

### How it is tested

Upstream Hermes is tested in four configurations, with the checks of
[`ci/hermes/lib.sh`](../ci/hermes/lib.sh). The `docker` workflow runs
this setup under Docker Compose: [`ci/e2e-hermes.sh`](../ci/e2e-hermes.sh) with
[`ci/hermes/compose.yaml`](../ci/hermes/compose.yaml) over the compose
example and its git server ([docker.md](docker.md)), with this
section's `exclude` and `attributes` and the example's remote settings
(the Hermes `config` names a placeholder remote). Hermes is the
upstream image, pinned by digest and started as shipped (`gateway run`,
root start-up), with the mount at `/opt/data`; its tool containers run on
a docker-in-docker service that has the mount too, and
`terminal.docker_volumes` passes it into them. A scripted stand-in for
the model API makes every turn deterministic. The job checks that:

- Hermes' root start-up runs on the mount and files keep their owners;
- a skill Hermes creates with its own skill tool is committed as its user
  and pushed;
- a tool container's edit, made through Hermes' terminal tool, reaches
  the Hermes container and is committed as root and pushed;
- `.env`, `state.db` (which holds the sessions), `response_store.db`,
  `logs/`, and an `auth.json` and a log a tool container writes inside
  `skills/`, are in the tree but never in the history, locally or on the
  remote;
- a patch queued with `agentfs patch submit` lands when Hermes and
  docker-in-docker are stopped and agentfs restarted, and Hermes' next
  model request carries the patched `SOUL.md`;
- with `terminal.docker_volumes` emptied, a tool container's write to
  `/opt/data` stays in the tool container, reaching neither Hermes nor
  the history; Hermes' `skills/` is read-only there; a file in the tool
  container's home lands in Hermes' `sandboxes/` and is never recorded;
  Hermes' own edits are still recorded.

The `hermes` workflow runs [`ci/e2e-hermes-kind.sh`](../ci/e2e-hermes-kind.sh)
on kind, one job per configuration, with the same checks (start-up,
Hermes' skill, a tool container's edit, exclusions, a queued patch at the
next start) and the manifests in [`ci/hermes/k8s`](../ci/hermes/k8s):

- **sidecar**: this pod, from `deploy/sidecar` with the Hermes rule
  files, `subPath` mounts and their liveness probes, docker-in-docker as
  a native sidecar. First upstream Hermes runs on the plain volume, then
  the migration Job above runs as written: the import records Hermes'
  files and pushes them, and reports `.env`, the databases, the logs and
  a nested repository as skipped. Then a clean exit of the sidecar alone:
  Hermes' liveness probe restarts it onto the new mount, it records
  again, and a queued patch is deferred (`PatchQueueDeferred`); it lands
  at the next pod restart. The import also records what Hermes keeps in `skills/` beside
  the skills: the bundled skills it syncs there and its own
  `.usage.json`, `.curator_ledger.jsonl` and `.bundled_manifest`.
- **fence**: the read-only fence alternative, on a fresh volume; the
  patch lands at the next pod restart.
- **csi**: Hermes and docker-in-docker in one pod on a claim of the CSI
  driver ([csi.md](csi.md)), with a `hermes` profile holding these
  rules; the patch lands at the next NodeStage.

Not tested: a profile's files, a real model, tool containers given only
part of the volume, docker-in-docker without the mount, the fence pod
across a restart of the sidecar alone, and Hermes' file tools under the
fence's `HERMES_WRITE_SAFE_ROOT`.

## 11. Limitations

- `linux/amd64` images only.
- Recording costs about 2 to 4 ms per changed small file, and closing a large
  file costs a copy on ext4 (2.9 s per clean GiB; 3 ms on XFS): put the
  backing volume on XFS when files are large, and keep bulk trees such as
  `node_modules/` off the mount. See [performance.md](performance.md).
- A hung daemon is not detected, and passthrough descriptors opened
  before a crash write unrecorded until closed (§7).
- A `subPath` consumer that runs as root with `CAP_DAC_OVERRIDE`,
  restarted while the daemon is being restarted after a clean exit, can
  write to the bare directory, unrecorded; the sealed mount point stops
  every other one, and the read-only `emptyDir` has no such gap (§7).
- The privileged sidecar (§9); the CSI driver ([csi.md](csi.md)) needs no
  privilege in workload pods.
- One pod per volume. The sidecar stays single-process (`agentfs mount`
  without `--history-service`). For several pods on one volume use the
  CSI driver ([csi.md](csi.md)), which runs a history service per volume
  and a front per pod, one node per volume at a time; multi-node access
  modes are refused (design §19.1, 2026-10-15).
- A state area on NFS must be NFS 4.1 or later, without `nolock` and
  with `local_lock=none`: the state lock is the only guard against a
  second writer, and agentfs refuses other NFS mounts at start. Even
  then, a node cut off but still running keeps writing: if another node
  mounts the same state area once the NFS lease expires, two writers can
  corrupt the journal. The CSI driver keeps every worktree on its node's
  own disk and refuses a network filesystem
  ([csi.md](csi.md#11-where-worktrees-live)); moving a volume is a new
  volume started from its branch ([runbooks/moving.md](runbooks/moving.md)).
- The image is not bit-reproducible (Debian packages are not pinned),
  and is not yet signed or shipped with an SBOM.
