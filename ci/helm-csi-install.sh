#!/usr/bin/env bash
# Installs the CSI driver from charts/agentfs-csi with chart-testing
# (`ct install`, its values in charts/agentfs-csi/ci/) into the kind
# cluster of the helm workflow's e2e job, for ci/e2e-csi.sh to test with
# CSI_INSTALL=chart (docs/helm.md §6). The release is kept
# (--skip-clean-up) so the e2e runs against it.
#
# Before the install, what the CI overlay ci/k8s-csi/driver adds to
# deploy/csi: the privileged namespace (deploy/csi/namespace.yaml), the
# runner's git server Service and the credentials of the `ci` profile.
#
# Needs kubectl, helm and ct, and a kind cluster with agentfs:ci loaded.

set -euo pipefail
cd "$(dirname "$0")/.."

NS=agentfs-csi
kubectl apply -f deploy/csi/namespace.yaml
kubectl apply -f ci/k8s-csi/driver/git-server.yaml
# The CI-only password of the throwaway git server (ci/e2e-csi.sh).
kubectl -n "$NS" create secret generic agentfs-csi-ci-credentials \
  --from-literal=ci-origin=$'username=agentfs\npassword=ci-csi-password' \
  --dry-run=client -o yaml | kubectl apply -f -

profiles=ci/k8s-csi/driver/profiles
ct install --config ci/ct.yaml --charts charts/agentfs-csi --release-name agentfs-csi \
  --namespace "$NS" --skip-clean-up \
  --helm-extra-set-args "--set-file=profiles.ci.config=$profiles/ci.config --set-file=profiles.ci.exclude=$profiles/ci.exclude --set-file=profiles.ci.attributes=$profiles/ci.attributes"

helm list -n "$NS"
