#!/usr/bin/env bash
# End-to-end test of the Docker examples (examples/docker, docs/docker.md)
# on a Linux host with docker and docker compose. One example per run:
#
#   ci/e2e-docker.sh run             docker-run.sh, no remote
#   ci/e2e-docker.sh compose         compose.yaml, no remote; also a crash
#   ci/e2e-docker.sh compose-remote  compose.yaml + compose.remote.yaml
#   ci/e2e-docker.sh diagnose <example>
#
# Each checks, from real containers:
#   - a root and a uid 10000 workload container write through the mount
#     with normal permissions (design §18 test 4), and the bare directory
#     under the mount is read-only to them (the fence);
#   - nothing of the state area is visible to them: /agentfs holds only
#     the mount, the tree only their files, /data and the Docker volume
#     are not mounted (test 5);
#   - the writes are committed to agentfs/<volume> with the writers' uids
#     as authors, and for compose-remote reach the example git server;
#   - `agentfs status` runs in the agentfs container (docker exec);
#   - stopping makes the final commit (and, with the remote, pushes it),
#     and leaves nothing mounted on the host.
# `compose` also kills the daemon with SIGKILL from the host: Docker
# restarts the container, which mounts over the stale mount, and the
# running workload reaches the new mount with no restart of its own
# (design §17.1, §18 test 2).
#
# Needs the agentfs image as $AGENTFS_IMAGE (default agentfs:ci), the fuse
# module loaded, and sudo (to SIGKILL the daemon from the host).

. "$(dirname "$0")/lib.sh"

EX=examples/docker
export AGENTFS_IMAGE=${AGENTFS_IMAGE:-agentfs:ci}
WORKLOAD_IMAGE=debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f
EXAMPLE=${1:-}
WS=/agentfs/workspace

case "$EXAMPLE" in
run)
  export VOLUME=ci-docker-run MOUNT_DIR=${RUNNER_TEMP:-/tmp}/agentfs-docker-run
  AGENTFS=agentfs-$VOLUME
  in_root() { docker exec writer-root sh -c "$1"; }
  in_user() { docker exec writer-10000 sh -c "$1"; }
  in_agentfs() { docker exec "$AGENTFS" "$@"; }
  history() { docker exec "$AGENTFS" git --git-dir /data/state/git "$@"; }
  stopped_history() {
    docker run --rm -v "agentfs-$VOLUME:/data" --entrypoint git "$AGENTFS_IMAGE" \
      --git-dir /data/state/git "$@"
  }
  agentfs_logs() { docker logs "$AGENTFS" 2>&1; }
  ;;
compose | compose-remote)
  export VOLUME=ci-$EXAMPLE
  files=(-f "$EX/compose.yaml")
  [ "$EXAMPLE" = compose ] || files+=(-f "$EX/compose.remote.yaml")
  files+=(-f ci/docker/compose.ci.yaml)
  dc() { docker compose "${files[@]}" "$@"; }
  in_root() { dc exec -T writer-root sh -c "$1"; }
  in_user() { dc exec -T workload sh -c "$1"; }
  in_agentfs() { dc exec -T agentfs "$@"; }
  history() { dc exec -T agentfs git --git-dir /data/state/git "$@"; }
  stopped_history() {
    dc run --rm --no-deps -T --entrypoint git agentfs-init --git-dir /data/state/git "$@"
  }
  rgit() { dc exec -T -u www-data git-server git --git-dir /srv/git/history.git "$@"; }
  agentfs_logs() { dc logs --no-color --no-log-prefix agentfs; }
  ;;
diagnose) ;;
*) echo "usage: $0 run|compose|compose-remote|diagnose <example>" >&2; exit 2 ;;
esac

if [ "$EXAMPLE" = diagnose ]; then
  set +e
  step "diagnose"
  docker version
  docker compose version
  docker ps -a
  for c in $(docker ps -aq); do
    echo "---- $(docker inspect -f '{{.Name}} restarts={{.RestartCount}} health={{if .State.Health}}{{.State.Health.Status}}{{end}}' "$c")"
    docker logs --tail 80 "$c" 2>&1
  done
  echo "---- host mounts"
  grep -E 'fuse|agentfs' /proc/self/mountinfo
  exit 0
fi

BRANCH=agentfs/$VOLUME

# Waits until `history`-like command $1 shows <path> on the branch with
# <content>.
wait_content() { # git-fn path content [seconds]
  local deadline=$((SECONDS + ${4:-60}))
  until [ "$("$1" show "$BRANCH:$2" 2>/dev/null)" = "$3" ]; do
    [ $SECONDS -lt $deadline ] || fail "$1 never had $2 = '$3'"
    sleep 1
  done
  pass "$1 has $2"
}
author() { "$1" log -1 --format=%an "$BRANCH" -- "$2"; } # git-fn path

step "start the $EXAMPLE example"
docker version --format 'docker {{.Server.Version}}'
docker compose version
case "$EXAMPLE" in
run)
  "$EX/docker-run.sh" up
  idle=(sh -c "trap 'exit 0' TERM; while :; do sleep 1; done")
  "$EX/docker-run.sh" workload --name writer-root --user 0:0 "$WORKLOAD_IMAGE" "${idle[@]}" >/dev/null
  "$EX/docker-run.sh" workload --name writer-10000 --user 10000:10000 --cap-drop ALL \
    --security-opt no-new-privileges "$WORKLOAD_IMAGE" "${idle[@]}" >/dev/null
  ;;
