#!/usr/bin/env bash
# Reproduction for bead agentfs-4do: does restarting ONLY the agentfs
# container, while a workload container keeps running on the mount, apply
# queued patches under that live workload?
#
# Patches land only when no workload is attached (design §12, §19.1 "No
# live landing yet"). This test asserts that behaviour from the workload's
# side, with examples/docker/docker-run.sh (docs/docker.md):
#
#   - agentfs and a uid 10000 workload container on the mount; the
#     workload's cwd is in the mount and a process in it holds the target
#     file open;
#   - a patch is queued with `agentfs patch submit` (the volume has no
#     remote, so the producer pushes to the state area's git directory,
#     the volume's local inbox: docs/configuration.md agentfs.patchRemote);
#     `agentfs patch apply` under the daemon says it is attached;
#   - (1) `docker restart` of the agentfs container (SIGTERM, a clean stop
#     and a fresh mount), then (2) SIGKILL of its daemon from the host
#     (Docker restarts it over the stale mount), each with a new patch;
#   - after each, from the still-running workload: the target file must be
#     unchanged and the patch still queued.
#
# A patch that lands under the running workload fails the job (red = bug
# confirmed). Each variant's evidence is printed: the file as the
# workload sees it, the open descriptor, agentfs's log lines since the
# restart, `agentfs status`, and the target's last commit.
#
# Needs the agentfs image as $AGENTFS_IMAGE (default agentfs:ci), the fuse
# module loaded, and sudo (to SIGKILL the daemon from the host).

. "$(dirname "$0")/lib.sh"

EX=examples/docker
export AGENTFS_IMAGE=${AGENTFS_IMAGE:-agentfs:ci}
export VOLUME=ci-restart-patch MOUNT_DIR=${RUNNER_TEMP:-/tmp}/agentfs-restart-patch
WORKLOAD_IMAGE=debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f
AGENTFS=agentfs-$VOLUME
WL=workload-$VOLUME
WS=/agentfs/workspace
BRANCH=agentfs/$VOLUME
TARGET=target.txt

in_wl() { docker exec "$WL" sh -c "$1"; }
in_agentfs() { docker exec "$AGENTFS" "$@"; }
history() { in_agentfs git --git-dir /data/state/git "$@"; }

wait_content() { # path content [seconds]
  local deadline=$((SECONDS + ${3:-60}))
  until [ "$(history show "$BRANCH:$1" 2>/dev/null)" = "$2" ]; do
    [ $SECONDS -lt $deadline ] || fail "the history never had $1 = '$2'"
    sleep 1
  done
  pass "the history has $1 = '$2'"
}

wait_healthy() { # what started-at-before restart-count-before
  local deadline=$((SECONDS + 180)) now
  while :; do
    now=$(docker inspect -f '{{.State.StartedAt}} {{.RestartCount}} {{if .State.Health}}{{.State.Health.Status}}{{end}}' "$AGENTFS")
    case "$now" in
    "$2 "*) ;; # not restarted yet
    *healthy) break ;;
    esac
    [ $SECONDS -lt $deadline ] || fail "$1: agentfs never came back healthy: $now"
    sleep 1
  done
  pass "$1: agentfs restarted and is healthy ($now)"
}

# A process in the workload with its cwd in the mount and the target open
# on descriptor 3, told apart from earlier holders by <tag>. Prints its pid
# (in the workload's pid namespace).
hold_open() { # tag
  docker exec -d "$WL" sh -c "cd $WS && exec 3<$TARGET && exec sleep $1"
  local pid="" deadline=$((SECONDS + 30))
  until [ -n "$pid" ]; do
    pid=$(in_wl "for p in /proc/[0-9]*; do [ \"\$(tr '\\0' ' ' <\$p/cmdline 2>/dev/null)\" = 'sleep $1 ' ] && echo \${p#/proc/}; done; true" | head -n1)
    [ -n "$pid" ] || { [ $SECONDS -lt $deadline ] || fail "the holder never started"; sleep 0.5; }
  done
  echo "$pid"
}

queue_patch() { # content message
  in_agentfs sh -ec '
    rm -rf /tmp/producer && git init -q /tmp/producer && cd /tmp/producer
    git config user.name producer && git config user.email producer@example.com
    git remote add origin /data/state/git
    printf %s "$1" >/tmp/patch-content
    agentfs patch submit --volume "$2" --put "$3=/tmp/patch-content" -m "$4"
  ' sh "$1" "$VOLUME" "$TARGET" "$2"
}

VERDICTS=()

