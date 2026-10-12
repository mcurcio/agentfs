# Deploying agentfs

| Path | What |
|---|---|
| [`sidecar/`](sidecar) | A kustomize base: a Deployment with agentfs as a privileged native sidecar (phase 1, design §17.1), a `ReadWriteOncePod` PVC, the operator ConfigMap (settings, `info/exclude`, `info/attributes`) and the credentials Secret |
| [`examples/hermes/`](examples/hermes) | Operator rules for Hermes, the first consumer (design §16) |

Everything is explained in [docs/deployment.md](../docs/deployment.md):
the image, the pod layout and why, configuration, users and permissions,
operations, a crashed daemon, the security trade-offs of the privileged
sidecar, and the Hermes example.

`ci/k8s/` is the CI overlay of `sidecar/` that the `e2e-kind` job deploys
to a kind cluster ([`ci/e2e-kind.sh`](../ci/e2e-kind.sh)).
