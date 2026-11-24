#!/usr/bin/env bash
# Creates a CI kind cluster on the pinned node image (ci/install-kind.sh's
# KIND_NODE_IMAGE) whose nodes pull Docker Hub through mirror.gcr.io
# (ci/docker-hub-mirror.sh). The runner's dockerd pulls the node image, so
# it must pull through the mirror already (.github/actions/docker-hub-mirror).
# Every kind cluster a workflow makes is made here.
#
# Usage: ci/kind-cluster.sh NAME CONFIG WAIT
#   e.g. ci/kind-cluster.sh agentfs-e2e ci/kind/cluster.yaml 120s

here=$(cd "$(dirname "$0")" && pwd)
. "$here/lib.sh"

name=${1:?cluster name}
config=${2:?kind config}
wait=${3:?wait, e.g. 120s}
: "${KIND_NODE_IMAGE:?run ci/install-kind.sh first}"

require_docker_hub_mirror
kind version
kind create cluster --name "$name" --config "$config" --image "$KIND_NODE_IMAGE" --wait "$wait"
"$here/docker-hub-mirror.sh" kind "$name"
