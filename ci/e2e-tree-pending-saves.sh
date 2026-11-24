#!/usr/bin/env bash
# Pending saves under failure (docs/trees.md, "Pending saves"), on a failpoints
# build with FUSE: a save worker's decision that fails, panics or kills agentfs
# (failpoint tree.close_decide) loses nothing. Each case runs on a fresh volume;
# ci/trees-pending-saves.py drives the client and checks the view, the metrics
# file and history.
set -euo pipefail
. "$(dirname "$0")/tree-git-test-lib.sh"
here=$(cd "$(dirname "$0")" && pwd)
bin=${AGENTFS_BIN:-$PWD/target/debug/agentfs}
base=${TREE_PENDING_DIR:-${TMPDIR:-/tmp}}
mkdir -p "$base"
work=$(mktemp -d "$base/agentfs-pending-saves.XXXXXX")
work=$(realpath "$work")
printf "artifacts: %s\n" "$work"
pid=
cleanup() {
  [ -z "$pid" ] || tree_stop "$pid"
  for m in "$work/main" "$work/fork"; do fusermount3 -uz "$m" 2>/dev/null || true; done
}
on_exit cleanup

# `start SPEC LOG [fork]` mounts the root, with the fork's view too when asked,
# its failpoints set to SPEC, and waits until every view is mounted.
start() {
  local spec=$1 log=$2 views=("$work/main")
  local args=("$work/main" --metrics-output "$work/metrics.json")
  if [ "${3:-}" = fork ]; then
    views+=("$work/fork")
    args+=(--fork-mount "$work/fork")
  fi
  rm -f "$work/metrics.json"
  env AGENTFS_FAILPOINTS="$spec" "$bin" --state "$work/state" mount "${args[@]}" >"$log" 2>&1 & pid=$!
  for _ in $(seq 1 200); do
    local up=1 view
    for view in "${views[@]}"; do mountpoint -q "$view" || up=; done
    [ -z "$up" ] || return 0
    kill -0 "$pid" || { cat "$log" >&2; return 1; }
    sleep .1
  done
  echo 'mount startup timed out' >&2; return 1
}

# `stop`: a clean unmount of every view, then the history's own check.
stop() {
  local m
  for m in "$work/main" "$work/fork"; do
    if mountpoint -q "$m"; then fusermount3 -u "$m"; fi
  done
  tree_wait_exit "$pid"; pid=
  git --git-dir="$work/state/history.git" fsck --strict
}

fired() {
  grep -F "agentfs failpoint tree.close_decide triggered ($1)" "$2" >/dev/null ||
    { cat "$2" >&2; echo "tree.close_decide=$1 did not fire" >&2; return 1; }
}

saves() {
  python3 "$here/trees-pending-saves.py" "$work/main" "$work/state/history.git" \
    "$work/metrics.json" "$@"
}

fresh() {
  rm -rf "$work/state" "$work/main" "$work/fork"
  mkdir "$work/main" "$work/fork"
  "$bin" --state "$work/state" init --volume t
}

# A failed decision: the save stays pending and holds its file for the fork's
# sync until the fork tree's next step saves it.
fresh
start tree.close_decide=error "$work/error.log" fork
fork=$(sed -n 's/^fork tree: //p' "$work/error.log")
[ -n "$fork" ] || { cat "$work/error.log" >&2; echo 'no fork tree in the log' >&2; exit 1; }
saves --case error --fork "$work/fork" --fork-ref "refs/heads/$fork"
fired error "$work/error.log"
grep -F "tree SaveFailed: tree=" "$work/error.log" >/dev/null || { cat "$work/error.log" >&2; exit 1; }
stop

# A panic in the decision: the worker goes on, the save at the next step.
fresh
start tree.close_decide=panic "$work/panic.log"
saves --case panic
fired panic "$work/panic.log"
grep -F "a save panicked; it stays pending" "$work/panic.log" >/dev/null || { cat "$work/panic.log" >&2; exit 1; }
stop

# agentfs killed in the decision, after close(2) returned: the restart's
# recovery saves it.
fresh
start tree.close_decide=kill "$work/kill.log"
saves --case kill
status=0; tree_wait_exit "$pid" 60 || status=$?; pid=
[ "$status" = 137 ] || { echo "agentfs exited with status $status, not the failpoint's SIGKILL" >&2; cat "$work/kill.log" >&2; exit 1; }
fired kill "$work/kill.log"
tree_unmount_dead "$work/main"
flock "$work/state/actor.lock" true
start '' "$work/restarted.log"
saves --case restarted
stop

# Two pending saves of one file, closed in the reverse of their opens' order
# while the first decision sleeps, made durable, then agentfs killed with
# SIGKILL: the restart lands them in close order, from the journal's close
# records, as the live volume would have.
fresh
start 'tree.close_decide=sleep(600000)' "$work/order.log"
saves --case order
tree_kill_mount "$pid" "$work/order.log" "$work/main"; pid=
flock "$work/state/actor.lock" true
start '' "$work/order-restarted.log"
saves --case order-restarted
stop
echo "pending saves: error, panic and kill each lose nothing; a restart keeps close order"
