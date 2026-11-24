#!/usr/bin/env bash
# Docker Hub through Google's pull-through cache for every daemon CI pulls
# with (docs/development.md, "Runners, images and runbooks"): anonymous
# pulls from shared runners hit Docker Hub's rate limit. The images stay
# pinned by digest, so their content is the same, and each daemon falls
# back to Docker Hub when the mirror fails.
#
# The mirror's one home is `registry-mirrors` in ci/docker/daemon.json.
# The runner's dockerd merges that file, ci/hermes's docker-in-docker
# daemons mount it as their own daemon.json, and the commands below derive
# BuildKit's and kind's settings from it.
#
# Usage:
#   ci/docker-hub-mirror.sh daemon           the runner's dockerd: merges the file
#                                            into /etc/docker/daemon.json and
#                                            restarts dockerd (sudo)
#   ci/docker-hub-mirror.sh buildkitd FILE   writes a buildkitd.toml whose docker.io
#                                            pulls go through the mirror
#   ci/docker-hub-mirror.sh kind CLUSTER     every node's containerd: a hosts.toml
#                                            for docker.io under its certs.d
#   ci/docker-hub-mirror.sh check            fails unless the runner's dockerd
#                                            lists the mirror
#
# .github/actions/docker-hub-mirror runs `daemon` and `buildkitd`, and
# ci/kind-cluster.sh runs `kind`. Needs jq.

here=$(cd "$(dirname "$0")" && pwd)
. "$here/lib.sh"

SETTINGS=$here/docker/daemon.json
MIRROR=$(jq -er '."registry-mirrors"[0]' "$SETTINGS")

check() {
  local mirrors
  mirrors=$(docker info --format '{{json .RegistryConfig.Mirrors}}')
  echo "dockerd's registry mirrors: $mirrors"
  jq -e --arg m "$MIRROR" 'any(.[]?; rtrimstr("/") == $m)' >/dev/null <<<"$mirrors" ||
    fail "the runner's dockerd does not pull Docker Hub through $MIRROR: run .github/actions/docker-hub-mirror first"
}

case "${1:-}" in
daemon)
  current=/etc/docker/daemon.json
  merged=$(mktemp)
  if sudo test -s "$current"; then
    sudo cat "$current" | jq -s '.[0] * .[1]' - "$SETTINGS" >"$merged"
  else
    cp "$SETTINGS" "$merged"
  fi
  sudo install -m 0644 "$merged" "$current"
  rm -f "$merged"
  sudo cat "$current"
  sudo systemctl restart docker
  wait_for 60 docker info || fail "dockerd did not come back after its restart"
  check
  ;;
buildkitd)
  file=${2:?buildkitd.toml path}
  cat >"$file" <<EOF
[registry."docker.io"]
  mirrors = ["${MIRROR#https://}"]
EOF
  cat "$file"
  ;;
kind)
  cluster=${2:?kind cluster name}
  # ci/kind-cluster.sh points each node's containerd at
  # /etc/containerd/certs.d (config_path), which it reads on each pull, so
  # no restart is needed (kind's "Local Registry" guide). Docker Hub stays
  # the fallback. config_path may be a colon-separated list.
  dir=/etc/containerd/certs.d/docker.io
  hosts=$(printf 'server = "https://registry-1.docker.io"\n\n[host."%s"]\n  capabilities = ["pull", "resolve"]\n' "$MIRROR")
  nodes=$(kind get nodes --name "$cluster")
  [ -n "$nodes" ] || fail "kind cluster $cluster has no nodes"
  for node in $nodes; do
    paths=$(docker exec "$node" containerd config dump 2>/dev/null | grep -E '^ *config_path *=' || true)
    echo "$node's containerd: $paths"
    grep -E "config_path *= *[\"']([^\"']*:)?/etc/containerd/certs\.d(:[^\"']*)?[\"']" >/dev/null <<<"$paths" ||
      fail "$node's containerd does not read registry hosts from /etc/containerd/certs.d"
    docker exec "$node" mkdir -p "$dir"
    printf '%s\n' "$hosts" | docker exec -i "$node" cp /dev/stdin "$dir/hosts.toml"
    echo "$node: $dir/hosts.toml"
    docker exec "$node" cat "$dir/hosts.toml"
  done
  ;;
check) check ;;
*)
  echo "usage: $0 daemon | buildkitd FILE | kind CLUSTER | check" >&2
  exit 2
  ;;
esac
