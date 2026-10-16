#!/usr/bin/env bash
# agentfs as a Docker volume with plain `docker run` (docs/docker.md).
#
#   examples/docker/docker-run.sh up
#   examples/docker/docker-run.sh workload [docker run options] IMAGE [COMMAND...]
#   examples/docker/docker-run.sh down
#
# `up` initialises the volume and starts the agentfs container, which
# FUSE-mounts it at $MOUNT_DIR/workspace on the host (an `rshared` bind of
# $MOUNT_DIR), and waits until the mount is live. `workload` starts a
# container that sees that directory at /agentfs (an `rslave`, read-only
# bind) and works in /agentfs/workspace. `down` stops the workload
# containers, then agentfs, which makes its final commit and unmounts.
#
# The volume's files and its history live in the Docker volume
# agentfs-$VOLUME, mounted only into the agentfs containers: the
# workload never sees the history.
#
# Settings, all optional:
#   VOLUME          the volume's name and its branch agentfs/<name> (example)
#   MOUNT_DIR       the host directory the mount appears in; it must be on
#                   a shared mount, as / is on systemd hosts
#                   (<this directory>/mnt-run)
#   AGENTFS_IMAGE   the agentfs image (ghcr.io/mcurcio/agentfs:latest; pin
#                   a commit tag or a digest in real use)
#   AGENTFS_CONFIG  a git-config fragment with settings and remotes
#                   (<this directory>/config/local: no remote)
#   AGENTFS_SECRETS a directory of credential files, mounted at
#                   /run/secrets in the agentfs container (none)
#   WORKLOAD_USER   the uid:gid the tree's root is given to, so a non-root
#                   workload can write at the top of the volume (10000:10000)

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
VOLUME=${VOLUME:-example}
MOUNT_DIR=${MOUNT_DIR:-$here/mnt-run}
AGENTFS_IMAGE=${AGENTFS_IMAGE:-ghcr.io/mcurcio/agentfs:latest}
AGENTFS_CONFIG=${AGENTFS_CONFIG:-$here/config/local}
WORKLOAD_USER=${WORKLOAD_USER:-10000:10000}

DATA=agentfs-$VOLUME          # the Docker volume: backing/ and state/
AGENTFS=agentfs-$VOLUME       # the agentfs container
LABEL=io.github.mcurcio.agentfs.workload=$VOLUME

up() {
  mkdir -p "$MOUNT_DIR"
  docker volume create "$DATA" >/dev/null

  # Creates the state area and /data/backing on the first run, checks
  # them on every later one (another volume's state area fails here), and
  # names the config fragment as the history's include.path.
  docker run --rm \
    -e AGENTFS_VOLUME="$VOLUME" -e AGENTFS_STATE=/data/state -e AGENTFS_ROOT=/data/backing \
    -e WORKLOAD_USER="$WORKLOAD_USER" \
    -v "$DATA:/data" \
    -v "$AGENTFS_CONFIG:/etc/agentfs/config:ro" \
    --entrypoint sh "$AGENTFS_IMAGE" \
    -c 'agentfs init --include /etc/agentfs/config && chown "$WORKLOAD_USER" /data/backing'

  local secrets=()
  [ -z "${AGENTFS_SECRETS:-}" ] || secrets=(-v "$AGENTFS_SECRETS:/run/secrets:ro")

  # The recorder. FUSE needs /dev/fuse and CAP_SYS_ADMIN, and Docker's
  # AppArmor profile denies mount(2), so it runs unconfined (--privileged
  # covers all three, and more). Its mount propagates to the host through
  # the rshared bind; restarted after a crash, it mounts over the stale
  # mount (AGENTFS_ON_STALE_MOUNT=recover) and the new mount propagates
  # the same way.
  docker run -d --name "$AGENTFS" --hostname "$AGENTFS" \
    --restart unless-stopped --stop-timeout 60 \
    --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
    -e AGENTFS_STATE=/data/state -e AGENTFS_ROOT=/data/backing \
    -e AGENTFS_ON_STALE_MOUNT=recover -e AGENTFS_SEAL_MOUNTPOINT=true \
    -e OTEL_SDK_DISABLED=true \
    -v "$DATA:/data" \
    -v "$AGENTFS_CONFIG:/etc/agentfs/config:ro" \
    "${secrets[@]}" \
    --mount "type=bind,src=$MOUNT_DIR,dst=/agentfs,bind-propagation=rshared" \
    --health-cmd 'agentfs probe /agentfs/workspace' \
    --health-interval 10s --health-start-period 300s --health-start-interval 1s \
    "$AGENTFS_IMAGE" mount /data/backing /agentfs/workspace >/dev/null

  local deadline=$((SECONDS + 300)) status=""
  until [ "$status" = healthy ]; do
    if [ $SECONDS -ge $deadline ] || [ "$(docker inspect -f '{{.State.Running}}' "$AGENTFS")" != true ]; then
      docker logs "$AGENTFS" >&2
      echo "agentfs did not mount $MOUNT_DIR/workspace" >&2
      exit 1
    fi
    sleep 1
    status=$(docker inspect -f '{{.State.Health.Status}}' "$AGENTFS")
  done
  echo "mounted volume $VOLUME at $MOUNT_DIR/workspace"
}

# A container on the volume. The directory is bound read-only and
# rslave: the FUSE mount inside it (and a mount that replaces it after a
# crash) propagates in and stays writable (bind-recursive=writable),
# while the bare directory under it is read-only, so nothing is written
# where nothing records it while no daemon is mounted.
workload() {
  docker run -d --label "$LABEL" -w /agentfs/workspace \
    --mount "type=bind,src=$MOUNT_DIR,dst=/agentfs,readonly,bind-propagation=rslave,bind-recursive=writable" \
    "$@"
}

down() {
  # The workload first, so nothing holds the mount when agentfs unmounts.
  local ids
  ids=$(docker ps -aq --filter "label=$LABEL")
  if [ -n "$ids" ]; then
    # shellcheck disable=SC2086
    docker stop $ids >/dev/null && docker rm $ids >/dev/null
  fi
  if docker inspect "$AGENTFS" >/dev/null 2>&1; then
    docker stop "$AGENTFS" >/dev/null
    docker logs "$AGENTFS" 2>&1 | grep -E 'final (commit|push)' || true
    docker rm "$AGENTFS" >/dev/null
  fi
  echo "stopped volume $VOLUME; its history stays in the Docker volume $DATA"
}

case "${1:-}" in
up) up ;;
workload) shift; workload "$@" ;;
down) down ;;
*) echo "usage: $0 up | workload [docker run options] IMAGE [COMMAND...] | down" >&2; exit 2 ;;
esac
