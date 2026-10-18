# Helm charts

Two charts install what [`deploy/`](../deploy) holds, for clusters run
with Helm. They render the same objects as the kustomize bases, and CI
checks that they do (§5).

- [1. What is here](#1-what-is-here)
- [2. Install the CSI driver](#2-install-the-csi-driver)
- [3. Values](#3-values)
- [4. The sidecar helpers](#4-the-sidecar-helpers)
- [5. deploy/ and the charts](#5-deploy-and-the-charts)
- [6. How it is tested](#6-how-it-is-tested)
- [7. Publishing](#7-publishing)
- [8. Limitations](#8-limitations)

## 1. What is here

| Chart | Type | What |
|---|---|---|
| [`charts/agentfs-csi`](../charts/agentfs-csi) | application | The CSI driver ([csi.md](csi.md)): CSIDriver, RBAC with the provisioner admission policy that narrows it, the node plugin DaemonSet with its per-node csi-provisioner, profiles, credentials, the class admission policy and StorageClasses; optionally a minimal git server (§2) |
| [`charts/agentfs-sidecar`](../charts/agentfs-sidecar) | library | Helpers that add the phase 1 sidecar ([deployment.md](deployment.md)) to a workload's own chart |
| [`deploy/examples/helm-sidecar`](../deploy/examples/helm-sidecar) | application, not published | A workload using the helpers: `deploy/sidecar` as Helm renders it |

Both published charts are at `oci://ghcr.io/mcurcio/charts` (§7).

## 2. Install the CSI driver

The driver is one per cluster: its name, `agentfs.mcurcio.github.io`, and
its cluster-wide objects (CSIDriver, ClusterRoles, the admission policies)
have fixed names, and object names do not carry the release name.

The node plugin is privileged (csi.md §10). Helm installs into a
namespace that already exists, so create it with the Pod Security labels
first (the same as [`deploy/csi/namespace.yaml`](../deploy/csi/namespace.yaml)),
and the credentials Secret out of band:

```sh
kubectl apply -f deploy/csi/namespace.yaml        # or create and label agentfs-csi
kubectl -n agentfs-csi create secret generic agentfs-csi-credentials \
  --from-literal=example-origin=$'username=agentfs\npassword=<token>'
helm install agentfs-csi oci://ghcr.io/mcurcio/charts/agentfs-csi \
  --namespace agentfs-csi --version <version> \
  --set credentials.existingSecret=agentfs-csi-credentials \
  --set-file profiles.example.config=deploy/csi/profiles/example.config \
  --set-file profiles.example.exclude=deploy/csi/profiles/example.exclude \
  -f storageclasses.yaml
```

where `storageclasses.yaml` names the classes and their parameters:

```yaml
storageClasses:
  - name: agentfs
    parameters:
      profile: example
      volumeName: "{pvcNamespace}.{pvcName}.{pvName}"
      onDelete: retain
  # A fork class (csi.md §14): claims cloning an `agentfs` claim. `link:
  # unlinked` makes independent forks that never deliver back to their
  # source and never push to its remotes; omit it (or `linked`) for forks
  # that do deliver. `remotes` names the profile holding the forks' own
  # remotes (`profiles.sandbox-remotes.config`); without it they are
  # local-only, and deleting a claim deletes its history.
  - name: agentfs-sandbox
    parameters:
      fork: "true"
      link: unlinked
      remotes: sandbox-remotes
```

The release notes (`helm status`) warn when the namespace enforces a Pod
Security level other than `privileged`, and when no profile or
StorageClass is set.

There is no controller Deployment: the DaemonSet runs on every node, and
each node's csi-provisioner (`--node-deployment`) provisions the claims
whose pods are scheduled there, on that node's disk under
`basePath.path` (csi.md §2, §11). Give that path a disk of its own on
each node: sizes are not enforced, and a full disk makes the kubelet
evict every pod on the node.

**Upgrades.** The node plugin's update strategy is `OnDelete`, as in
`deploy/csi`: `helm upgrade` changes the DaemonSet, and each node takes
the new plugin when you delete its pod, one drained node at a time
([runbooks/csi-recovery.md](runbooks/csi-recovery.md)). Profiles reach
the running pods without a restart (csi.md §4).

### The git server

`server.enabled=true` runs the chart's minimal git server, a remote for
the profiles inside the cluster: the owner asked for "Something lighter.
This is a minimal fit remote" and "Ideally it's optional config in the
one helm chart" (2026-10-16, design §19.1). It is off by default.

- **What runs.** One pod of the StatefulSet `agentfs-git-server`, the
  image `ghcr.io/mcurcio/agentfs-git-server`
  ([`examples/docker/git-server`](../examples/docker/git-server): nginx,
  basic auth and `git http-backend`), as uid 33 with a read-only root
  filesystem. An init container creates the bare repository
  `history.git` once, and at every start sets `core.fsync committed`:
  git's default leaves loose objects (how a small push arrives) unsynced
  while it syncs the ref, so an unclean shutdown of the server's node
  could leave a branch naming objects that are gone, after a driver saw
  that branch and deleted its own copy (csi.md §7). The Service serves it at
  `http://agentfs-git-server.<namespace>.svc:<server.service.port>/git/history.git`
  (port 8080 by default).
- **Where the history lives.** On the claim `repos-agentfs-git-server-0`,
  of `server.storage.storageClassName` (empty: the cluster default) with
  `server.storage.size` and `server.storage.accessModes`. Any
  StorageClass works, NFS 4.0 and iSCSI included. The claim template is
  fixed at install, since a StatefulSet's cannot change, and
  `helm uninstall` keeps the claim.
- **Credentials, wired once.** One Secret holds `htpasswd`, which the
  server reads, and `token`, an agentfs credentials file (`username=`
  and `password=` lines) that the node plugin mounts as
  `/etc/agentfs-csi/credentials/agentfs-server`. With
  `server.credentials.existingSecret` empty, the chart makes the Secret
  `agentfs-git-server` with user `agentfs` and a random password, and
  keeps it on upgrade (`lookup`; `helm template` and `--dry-run` show a
  fresh one each time). To bring your own, put the same password in both
  keys:

  ```sh
  kubectl -n agentfs-csi create secret generic my-git-server \
    --from-literal=htpasswd="$(htpasswd -nbB agentfs "$PASSWORD")" \
    --from-literal=token=$'username=agentfs\npassword='"$PASSWORD"
  ```

  The credentials Secret
  (`credentials.*`) must not have its own `agentfs-server` key.
- **A profile pushing to it**, as the release notes print it (csi.md §4):

  ```ini
  [remote "server"]
      url = http://agentfs-git-server.agentfs-csi.svc:8080/git/history.git
      agentfsCredentials = /etc/agentfs-csi/credentials/agentfs-server
  ```

  Node plugins already running when the server is first enabled see the
  `agentfs-server` key only after their pod is deleted (`OnDelete`,
  above).

What it does not do: there is one replica and no fencing, no per-volume
keys and no hooks (design "The server"), so any profile with the token
can write any ref. It speaks plain HTTP inside the cluster, with no TLS.
If its node is lost and its claim is on that node's disk, the remote is
down until the node returns. Pull requests and the like come from adding
another remote, such as GitHub, to the profile.

## 3. Values

Every value is in the charts' READMEs, generated from `values.yaml` by
helm-docs: [agentfs-csi](../charts/agentfs-csi/README.md) and
[agentfs-sidecar](../charts/agentfs-sidecar/README.md). Each chart has a
`values.schema.json`, so an unknown key or a wrong type fails the install.

The defaults are those of `deploy/`, with these differences:

| Value | Default | Why |
|---|---|---|
| `image.tag` (CSI) | the chart's appVersion | a published chart pins the image of its own commit (§7); in the repository it is `latest` |
| `image.tag` (sidecar) | none, required | the library cannot know its consumer's release, so it refuses to render without a tag or digest |
| `profiles` | none | `deploy/csi`'s `example` profile points at a placeholder remote; a profile with no `[remote]` section makes local-only volumes ([configuration](configuration.md#remotes-modes)) |
| `credentials.existingSecret` | empty: the chart renders an empty `agentfs-csi-credentials` Secret | set it to keep secrets out of the release record |
| `storageClasses` | none | they belong to the workloads' operator; `deploy/csi/example` has two |

StorageClass `parameters` are passed to the driver as written
([configuration.md](configuration.md#the-csi-driver-agentfs-csi)): new
driver parameters need no chart change, and a start class
(`startFrom`) is an ordinary entry ([moving](runbooks/moving.md)).
`reclaimPolicy` is always `Delete` (the schema allows nothing else, and
the class admission policy refuses any other class of the driver):
DeleteVolume must run for the driver's release decider to answer, and
`onDelete: retain` is what keeps a worktree. `classAdmission.enabled`
(on by default) installs that policy, `agentfs-class`; with it off a
class with `Retain` loses nothing, but its PVs stay `Released` for ever.

`alerts.enabled=true` installs the example alert rules of
[`deploy/alerts`](../deploy/alerts) as a Prometheus Operator
`PrometheusRule` in the release namespace, with `alerts.labels` for your
Prometheus's `ruleSelector` (deployment.md "Alerts"). It is off by
default and needs the PrometheusRule CRD. The chart reads
`deploy/alerts/agentfs.rules.yml` through a symlink
(`charts/agentfs-csi/files/`), so the rules have one home. Promote the
`agentfs.volume` resource attribute in Prometheus: under the CSI driver
the volumes on a node share one `instance` (deployment.md "Names and
labels").

`sidecars.provisioner.extraArgs` passes flags to each node's
csi-provisioner (`--retry-interval-max=20s` retries a waiting
DeleteVolume sooner), and `node.extraArgs` to `agentfs-csi`
(`--release-scan-interval`).

## 4. The sidecar helpers

`agentfs-sidecar` is a library chart: it renders nothing by itself. A
workload's chart depends on it under an alias and includes its helpers
where the pod needs them:

```yaml
# Chart.yaml
dependencies:
  - name: agentfs-sidecar
    version: <version>
    repository: oci://ghcr.io/mcurcio/charts
    alias: agentfs
```

```yaml
# values.yaml
agentfs:
  image: { tag: <40-char commit> }
  volume: my-workload
  config:
    settings: |
      [remote "origin"]
      	url = https://git.example.com/acme/agent-history.git
      	agentfsCredentials = /run/secrets/agentfs/origin
```

```yaml
# templates/deployment.yaml (the pod spec)
    metadata:
      annotations:
        {{- include "agentfs-sidecar.podAnnotations" .Values.agentfs | nindent 8 }}
    spec:
      terminationGracePeriodSeconds: 60
      initContainers:
        {{- include "agentfs-sidecar.initContainers" .Values.agentfs | nindent 8 }}
      containers:
        - name: workload
          volumeMounts:
            {{- include "agentfs-sidecar.workspaceMount" .Values.agentfs | nindent 12 }}
      volumes:
        {{- include "agentfs-sidecar.volumes" .Values.agentfs | nindent 8 }}
```

plus `agentfs-sidecar.configMap`, `.secret` and `.claim` for the objects
they mount. The workload uses `/agentfs/workspace`. What the pod itself
must do (one pod at a time on the claim, a grace period long enough for
the final commit) is in deployment.md §3;
[`deploy/examples/helm-sidecar`](../deploy/examples/helm-sidecar) is a
complete chart, with the settings and rules kept as files of the chart.

Helm checks the library's values against its `values.schema.json` under
the alias's key.

## 5. deploy/ and the charts

`deploy/` stays the kustomize source, and
[`ci/helm-equivalence.sh`](../ci/helm-equivalence.sh) checks that the
charts render the same objects:

- `charts/agentfs-csi` with [`ci/helm/deploy-csi-values.yaml`](../ci/helm/deploy-csi-values.yaml)
  and the `deploy/csi/profiles` files equals `kubectl kustomize deploy/csi`
  plus the two StorageClasses of `deploy/csi/example`;
- `deploy/examples/helm-sidecar` equals `kubectl kustomize deploy/sidecar`;
- `charts/agentfs-csi` with `alerts.enabled=true` renders
  `deploy/alerts/prometheusrule.yaml` (in the release namespace).

The git server (`server.enabled`, §2) is the chart's alone: `deploy/csi`
has no kustomize equivalent, and the comparison renders it off, which
leaves the node plugin's credentials volume the plain Secret of
`deploy/csi`. A kustomize user runs the image from
[`examples/docker/git-server`](../examples/docker/git-server) with
their own manifests.

The comparison ([`ci/helm-equivalence.py`](../ci/helm-equivalence.py))
is object by object and field by field. It ignores only what Helm adds by
design: the release's Namespace object, Helm's standard labels, `checksum/`
annotations, and kustomize's hash suffix on the sidecar's ConfigMap. A
change to either side without the other fails the lint job with a diff.

Run it locally with helm, kubectl and python3 with PyYAML:

```sh
ci/helm-equivalence.sh
helm-docs --chart-search-root charts   # after changing values.yaml
```

## 6. How it is tested

The `helm` workflow ([`.github/workflows/helm.yml`](../.github/workflows/helm.yml)),
on every push and pull request:

- **lint:** `helm lint --strict` of both charts and the example, `ct lint`
  (chart-testing, [`ci/ct.yaml`](../ci/ct.yaml): Chart.yaml schema,
  yamllint, the values in `charts/agentfs-csi/ci/`), the READMEs current
  with helm-docs ([`ci/install-helm-docs.sh`](../ci/install-helm-docs.sh)),
  and §5's equivalence.
- **e2e (CSI chart on kind):** the image built as image.yml builds it,
  a kind cluster, then `ct install` of `charts/agentfs-csi`
  ([`ci/helm-csi-install.sh`](../ci/helm-csi-install.sh)) with the image
  and the `ci` profile e2e-csi uses, and the release kept; then
  [`ci/e2e-csi.sh`](../ci/e2e-csi.sh) with `CSI_INSTALL=chart` runs every
  e2e-csi check against the chart-installed driver (csi.md §13: RBAC,
  two restricted pods on one claim, crashes and recovery, refusals,
  restore and revert, teardown). The sidecar example then passes
  server-side validation (`kubectl apply --dry-run=server`).
- **e2e (CSI move between nodes)**, in the f8 workflow: `helm install` of
  `charts/agentfs-csi` from this tree on a two-worker kind cluster
  ([`ci/k8s-csi/move/values.yaml`](../ci/k8s-csi/move/values.yaml)), and
  [`ci/e2e-csi-move.sh`](../ci/e2e-csi-move.sh) (csi.md §13).
- **e2e (the chart's git server)**, in the f8 workflow: `helm lint
  --strict` and rendering checks with
  [`charts/agentfs-csi/ci/server.yaml`](../charts/agentfs-csi/ci/server.yaml)
  (not named `*-values.yaml`, so `ct install` above does not take it),
  then `helm install` on one kind node and
  [`ci/e2e-chart-server.sh`](../ci/e2e-chart-server.sh): a pod's write
  reaches the server as `agentfs/<volume>` at the worktree's head (read
  with `git ls-remote` through `kubectl port-forward`), the repository
  has `core.fsync committed`, a wrong password
  is refused, the repository survives deleting the server pod, and
  `helm upgrade` keeps the password.
- **git server image**, in the image workflow: the image is built, then
  takes a push and refuses a wrong password, run as the chart runs it.

Not tested: the forks e2e (`ci/e2e-csi-forks.sh`) against the chart,
`helm upgrade` between chart versions, the `PrometheusRule` against a
cluster with its CRD (kind has none; only its equivalence and the alerts
workflow's checks of the rules), the sidecar chart on a cluster
(only its equivalence with `deploy/sidecar`, which image.yml's e2e-kind
runs), and installing from the published OCI artifacts.

## 7. Publishing

After lint and e2e pass on a push to `main` or a `v*` tag, the `publish`
job packages both charts and pushes them with the workflow's
`GITHUB_TOKEN` (`packages: write`) to `oci://ghcr.io/mcurcio/charts`:

| Push | Chart version | `agentfs-csi` appVersion |
|---|---|---|
| `main` | `<Chart.yaml version>-main.<run number>` | the commit |
| tag `vX.Y.Z` | `X.Y.Z` | the commit |

The appVersion is the default image tag, `ghcr.io/mcurcio/agentfs:<commit>`,
which image.yml publishes from `main` once its own tests pass; the two
workflows run side by side, so the image can appear a few minutes after
the chart, or not at all if image.yml fails. The git server's default
tag is the same appVersion: image.yml publishes
`ghcr.io/mcurcio/agentfs-git-server:<commit>` and `:latest` from `main`
once its own test passes. Nothing here creates tags.

```sh
helm show values oci://ghcr.io/mcurcio/charts/agentfs-csi --version <version>
```

## 8. Limitations

- One driver per cluster (§2); a second release fails on the cluster-wide
  objects.
- The chart does not create its namespace (§2).
- A new GHCR package can start private: check its visibility in the
  package settings after the first publish.