*)
  dc up -d --build
  ;;
esac
assert_contains "$(agentfs_logs)" "mounted " "agentfs mounted the volume"
assert_contains "$(agentfs_logs)" "volume $VOLUME" "the mount is volume $VOLUME"

step "root and uid 10000 write with normal permissions (design §18 test 4)"
in_root "printf root > $WS/root.txt && chmod 600 $WS/root.txt"
in_user "printf user > $WS/user.txt"
assert_eq 10000 "$(in_root "stat -c %u $WS/user.txt")" "uid 10000's file is its own"
assert_eq "0 600" "$(in_root "stat -c '%u %a' $WS/root.txt")" "root's file is root's, mode 600"
if out=$(in_user "printf x >> $WS/root.txt" 2>&1); then
  fail "uid 10000 wrote root's 0600 file"
fi
assert_contains "$out" "Permission denied" "uid 10000 cannot write root's 0600 file"
assert_eq user "$(in_root "cat $WS/user.txt")" "root reads uid 10000's file"
if out=$(in_root 'touch /agentfs/outside' 2>&1); then
  fail "root wrote to the bare directory beside the mount"
fi
assert_contains "$out" "Read-only file system" "the directory under the mount is read-only (the fence)"

step "the state area is invisible to the workload (design §18 test 5)"
for writer in in_root in_user; do
  assert_eq workspace "$("$writer" 'ls -A /agentfs')" "$writer: /agentfs holds only the mount"
  assert_eq "root.txt user.txt" "$("$writer" "ls -A $WS" | tr '\n' ' ' | sed 's/ $//')" \
    "$writer: the tree holds only the writers' files"
  if "$writer" 'test -e /data'; then fail "$writer can reach /data"; fi
  assert_eq 0 "$("$writer" 'grep -c -e /var/lib/docker/volumes -e /data/state /proc/self/mountinfo || true')" \
    "$writer: no Docker volume or state area is mounted"
done

step "the writes are committed with the writers as authors"
wait_content history root.txt root
wait_content history user.txt user
assert_eq uid-0 "$(author history root.txt)" "root.txt authored by uid-0"
assert_eq uid-10000 "$(author history user.txt)" "user.txt authored by uid-10000"
if [ "$EXAMPLE" = compose-remote ]; then
  wait_content rgit root.txt root
  wait_content rgit user.txt user
  assert_eq uid-10000 "$(author rgit user.txt)" "the remote's user.txt authored by uid-10000"
fi
in_agentfs agentfs status
pass "agentfs status runs in the agentfs container"

if [ "$EXAMPLE" = compose ]; then
  step "SIGKILL the daemon: Docker restarts it over the stale mount (design §18 test 2)"
  in_user "printf before > $WS/before-crash.txt"
  wait_content history before-crash.txt before
  cid=$(dc ps -q agentfs)
  restarts=$(docker inspect -f '{{.RestartCount}}' "$cid")
  pid=$(docker inspect -f '{{.State.Pid}}' "$cid")
  # PID 1 ignores SIGKILL from inside its own namespace: kill from the host.
  sudo kill -KILL "$pid"
  deadline=$((SECONDS + 180))
  until [ "$(docker inspect -f '{{.RestartCount}} {{.State.Health.Status}}' "$cid")" = "$((restarts + 1)) healthy" ]; do
    [ $SECONDS -lt $deadline ] || fail "Docker never restarted agentfs to healthy: $(docker inspect -f '{{.RestartCount}} {{.State.Health.Status}}' "$cid")"
    sleep 1
  done
  pass "Docker restarted agentfs and it is healthy again"
  assert_contains "$(agentfs_logs)" "stacked over a stale mount" "the restarted daemon mounted over the stale mount"
  user_cid=$(dc ps -q workload)
  in_user "printf after > $WS/after-crash.txt"
  assert_eq true "$(docker inspect -f '{{.State.Running}}' "$user_cid")" "the workload kept running"
  assert_eq 0 "$(docker inspect -f '{{.RestartCount}}' "$user_cid")" "the workload was not restarted"
  wait_content history after-crash.txt after
  wait_content history before-crash.txt before 5
fi

step "stopping makes the final commit"
in_user "printf final > $WS/final.txt"
case "$EXAMPLE" in
run)
  out=$("$EX/docker-run.sh" down)
  echo "$out"
  assert_contains "$out" "final commit:" "docker-run.sh down shows the final commit"
  ;;
*)
  # As `docker compose down` does: the workload first, then agentfs.
  dc stop writer-root workload
  dc stop agentfs
  logs=$(agentfs_logs)
  assert_contains "$logs" "final commit:" "agentfs made its final commit"
  [ "$EXAMPLE" = compose ] || assert_contains "$logs" "final push: origin" "agentfs pushed at shutdown"
  ;;
esac
wait_content stopped_history final.txt final 5
if [ "$EXAMPLE" = compose-remote ]; then
  wait_content rgit final.txt final 5
fi
if grep -E " $(realpath -m "${MOUNT_DIR:-$EX/mnt}")/workspace " /proc/self/mountinfo; then
  fail "the mount is still on the host after stopping"
fi
pass "nothing is left mounted on the host"

step "clean up"
case "$EXAMPLE" in
run) docker volume rm "agentfs-$VOLUME" >/dev/null ;;
*) dc down -v ;;
esac
pass "the $EXAMPLE example passed"
