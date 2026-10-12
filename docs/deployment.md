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

- **Contents:** the `agentfs` binary built with the `https` and `ssh`
  features (so every remote transport works), on `debian:trixie-slim`
  with `fuse3` (`fusermount3`), `libssl3t64`, `ca-certificates` (the TLS
  trust store) and `git`, so the runbooks' `git log` and `git show` work
  through `kubectl exec`. Both base images are pinned by digest.
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

**Startup order.** The sidecar is a native sidecar: an init container
with `restartPolicy: Always`. The kubelet starts it after `agentfs-init`
and starts the workload containers only once its **startup probe**
(`agentfs probe --allow-stale /agentfs/workspace`) passes, that is, once
the mount is live. The probe allows 5 minutes (`periodSeconds: 2`,
`failureThreshold: 150`) for journal recovery at start. Its **readiness
probe** (`agentfs probe /agentfs/workspace`) makes a dead daemon visible
as a NotReady pod. There is **no liveness probe**: restarting the sidecar
cannot remount safely (§6 below).

**One writer.** `ReadWriteOncePod` lets only one pod in the cluster use
the volume, and the Deployment's `Recreate` strategy stops the old pod
before the new one starts, so a rollout never has two pods (and two
daemons) on one state area (§2.3). The state-area lock is the last line
of defence. Most clusters need a CSI driver for `ReadWriteOncePod`.

**Termination.** Native sidecars are stopped after the workload
containers, so the daemon unmounts once nothing uses the mount, runs a
final sweep, makes the final commit and waits up to `--push-grace` (5 s)
for pushes. A workload container that mounts the FUSE root itself through
a `subPath` (Hermes, §9 below) pins the mount until the kubelet tears the
pod's volumes down, so the daemon detaches lazily and waits its
`--unmount-grace` (10 s) first. `terminationGracePeriodSeconds: 60`
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
key and a known_hosts file in the Secret and name them with
`agentfsSshKey` and `agentfsKnownHosts`
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
http/protobuf over plain http), or `OTEL_SDK_DISABLED=true`. The example
samples 1% of traces (`parentbased_traceidratio`, `0.01`); metrics and
alarm logs are not sampled. See the [README](../README.md#telemetry).

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
  pod mounts the same volume.
- **Changing rules or remotes:** edit `config/`, apply; the pod rolls
  (§4 above).
- **Pushes failing:** [stuck-pushes.md](runbooks/stuck-pushes.md).
  `agentfs push` is refused while mounted; the mount's own push loop
  retries.
- **Alerts:** page on `agentfs.push.oldest_unpushed_age` (history not
  leaving the pod), the sidecar's readiness (a dead daemon), its restart
  count, the `RecordFailed` and `CommitFailed` alarms, and
  `agentfs.patch.blocked` (a patch queue that needs its producer,
  [patch-queue.md](runbooks/patch-queue.md)).

## 7. A crashed daemon

Every change the workload saw succeed was journaled first, so a crash
loses nothing that returned. What phase 1 does about the dead mount:

1. The workload's file calls under `/agentfs/workspace` fail with
   `ENOTCONN` ("Transport endpoint is not connected"). agentfs never
   uses `auto_unmount`, so nothing can be written to the empty directory
   underneath, where it would not be recorded.
2. The kubelet restarts the sidecar container. It runs with
   `AGENTFS_ON_STALE_MOUNT=hold`: it sees that the mount point is an
   agentfs mount whose daemon died, **does not remount** (the workload
   containers still hold the dead mount, and swapping it under them is
   not safe), takes no lock and waits. Its startup probe passes
   (`--allow-stale`), its readiness probe fails: **the pod is NotReady**.
3. An operator (or your automation) **deletes the pod**:
   `kubectl delete pod <pod>`. The workload containers stop first; the
   holding sidecar then gets SIGTERM, detaches the stale mount so the
   kubelet can remove the `emptyDir`, and exits.
4. The new pod's sidecar recovers the journal (`N uncommitted record(s)
   recovered` in its startup line), commits and pushes.

The [crashed sidecar runbook](runbooks/crashed-sidecar.md) has the
commands. The kind end-to-end test does exactly this after `kill -9` of
the daemon.

**Known gaps:**

- The pod restart is manual in phase 1. Automating it (a controller, or
  Kubernetes' container-restart rules once they can restart a whole pod)
  is a follow-up.
- **A clean exit while the pod runs** is not held: if the daemon exits
  on its own (an operator sends it SIGTERM, something unmounts the
  mount, a `--passthrough on` failure), the restarted sidecar finds no
  stale mount and mounts again at once. Writes the workload made in
  between landed in the bare `emptyDir` directory and are hidden by the
  new mount, unrecorded. Do not signal the sidecar; delete the pod.
  The example uses `--passthrough auto` for this reason.
- Sending SIGTERM to a holding sidecar while the workload runs detaches
  the stale mount and exposes the bare directory the same way.

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
final commit when the pod is deleted, and the crash recovery of §7 after
`kill -9` of the daemon from the node. `kubectl apply --dry-run=server`
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
- giving new files to their creators needs `CAP_CHOWN` and `CAP_FOWNER`.

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
pinned, scanned, updated. The CSI driver (phase 4, §14) moves the mount
into a node plugin, so workload pods no longer need a privileged
container at all.

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
  exclude change.
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

  The e2e test's uid 10000 writer uses exactly this pattern. (The
  alternative is to mount the `emptyDir` at `/agentfs` and set
  `HERMES_HOME=/agentfs/workspace`.)
- **The docker-in-docker sidecar** that runs Hermes' file and terminal
  tools mounts it the same way, so tool containers see the same files,
  and the `docker_volumes` pin in the iac workload changes to pass the
  path into tool containers. That is a deliberate policy change, and the
  owner must approve it.
- **Root init.** Hermes' start-up runs as root; with `default_permissions`
  and a real backing tree, root reads and writes the mount like any other
  directory, and files keep their owners.
- **The volume** changes from `ReadWriteOnce` to `ReadWriteOncePod`
  (§3 above), with the Deployment's `Recreate` strategy.

### Migrating the existing data

Hermes' current volume holds its files at the root. agentfs needs them
under `backing/`, next to `state/`. A one-time Job, with Hermes scaled to
zero and the same PVC mounted at `/data`:

```sh
set -eu
cd /data
mkdir backing
# Move everything except the new directory into it.
find . -mindepth 1 -maxdepth 1 ! -name backing -exec mv -t backing {} +
agentfs --state /data/state --root /data/backing init --volume hermes --include /etc/agentfs/config
cp /etc/agentfs/exclude /data/state/git/info/exclude
cp /etc/agentfs/attributes /data/state/git/info/attributes
# The initial import: record every file the rules accept.
cd /data/backing
find . -name .git -prune -o \( -type f -o -type l \) -printf '%P\n' |
  xargs -r agentfs --state /data/state --root /data/backing record
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

## 11. Limitations

- `linux/amd64` images only.
- The pod restart after a crash is manual (§7).
- A sidecar exit that is not a crash is not held (§7).
- The privileged sidecar (§9); the CSI driver (phase 4) removes it.
- One pod per volume. The sidecar stays single-process (`agentfs mount`
  without `--history-service`). The history service and fronts exist for
  one node (`agentfs serve-history`, `mount --history-service`; design
  §14, [operator guide](operator-guide.md#16-several-mounts-of-one-volume-history-service)),
  but no manifests run them: several pods on one volume wait for the CSI
  driver, which will run the service per volume and a front per pod.
- The image is not bit-reproducible (Debian packages are not pinned),
  and is not yet signed or shipped with an SBOM.
