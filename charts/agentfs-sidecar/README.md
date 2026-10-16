# agentfs-sidecar

Helpers that add agentfs to a workload's chart as a phase 1 native sidecar (design §17.1): its containers, volumes, settings and claim.

Generated from `values.yaml` by [helm-docs](https://github.com/norwoodj/helm-docs)
(`helm-docs --chart-search-root charts`); CI fails when it is stale. How to
use it is in [docs/helm.md](../../docs/helm.md) §4, with a worked example in
[deploy/examples/helm-sidecar](../../deploy/examples/helm-sidecar); the
sidecar itself in [docs/deployment.md](../../docs/deployment.md).

```yaml
# Chart.yaml of your workload
dependencies:
  - name: agentfs-sidecar
    version: <version>
    repository: oci://ghcr.io/mcurcio/charts
    alias: agentfs
```

| Helper | Renders |
|---|---|
| `agentfs-sidecar.initContainers` | the init container and the recorder (a native sidecar), for `initContainers` |
| `agentfs-sidecar.workspaceMount` | the workload container's mount of `/agentfs` (read-only; the workspace is `/agentfs/workspace`) |
| `agentfs-sidecar.volumes` | the pod's volumes: the claim, the shared emptyDir, the ConfigMap and the Secret |
| `agentfs-sidecar.podAnnotations` | a checksum of the settings, so changing them rolls the pod |
| `agentfs-sidecar.configMap`, `.secret`, `.claim` | the ConfigMap, the credentials Secret and the claim |

Each takes this library's values, e.g.
`{{ include "agentfs-sidecar.initContainers" .Values.agentfs | nindent 8 }}`.

## Values

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| claim.accessModes | list | `["ReadWriteOncePod"]` | Its access modes. ReadWriteOncePod makes this pod the single writer (design §2.3). |
| claim.name | string | `"agentfs-data"` | The claim holding the backing tree and the state area, mounted at `/data` in the agentfs containers only. |
| claim.size | string | `"10Gi"` | Its size. |
| claim.storageClassName | string | `""` | Its StorageClass; empty means the cluster default. XFS or btrfs make recording cost the same whatever a file's size (reflinks). |
| config.attributes | string | `""` | Mounted over `<state>/git/info/attributes`: agentfs attributes (design §4.2). |
| config.exclude | string | `""` | Mounted over `<state>/git/info/exclude`: paths never recorded when new (design §4.1). |
| config.name | string | `"agentfs-config"` | The ConfigMap the helpers render and mount. |
| config.settings | string | `""` | Operator settings and remotes, included from the state area's config (git-config syntax; every key is in docs/configuration.md). Must not set agentfs.volume or agentfs.branch. |
| credentials.secretName | string | `"agentfs-credentials"` | The Secret mounted at `/run/secrets/agentfs` in the sidecar. Create it out of band, or set `stringData` for the helpers to render it. |
| credentials.stringData | object | `{}` | Keys of a Secret for `agentfs-sidecar.secret` to render. Prefer an out-of-band Secret: values end up in the release record. |
| env | list | `[{"name":"OTEL_EXPORTER_OTLP_ENDPOINT","value":"http://otel-collector.observability.svc:4318"},{"name":"OTEL_SERVICE_NAME","value":"agentfs"},{"name":"OTEL_RESOURCE_ATTRIBUTES","value":"k8s.container.name=agentfs"},{"name":"OTEL_TRACES_SAMPLER","value":"parentbased_traceidratio"},{"name":"OTEL_TRACES_SAMPLER_ARG","value":"0.01"}]` | More environment of the sidecar: OpenTelemetry (docs/telemetry.md). |
| image.digest | string | `""` | A digest (`sha256:...`); when set, it pins the image. |
| image.pullPolicy | string | `""` | The pull policy; empty leaves the Kubernetes default. |
| image.repository | string | `"ghcr.io/mcurcio/agentfs"` | The agentfs image. |
| image.tag | string | `""` | Its tag: required unless `digest` is set. Pin a commit tag (`<40-char sha>`); `latest` follows main. |
| labels | object | `{}` | Labels of the ConfigMap, Secret and claim the helpers render. |
| onStaleMount | string | `"recover"` | What the sidecar does on a stale mount after a crash: `recover` mounts over it, `hold` waits for a pod restart (design §17.1). |
| resources | object | `{"limits":{"memory":"512Mi"},"requests":{"cpu":"50m","memory":"64Mi"}}` | The sidecar's resources. An OOM kill is a crash: size the limit for the volume. |
| sealMountpoint | bool | `true` | Seal the bare mount point (root's, mode 0000) so a non-root workload cannot write where nothing records it (design §17.1). |
| volume | string | `""` | The agentfs volume name (required); the history branch is `agentfs/<volume>` (design §4.3). |
