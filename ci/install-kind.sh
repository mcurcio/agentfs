#!/usr/bin/env bash
# Installs pinned kind and kubectl into <dir>/bin for CI's kind jobs. Linux
# amd64 only. Workflows run it through the kind action
# (.github/actions/kind), which keeps the release binaries in the Actions
# cache under --cache-key (docs/development.md, "Runners, images and
# runbooks").
#
# Usage:
#   ci/install-kind.sh <dir>             install; prints KIND_NODE_IMAGE=...
#   ci/install-kind.sh --cache-key       the cache key: the pins below
#   ci/install-kind.sh --verified <dir>  exits 0 when <dir>/bin holds the
#                                        release binaries, by SHA-256
#
# A binary already in <dir>/bin, restored from the cache, is kept when its
# SHA-256 is the pinned one; otherwise it is downloaded and checked, and a
# bad checksum fails. When the kind release download fails (GitHub answered
# 403 five times in run 45658613690), kind is built from its module at the
# same version with `go install` through Go's module proxy, which checks
# every module against Go's checksum database (sum.golang.org) instead of
# the release's SHA-256; that binary is not the release's, so the kind
# action does not cache it. kubectl comes only from dl.k8s.io: its main
# package lives in k8s.io/kubernetes, whose replace directives `go install`
# refuses, so the cache is its one shelter. KIND_URL overrides the kind
# release URL, to try the fallback.
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

KIND_URL=${KIND_URL:-https://github.com/kubernetes-sigs/kind/releases/download/$KIND_VERSION/kind-linux-amd64}
KUBECTL_URL=https://dl.k8s.io/release/$KUBECTL_VERSION/bin/linux/amd64/kubectl

# have <sha256> <file>: the file is there with that SHA-256.
have() {
  [ -f "$2" ] && echo "$1  $2" | sha256sum -c --status - 2>/dev/null
}

# download <url> <file> <sha256>: fails when the host refuses; exits when
# what it sent has another SHA-256. Nothing unchecked is left at <file>.
download() {
  rm -f "$2.part"
  if ! curl -fsSL --retry 4 --retry-all-errors -o "$2.part" "$1" >&2; then
    rm -f "$2.part"
    return 1
  fi
  if ! echo "$3  $2.part" | sha256sum -c --quiet - >&2; then
    rm -f "$2.part"
    echo "$1: not the pinned SHA-256 $3" >&2
    exit 1
  fi
  chmod 0755 "$2.part"
  mv "$2.part" "$2"
}

# go_program: the runner's go, or the newest one in its tool cache.
go_program() {
  if command -v go; then
    return
  fi
  local root
  root=$(env | sed -n 's/^GOROOT_[0-9_]*_X64=//p' | sort -V | sed -n '$p')
  [ -n "$root" ] && [ -x "$root/bin/go" ] && echo "$root/bin/go"
}

# build_kind <bin>: kind at KIND_VERSION from Go's module proxy, checked
# against Go's checksum database.
build_kind() {
  local go
  go=$(go_program) || { echo "no go program to build kind with" >&2; exit 1; }
  echo "building kind $KIND_VERSION with $("$go" version) from proxy.golang.org" >&2
  GOBIN=$1 GOPROXY=https://proxy.golang.org GOSUMDB=sum.golang.org \
    GONOSUMDB= GOPRIVATE= GOINSECURE= GOFLAGS= CGO_ENABLED=0 \
    "$go" install -trimpath "sigs.k8s.io/kind@$KIND_VERSION" >&2
  local reported
  reported=$("$1/kind" version)
  case " $reported " in
    *" $KIND_VERSION "*) ;;
    *) echo "built kind reports '$reported', not $KIND_VERSION" >&2; exit 1 ;;
  esac
}

case "${1:-}" in
  --cache-key)
    echo "kind-linux-amd64-$KIND_VERSION-$KIND_SHA256-kubectl-$KUBECTL_VERSION-$KUBECTL_SHA256"
    exit 0
    ;;
  --verified)
    dir=${2:?install directory}
    have "$KIND_SHA256" "$dir/bin/kind" && have "$KUBECTL_SHA256" "$dir/bin/kubectl"
    exit
    ;;
esac

dir=${1:?install directory}
mkdir -p "$dir/bin"

if ! have "$KIND_SHA256" "$dir/bin/kind"; then
  if ! download "$KIND_URL" "$dir/bin/kind" "$KIND_SHA256"; then
    echo "$KIND_URL refused; building kind from Go's module proxy instead" >&2
    [ -z "${GITHUB_ACTIONS:-}" ] ||
      echo "::warning title=install-kind.sh::the kind release download was refused; kind was built from Go's module proxy" >&2
    build_kind "$dir/bin"
  fi
fi
if ! have "$KUBECTL_SHA256" "$dir/bin/kubectl"; then
  download "$KUBECTL_URL" "$dir/bin/kubectl" "$KUBECTL_SHA256" ||
    { echo "$KUBECTL_URL refused" >&2; exit 1; }
fi

echo "KIND_NODE_IMAGE=$KIND_NODE_IMAGE"
