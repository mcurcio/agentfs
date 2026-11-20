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
DOCKERFILE
