#!/usr/bin/env bash
# Shared mappings on a real mount (ledger R1.4), on Linux CI only: the page-cache
# cases, the strict mapping script, then close-to-open on a mount of its own.
set -euo pipefail
. "$(dirname "$0")/tree-git-test-lib.sh"
bin=${AGENTFS_BIN:-$PWD/target/debug/agentfs}
work=$(mktemp -d)
pid=
cleanup() {
  status=$?
  fusermount3 -uz "$work/main" >/dev/null 2>&1 || true
  fusermount3 -uz "$work/fork" >/dev/null 2>&1 || true
  [ -z "$pid" ] || tree_stop "$pid"
  [ "$status" = 0 ] || cat "$work/mount.log" >&2 || true
  rm -rf "$work"
}
trap cleanup EXIT
wait_mounted() {
  for _ in $(seq 1 200); do
    if mountpoint -q "$1"; then return; fi
    kill -0 "$pid" || { cat "$work/mount.log" >&2; exit 1; }
    sleep .1
  done
  mountpoint -q "$1"
}
mkdir "$work/main" "$work/fork"
"$bin" --state "$work/state" init --volume t
"$bin" --state "$work/state" mount "$work/main" --fork-mount "$work/fork" >"$work/mount.log" 2>&1 & pid=$!
wait_mounted "$work/main"
wait_mounted "$work/fork"
result=0
python3 ci/trees-pagecache.py "$work/main" "$work/state/history.git" "$work/fork" || result=$?
python3 ci/trees-mmap.py "$work/main" "$work/state/history.git" || result=$?
fusermount3 -u "$work/main"
fusermount3 -u "$work/fork"
tree_wait_exit "$pid"; pid=
git --git-dir="$work/state/history.git" fsck --strict

# A fork's takes would hold the actor too: this runs against the saves alone.
"$bin" --state "$work/close-open" init --volume t
"$bin" --state "$work/close-open" mount "$work/main" >>"$work/mount.log" 2>&1 & pid=$!
wait_mounted "$work/main"
python3 ci/trees-close-open.py "$work/main" || result=$?
fusermount3 -u "$work/main"
tree_wait_exit "$pid"; pid=
exit "$result"
