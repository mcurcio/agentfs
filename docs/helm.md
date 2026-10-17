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
| [`charts/agentfs-csi`](../charts/agentfs-csi) | application | The CSI driver ([csi.md](csi.md)): CSIDriver, RBAC, the node plugin DaemonSet with its per-node csi-provisioner, profiles, credentials, the class admission policy and StorageClasses |
| [`charts/agentfs-sidecar`](../charts/agentfs-sidecar) | library | Helpers that add the phase 1 sidecar ([deployment.md](deployment.md)) to a workload's own chart |
| [`deploy/examples/helm-sidecar`](../deploy/examples/helm-sidecar) | application, not published | A workload using the helpers: `deploy/sidecar` as Helm renders it |

Both published charts are at `oci://ghcr.io/mcurcio/charts` (§7).

## 2. Install the CSI driver

The driver is one per cluster: its name, `agentfs.mcurcio.github.io`, and
its cluster-wide objects (CSIDriver, ClusterRoles, the admission policy)
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
| `profiles` | none | `deploy/csi`'s `example` profile points at a placeholder remote |
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
the chart, or not at all if image.yml fails. Nothing here creates tags.

```sh
helm show values oci://ghcr.io/mcurcio/charts/agentfs-csi --version <version>
```

## 8. Limitations

- One driver per cluster (§2); a second release fails on the cluster-wide
  objects.
- The chart does not create its namespace (§2).
- A new GHCR package can start private: check its visibility in the
  package settings after the first publish.
