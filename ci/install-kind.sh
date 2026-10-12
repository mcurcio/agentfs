#!/usr/bin/env bash
# Installs pinned kind and kubectl into <dir>/bin, checking their sha256
# sums, for the e2e-kind job (ci/e2e-kind.sh). Linux amd64 only.
#
# Usage: ci/install-kind.sh <dir>
#
# Prints, for $GITHUB_ENV or `export`:
#   KIND_NODE_IMAGE   the kindest/node image for this kind release, by digest
# Add <dir>/bin to PATH yourself.

set -euo pipefail

# kind v0.33.0 (https://github.com/kubernetes-sigs/kind/releases/tag/v0.33.0)
# and its default node image, Kubernetes v1.37.0; kubectl of the same minor.
KIND_VERSION=v0.33.0
KIND_SHA256=aee6151561422756b764a4ae28e7f44cda5af5a9eead3cc9985112b1de8d8e0d
KIND_NODE_IMAGE=kindest/node:v1.37.0@sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5
KUBECTL_VERSION=v1.37.0
KUBECTL_SHA256=6129359f4e1f3848a5572ccb0b26cf28b8ca08cef38c95a765b2f64a2c961a2f

dir=${1:?install directory}
mkdir -p "$dir/bin"

fetch() { # url dest sha256
  curl -fsSL --retry 4 --retry-all-errors -o "$2" "$1"
  echo "$3  $2" | sha256sum -c --quiet - >&2
  chmod 0755 "$2"
}

fetch "https://github.com/kubernetes-sigs/kind/releases/download/$KIND_VERSION/kind-linux-amd64" \
  "$dir/bin/kind" "$KIND_SHA256"
fetch "https://dl.k8s.io/release/$KUBECTL_VERSION/bin/linux/amd64/kubectl" \
  "$dir/bin/kubectl" "$KUBECTL_SHA256"

echo "KIND_NODE_IMAGE=$KIND_NODE_IMAGE"
