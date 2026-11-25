#!/usr/bin/env bash
# Builds the development image, agentfs-dev by default: the gate and the model's runs
# (ci/model-docker.sh) in a container. It starts from the Rust image the Dockerfile builds
# with, which is the release rust-toolchain.toml pins, and adds what the tests need.
# CONTRIBUTING.md, "A local Linux gate in a container".
#   ci/dev-image.sh [TAG]
# RUST_IMAGE overrides the base image (default: the Dockerfile's RUST_IMAGE) and DOCKER
# the docker program (docker; Podman takes the same commands).
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
tag=${1:-agentfs-dev}
base=${RUST_IMAGE:-$(sed -n 's/^ARG RUST_IMAGE=//p' "$root/Dockerfile")}
if [ -z "$base" ]; then
    echo "no ARG RUST_IMAGE line in $root/Dockerfile" >&2
    exit 2
fi
"${DOCKER:-docker}" build -t "$tag" --build-arg RUST_IMAGE="$base" - <<'DOCKERFILE'
ARG RUST_IMAGE
FROM ${RUST_IMAGE}
RUN apt-get update \
 && apt-get install -y --no-install-recommends libssl-dev pkg-config git-lfs \
 && rm -rf /var/lib/apt/lists/* \
 && rustup component add rustfmt clippy
# cargo-nextest, the gate's test runner (AGENTS.md, "Local gates"): its release, checked
# against the release's SHA-256.
RUN set -eu; version=0.9.152; arch=$(uname -m); \
    case "$arch" in \
      x86_64) sum=d039b43c3e0958441b6ad0a39c8884d018b3d2e8fb9928f209074b1100b5e17a ;; \
      aarch64) sum=ca1f81a5b86533eb45ab97e0064aa88e925e53aab9a698ca6331a7cad1f85f07 ;; \
      *) echo "no cargo-nextest checksum for $arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/nextest.tgz \
      "https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-$version/cargo-nextest-$version-$arch-unknown-linux-gnu.tar.gz"; \
    echo "$sum  /tmp/nextest.tgz" | sha256sum -c -; \
    tar xzf /tmp/nextest.tgz -C "$CARGO_HOME/bin"; \
    rm /tmp/nextest.tgz
DOCKERFILE
