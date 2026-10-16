# Deploying agentfs

| Path | What |
|---|---|
| [`sidecar/`](sidecar) | A kustomize base: a Deployment with agentfs as a privileged native sidecar (phase 1, design §17.1), a `ReadWriteOncePod` PVC, the operator ConfigMap (settings, `info/exclude`, `info/attributes`) and the credentials Secret |
| [`csi/`](csi) | The CSI driver (phase 4, design §14.2): its namespace, CSIDriver, RBAC, controller Deployment, node DaemonSet, the profiles ConfigMap (`profiles/`) and the credentials Secret; sidecars pinned by digest. Installs only the driver |
| [`csi/example/`](csi/example) | A workload on the driver: a restricted namespace, a StorageClass using the `example` profile, a `ReadWriteOnce` claim and a two-replica Deployment |
| [`alerts/`](alerts) | Example alert rules for agentfs's metrics: a Prometheus rules file, the same as a `PrometheusRule`, and their promtool tests ([docs/deployment.md](../docs/deployment.md#alerts)) |
| [`examples/hermes/`](examples/hermes) | Operator rules for Hermes, the first consumer (design §16) |
| [`examples/helm-sidecar/`](examples/helm-sidecar) | A workload chart using the `agentfs-sidecar` Helm library: `sidecar/` as Helm renders it, its settings and rules linked from `sidecar/config` |

The Helm charts in [`../charts`](../charts) render the same objects as
`sidecar/` and `csi/`; CI checks they do ([docs/helm.md](../docs/helm.md)).

The sidecar is explained in [docs/deployment.md](../docs/deployment.md):
the image, the pod layout and why, configuration, users and permissions,
operations, a crashed daemon, the security trade-offs of the privileged
sidecar, and the Hermes example.

The CSI driver is explained in [docs/csi.md](../docs/csi.md). In order:

1. edit `csi/profiles/` (one `<profile>.config`, optional `.exclude` and
   `.attributes`) and create the `agentfs-csi-credentials` Secret (or edit
   `csi/credentials.yaml`);
2. `kubectl apply -k deploy/csi`, and wait for the `agentfs-csi-node`
   pods and the controller;
3. create StorageClasses naming your profiles, then claims; or
   `kubectl apply -k deploy/csi/example`.

`ci/k8s/` is the CI overlay of `sidecar/` that the `e2e-kind` job deploys
to a kind cluster ([`ci/e2e-kind.sh`](../ci/e2e-kind.sh)); `ci/k8s-csi/`
the overlay of `csi/` and the workloads the `e2e-csi` job deploys
([`ci/e2e-csi.sh`](../ci/e2e-csi.sh)).