# Restarts agentfs with <how> (restart|kill) and reports what the running
# workload sees. <before> is the target's content before the patch,
# <patched> the patch's.
variant() { # name how before patched tag
  local name=$1 how=$2 before=$3 patched=$4 tag=$5 pid started restarts since out code seen n
  step "$name: queue a patch setting $TARGET to '$patched'"
  out=$(queue_patch "$patched" "$name: under a live workload" 2>&1)
  echo "$out"
  assert_contains "$out" "queued patch" "$name: the patch is queued"
  n=$(printf '%s\n' "$out" | sed -n 's/^queued patch \([0-9]*\) .*/\1/p' | head -n1)
  code=0
  out=$(in_agentfs agentfs patch apply --no-fetch 2>&1) || code=$?
  echo "$out"
  assert_eq 75 "$code" "$name: patch apply under the daemon exits 75"
  assert_contains "$out" "workload attached; lands at next mount" "$name: the daemon counts the workload as attached"

  step "$name: the workload is attached"
  pid=$(hold_open "$tag")
  in_wl "ls -l /proc/$pid/cwd /proc/$pid/fd/3"
  assert_eq "$before" "$(in_wl "cat $WS/$TARGET")" "$name: the workload reads '$before' before the restart"
  in_agentfs agentfs status | grep -E '^patches:' || true

  step "$name: restart ONLY the agentfs container ($how)"
  started=$(docker inspect -f '{{.State.StartedAt}}' "$AGENTFS")
  restarts=$(docker inspect -f '{{.RestartCount}}' "$AGENTFS")
  since=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  case "$how" in
  restart) docker restart "$AGENTFS" >/dev/null ;;
  kill) sudo kill -KILL "$(docker inspect -f '{{.State.Pid}}' "$AGENTFS")" ;;
  esac
  wait_healthy "$name" "$started" "$restarts"
  assert_eq true "$(docker inspect -f '{{.State.Running}}' "$WL")" "$name: the workload kept running"
  assert_eq 0 "$(docker inspect -f '{{.RestartCount}}' "$WL")" "$name: the workload was not restarted"

  step "$name: evidence, as seen from the workload"
  echo "---- agentfs log since the restart"
  docker logs --since "$since" "$AGENTFS" 2>&1 | sed 's/^/agentfs: /'
  echo "---- the workload's holder (pid $pid)"
  in_wl "ls -l /proc/$pid/cwd /proc/$pid/fd/3 2>&1; echo \"fd 3 reads: \$(cat /proc/$pid/fd/3 2>&1)\"; echo \"cwd lists: \$(ls /proc/$pid/cwd/ 2>&1)\"" || true
  seen=$(in_wl "cat $WS/$TARGET" 2>&1) || true
  echo "---- $WS/$TARGET as the workload reads it now: '$seen'"
  echo "---- agentfs status"
  in_agentfs agentfs status 2>&1 | grep -E '^(patches|volume|mount)' || in_agentfs agentfs status || true
  echo "---- the target's last commit"
  history log -1 --format='%h %s%n%(trailers)' "$BRANCH" -- "$TARGET" || true

  if [ "$seen" = "$patched" ]; then
    VERDICTS+=("$name: BUG: patch $n landed under the running workload ($WS/$TARGET '$before' -> '$patched')")
    printf '\033[31mBUG:\033[0m %s\n' "${VERDICTS[-1]}"
  elif [ "$seen" = "$before" ] && in_agentfs agentfs status 2>&1 | grep -qE '^patches: .*[1-9][0-9]* queued'; then
    pass "$name: patch $n did not land under the running workload; still queued"
  else
    VERDICTS+=("$name: UNEXPECTED: the workload reads '$seen' (before '$before', patch '$patched')")
    printf '\033[31mUNEXPECTED:\033[0m %s\n' "${VERDICTS[-1]}"
  fi
}

step "start agentfs and a workload container (docker-run.sh)"
docker version --format 'docker {{.Server.Version}}'
"$EX/docker-run.sh" up
"$EX/docker-run.sh" workload --name "$WL" --user 10000:10000 --cap-drop ALL \
  --security-opt no-new-privileges "$WORKLOAD_IMAGE" \
  sh -c "trap 'exit 0' TERM; while :; do sleep 1; done" >/dev/null
in_wl "printf original > $WS/$TARGET"
wait_content "$TARGET" original

variant "docker-restart" restart original patched-1 100001
# After variant 1 the target is whatever the workload reads now.
current=$(in_wl "cat $WS/$TARGET")
variant "sigkill" kill "$current" patched-2 100002

step "clean up"
"$EX/docker-run.sh" down
docker volume rm "agentfs-$VOLUME" >/dev/null

step "verdict"
if [ ${#VERDICTS[@]} -gt 0 ]; then
  fail "$(printf '%s; ' "${VERDICTS[@]}")"
fi
pass "no patch landed under the running workload"
