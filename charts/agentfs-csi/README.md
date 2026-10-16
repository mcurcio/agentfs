# agentfs-csi

The agentfs CSI driver: volumes whose every change is recorded in git history the workload cannot see (design §14.2).

Generated from `values.yaml` by [helm-docs](https://github.com/norwoodj/helm-docs)
(`helm-docs --chart-search-root charts`); CI fails when it is stale. How to
install and run it is in [docs/helm.md](../../docs/helm.md); the driver
itself in [docs/csi.md](../../docs/csi.md).

```sh
kubectl create namespace agentfs-csi
kubectl label namespace agentfs-csi pod-security.kubernetes.io/enforce=privileged
helm install agentfs-csi oci://ghcr.io/mcurcio/charts/agentfs-csi \
  --namespace agentfs-csi --version <version> \
  --set credentials.existingSecret=agentfs-csi-credentials \
  --set-file profiles.example.config=example.config
```

## Requirements

Kubernetes: `>=1.30.0-0`

## Values

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| alerts.enabled | bool | `false` | Install the example alert rules of deploy/alerts as a Prometheus Operator PrometheusRule (docs/deployment.md "Alerts"). Needs its CRD. |
| alerts.labels | object | `{}` | More labels on the PrometheusRule, for your Prometheus's ruleSelector (kube-prometheus-stack selects `release: <its release>`). |
| basePath.path | string | `"/var/lib/agentfs-csi/volumes"` | Where volumes live (`--base-path`): one filesystem, at this path on the controller and every node (docs/csi.md §3). |
| basePath.volume | object | `{}` | The volume mounted at `path` in the controller and the node plugin, as a pod volume source: one shared filesystem on several nodes, which on NFS must be 4.1 or later (docs/csi.md §11). Empty means a hostPath of `path`, right for a single node. |
| controller.affinity | object | `{}` | The controller's affinity. |
| controller.extraArgs | list | `[]` |  |
| controller.nodeSelector | object | `{}` | The controller's node selector. |
| controller.podAnnotations | object | `{}` | Annotations on the controller's pods. |
| controller.priorityClassName | string | `"system-cluster-critical"` | The controller's priority class. |
| controller.replicas | int | `1` | Controller replicas. The sidecars elect a leader: a second replica is a hot standby. |
| controller.resources | object | `{}` | The agentfs-csi container's resources. |
| controller.tolerations | list | `[]` | The controller's tolerations. |
| credentials.existingSecret | string | `""` | Use this existing Secret for remote credentials instead of the chart's; create it out of band (`kubectl create secret generic`). |
| credentials.stringData | object | `{}` | Keys of the chart's own Secret, mounted at `/etc/agentfs-csi/credentials/<key>`. Prefer `existingSecret`: values end up in the release record. |
| forkAdmission.enabled | bool | `true` | Install the ValidatingAdmissionPolicy that refuses a fork class whose reclaim policy is not Delete (docs/csi.md §14). |
| image.digest | string | `""` | A digest (`sha256:...`); when set, it pins the image. |
| image.pullPolicy | string | `""` | The pull policy; empty leaves the Kubernetes default. |
| image.repository | string | `"ghcr.io/mcurcio/agentfs"` | The agentfs image: `agentfs-csi` and the `agentfs` it starts. |
| image.tag | string | `""` | Its tag; empty means the chart's appVersion (a published chart's commit id). |
| imagePullSecrets | list | `[]` | Pull secrets for every image, as `[{name: ...}]`. |
| kubeletDir | string | `"/var/lib/kubelet"` | The kubelet's directory on the nodes. Some distributions move it (k0s: `/var/lib/k0s/kubelet`). |
| node.affinity | object | `{}` | The node plugin's affinity. |
| node.env | list | `[{"name":"OTEL_SDK_DISABLED","value":"true"}]` | Environment of the node plugin, inherited by every history service and front: the `OTEL_*` variables (README "Telemetry"). Point `OTEL_EXPORTER_OTLP_ENDPOINT` at your collector to turn telemetry on. |
| node.extraArgs | list | `[]` |  |
| node.nodeSelector | object | `{}` | The node plugin's node selector. |
| node.podAnnotations | object | `{}` | Annotations on the node plugin's pods. |
| node.priorityClassName | string | `"system-node-critical"` | The node plugin's priority class. |
| node.resources | object | `{}` | The agentfs-csi container's resources. Every history service and front on the node runs in this container, inside these limits. |
| node.stateDir | string | `"/var/lib/agentfs-csi/node"` | Where the node keeps its staged and published records (`--node-state`); must survive plugin restarts. |
| node.terminationGracePeriodSeconds | int | `60` | Longer than `--unpublish-grace` plus `--unstage-grace`, so a stopped plugin lets its children make their final commits. |
| node.tolerations | list | `[{"operator":"Exists"}]` | The node plugin runs on every node by default. |
| profiles | object | `{}` | Profiles: every setting of a volume, by name (docs/csi.md "Profiles and credentials"). Each is `{config, exclude, attributes}`: the git-config fragment and the optional rule files. A StorageClass picks one with the parameter `profile: <name>`. Load files with `--set-file profiles.<name>.config=<path>`. |
| sidecars.attacher.extraArgs | list | `[]` | More csi-attacher flags. |
| sidecars.attacher.image.digest | string | `"sha256:d1a26170efed45491074ca4545c81844eee2b9043fb885ac01fb4aed6f8b72d2"` | csi-attacher's digest. |
| sidecars.attacher.image.repository | string | `"registry.k8s.io/sig-storage/csi-attacher"` | csi-attacher's image. |
| sidecars.attacher.image.tag | string | `"v4.13.0"` | csi-attacher's tag. |
| sidecars.attacher.resources | object | `{}` | csi-attacher's resources. |
| sidecars.livenessProbe.image.digest | string | `"sha256:19f2cf2f40e1987c7943945deca5f65cd4e4bf5cc0657fc78a5df131861578e8"` | livenessprobe's digest. |
| sidecars.livenessProbe.image.repository | string | `"registry.k8s.io/sig-storage/livenessprobe"` | livenessprobe's image. |
| sidecars.livenessProbe.image.tag | string | `"v2.20.0"` | livenessprobe's tag. |
| sidecars.livenessProbe.resources | object | `{}` | livenessprobe's resources. |
| sidecars.nodeDriverRegistrar.image.digest | string | `"sha256:b7fefd08651f00ac4df1a196ed4c621c2451fbb899297fe9fad9e36c667af0c0"` | node-driver-registrar's digest. |
| sidecars.nodeDriverRegistrar.image.repository | string | `"registry.k8s.io/sig-storage/csi-node-driver-registrar"` | node-driver-registrar's image. |
| sidecars.nodeDriverRegistrar.image.tag | string | `"v2.18.0"` | node-driver-registrar's tag. |
| sidecars.nodeDriverRegistrar.resources | object | `{}` | node-driver-registrar's resources. |
| sidecars.provisioner.extraArgs | list | `[]` | More csi-provisioner flags, e.g. `--retry-interval-max=20s`. |
| sidecars.provisioner.image.digest | string | `"sha256:6ebe60fd8ed67a7f8a90d34402e9e706b274e53612d6fa5fbb8285728db622e2"` | csi-provisioner's digest. |
| sidecars.provisioner.image.repository | string | `"registry.k8s.io/sig-storage/csi-provisioner"` | csi-provisioner's image. |
| sidecars.provisioner.image.tag | string | `"v6.4.0"` | csi-provisioner's tag. |
| sidecars.provisioner.resources | object | `{}` | csi-provisioner's resources. |
| storageClasses | list | `[]` | StorageClasses of the driver: `[{name, parameters, reclaimPolicy, volumeBindingMode, annotations}]`. `parameters` go to the driver as written (docs/configuration.md "The CSI driver"). `reclaimPolicy` defaults to Delete for a fork class (`fork: "true"`) and Retain otherwise; `volumeBindingMode` to WaitForFirstConsumer. |
