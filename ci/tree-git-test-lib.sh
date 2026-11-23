#!/usr/bin/env bash
# Read published tree contents by the commit's format marker, including legacy roots.

# A file's place in a commit or checkout, <aa>/<bb>/<name>: aa and bb are hex
# digits 1-2 and 3-4 of `git hash-object --stdin` of the name (docs/trees.md).
tree_git_fanned() {
  local hex
  hex=$(printf %s "$1" | git hash-object --stdin) || return 1
  printf '%s/%s/%s\n' "${hex:0:2}" "${hex:2:2}" "$1"
}

# A directory's record in a tree commit: dirs/<aa>/<bb>/<key>, key the path's
# whole blob id (docs/trees.md, "What a commit holds").
tree_git_dir_record() {
  local hex
  hex=$(printf %s "$1" | git hash-object --stdin) || return 1
  printf 'dirs/%s/%s/%s\n' "${hex:0:2}" "${hex:2:2}" "$hex"
}

tree_git_file() {
  local gitdir=$1 commit=$2 path=$3
  if git --git-dir="$gitdir" show -s --format=%B "$commit" | grep -Fx 'Agentfs-Format: envelope-2' >/dev/null; then
    git --git-dir="$gitdir" cat-file -e "$commit:lines/$(tree_git_fanned "$path")" || return 1
    path=files/$(tree_git_fanned "$path")
  fi
  git --git-dir="$gitdir" show "$commit:$path"
}

tree_git_no_duplicate_opens() {
  local duplicates
  duplicates=$(git --git-dir="$1" log root --format='%(trailers:key=Agentfs-Open,valueonly)' | sed '/^$/d' | sort | uniq -d) || return 1
  [ -z "$duplicates" ] || { echo "duplicate Agentfs-Open: $duplicates" >&2; return 1; }
}

# The tip of the root's Clients chain in Git (refs/agentfs/clients/root); empty
# before its first event is built.
tree_git_root_clients() {
  git --git-dir="$1" rev-parse -q --verify refs/agentfs/clients/root 2>/dev/null || true
}

# Waits until a mount's attach (or rebind) of the root's client is built: the
# root's Clients chain in Git has moved past `before`, its tip before the mount.
# A mount of the root alone records that one step, one batch for the builder,
# before it serves, so a failpoint counted past the mount's own hits then meets
# the next client call's.
tree_git_wait_root_attached() {
  local gitdir=$1 before=$2
  for _ in $(seq 1 300); do
    [ "$(tree_git_root_clients "$gitdir")" = "$before" ] || return 0
    sleep .1
  done
  echo 'the mount did not attach its root client' >&2; return 1
}

# tree_at_rest METRICS: waits until the mount writing METRICS (its
# `--metrics-output`) is at rest: a metrics file written after this call began
# shows history built through every record and no pointer left at a step. Each
# built event that leaves a pointer at a built step makes the mount append one
# switch record, which costs one journal fsync and one builder batch; at rest,
# no built event is left to append anything, so a failpoint counted from here
# meets only the client's calls.
tree_at_rest() {
  python3 - "$1" <<'PY'
import json, sys, time
def read():
    try:
        with open(sys.argv[1]) as f: return json.load(f)
    except (OSError, ValueError): return {}
# A file numbered two past the one seen now was written after this began.
floor = read().get('seq', 0) + 2
limit = time.monotonic() + 60
while time.monotonic() < limit:
    m = read()
    h = m.get('history', {})
    if m.get('seq', 0) >= floor and h['built_through'] >= h['appended'] and h['pointers_at_steps'] == 0:
        sys.exit(0)
    time.sleep(.05)
sys.exit(f'the mount did not come to rest: {read().get("history")}')
PY
}

# tree_wait_exit PID [SECONDS]: waits for PID, a child of this shell, to exit
# and returns its status. A process that never exits (a drain that never
# releases) is a failure, not a hang: past the bound (default 120 s) it is
# killed with SIGKILL, said on stderr, and this returns 124. The wait is on
# the exit itself (`wait PID`, which also answers for a child that exited and
# was reaped already); the bound only ends a hang, from its own process.
# Never `wait -n` here: bash forgets a job that ended before the call ("no
# such job") and waits out the bound, the 120 s stall after a `kill -KILL`.
tree_wait_exit() {
  local pid=$1 bound=${2:-120} timer status=0
  (
    trap 'kill "$s" 2>/dev/null; exit 0' TERM
    sleep "$bound" &
    s=$!
    wait "$s"
    echo "process $pid did not exit within ${bound}s: killed with SIGKILL" >&2
    kill -KILL "$pid" 2>/dev/null
  ) &
  timer=$!
  wait "$pid" || status=$?
  if kill "$timer" 2>/dev/null; then
    wait "$timer" 2>/dev/null || true
    return "$status"
  fi
  wait "$timer" 2>/dev/null || true
  return 124
}

# tree_kill_mount PID LOG MOUNTPOINT...: kills PID, an agentfs mount and a
# child of this shell, with SIGKILL, as a crash does, and unmounts what it
# left. Before the kill it fails loudly, printing LOG and /proc/self/mounts,
# unless PID is alive (not a zombie) and every MOUNTPOINT is still mounted: a
# mount that ended by itself is a product bug to find, not a step to skip.
# The exit must then be the kill's (137): one that takes the bound is a
# failure too. Then tree_unmount_dead.
tree_kill_mount() {
  local pid=$1 log=$2 state status=0 m
  shift 2
  state=$(ps -o stat= -p "$pid" 2>/dev/null || true)
  if [ -z "$state" ] || [[ $state == Z* ]]; then
    echo "agentfs $pid ended before its kill (state '$state')" >&2
    cat "$log" /proc/self/mounts >&2
    return 1
  fi
  for m in "$@"; do
    if ! mountpoint -q "$m"; then
      echo "$m was no longer mounted before agentfs $pid's kill" >&2
      cat "$log" /proc/self/mounts >&2
      return 1
    fi
  done
  kill -KILL "$pid"
  tree_wait_exit "$pid" 60 || status=$?
  if [ "$status" != 137 ]; then
    echo "agentfs $pid exited with status $status after SIGKILL, not 137" >&2
    cat "$log" >&2
    return 1
  fi
  tree_unmount_dead "$@"
}

# tree_unmount_dead MOUNTPOINT...: after a mount's agentfs died, unmounts each
# MOUNTPOINT (a canonical path: a dead mount cannot be resolved) while
# /proc/self/mounts lists it. One it no longer lists is said on stderr: a
# daemon's death leaves its mount (docs/trees.md), so an entry gone is
# evidence to keep for the open errata entry, not a failure of the case.
tree_unmount_dead() {
  local m
  for m in "$@"; do
    if awk -v m="$m" '$2 == m { found = 1 } END { exit !found }' /proc/self/mounts; then
      fusermount3 -uz "$m"
    else
      echo "warning: $m's mount entry was gone after its agentfs died" >&2
    fi
  done
}

# tree_stop PID: SIGTERM, then tree_wait_exit with a short bound: a cleanup
# trap never hangs on a mount whose drain does not finish.
tree_stop() {
  kill "$1" 2>/dev/null || true
  tree_wait_exit "$1" 30 2>/dev/null || true
}
