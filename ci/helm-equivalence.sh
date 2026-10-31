#!/usr/bin/env bash
# Checks that the charts render what deploy/ holds (docs/helm.md §5):
# charts/agentfs-csi against deploy/csi and the StorageClasses of deploy/csi/example, and the sidecar library through
# deploy/examples/helm-sidecar against deploy/sidecar, the same with the
# sandbox overlay on (ci/helm/sidecar-sandbox-overlay-values.yaml) against
# deploy/examples/sidecar-sandbox-overlay, and the CSI chart's
# alerts option against deploy/alerts/prometheusrule.yaml. Object by object,
# field by field (ci/helm-equivalence.py says what is ignored).
#
# Needs helm, kubectl (for kustomize) and python3 with PyYAML.
#
#   ci/helm-equivalence.sh

set -euo pipefail
cd "$(dirname "$0")/.."

out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT

echo "== charts/agentfs-csi = deploy/csi + deploy/csi/example StorageClasses"
{
  kubectl kustomize deploy/csi
  echo "---"
  cat deploy/csi/example/storageclass.yaml
  echo "---"
  cat deploy/csi/example/fork-storageclass.yaml
} >"$out/csi-kustomize.yaml"
helm template agentfs-csi charts/agentfs-csi --namespace agentfs-csi \
  -f ci/helm/deploy-csi-values.yaml \
  --set-file profiles.example.config=deploy/csi/profiles/example.config \
  --set-file profiles.example.exclude=deploy/csi/profiles/example.exclude \
  --set-file profiles.example.attributes=deploy/csi/profiles/example.attributes \
  >"$out/csi-helm.yaml"
csi=0
python3 ci/helm-equivalence.py "$out/csi-kustomize.yaml" "$out/csi-helm.yaml" \
  --namespace agentfs-csi || csi=$?

echo "== deploy/examples/helm-sidecar (charts/agentfs-sidecar) = deploy/sidecar"
helm dependency update deploy/examples/helm-sidecar >/dev/null
kubectl kustomize deploy/sidecar >"$out/sidecar-kustomize.yaml"
helm template agentfs-example deploy/examples/helm-sidecar >"$out/sidecar-helm.yaml"
sidecar=0
python3 ci/helm-equivalence.py "$out/sidecar-kustomize.yaml" "$out/sidecar-helm.yaml" \
  --unhash agentfs-config || sidecar=$?

echo "== deploy/examples/helm-sidecar sandboxOverlay.enabled = deploy/examples/sidecar-sandbox-overlay"
kubectl kustomize deploy/examples/sidecar-sandbox-overlay >"$out/overlay-kustomize.yaml"
helm template agentfs-example deploy/examples/helm-sidecar \
  -f ci/helm/sidecar-sandbox-overlay-values.yaml >"$out/overlay-helm.yaml"
overlay=0
python3 ci/helm-equivalence.py "$out/overlay-kustomize.yaml" "$out/overlay-helm.yaml" \
  --unhash agentfs-config || overlay=$?

echo "== charts/agentfs-csi alerts.enabled = deploy/alerts/prometheusrule.yaml"
helm template agentfs-csi charts/agentfs-csi --namespace agentfs-csi \
  --set alerts.enabled=true --show-only templates/alerts.yaml >"$out/alerts-helm.yaml"
alerts=0
python3 ci/helm-equivalence.py deploy/alerts/prometheusrule.yaml "$out/alerts-helm.yaml" \
  --object-namespace agentfs-csi || alerts=$?

[ "$csi" = 0 ] && [ "$sidecar" = 0 ] && [ "$overlay" = 0 ] && [ "$alerts" = 0 ]
