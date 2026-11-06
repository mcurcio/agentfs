#!/usr/bin/env bash
# Shared mappings on a real mount (ledger R1.4), on Linux CI only: the page-cache
# cases, the strict mapping script, then close-to-open on a mount of its own.
set -euo pipefail
bin=${AGENTFS_BIN:-$PWD/target/debug/agentfs}
work=$(mktemp -d)
pid=
cleanup() {
  status=$?
  fusermount3 -uz "$work/main" >/dev/null 2>&1 || true
  fusermount3 -uz "$work/sandbox" >/dev/null 2>&1 || true
  [ -z "$pid" ] || { kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; }
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
mkdir "$work/main" "$work/sandbox"
"$bin" --state "$work/state" trees init
fork=$("$bin" --state "$work/state" trees fork)
"$bin" --state "$work/state" trees mount "$work/main" --sandbox "$work/sandbox" --fork "$fork" >"$work/mount.log" 2>&1 & pid=$!
wait_mounted "$work/main"
wait_mounted "$work/sandbox"
result=0
fusermount3 -u "$work/main"
fusermount3 -u "$work/sandbox"
wait "$pid"; pid=
git --git-dir="$work/state/history.git" fsck --strict

# A sandbox's takes would hold the actor too: this runs against the saves alone.
"$bin" --state "$work/close-open" trees init
"$bin" --state "$work/close-open" trees mount "$work/main" >>"$work/mount.log" 2>&1 & pid=$!
wait_mounted "$work/main"
uname -a
cat /proc/sys/fs/fuse/max_user_bgreq
python3 ci/trees-close-open.py "$work/main" || result=$?
grep -a REQ "$work/mount.log" | head -1500
fusermount3 -u "$work/main"
wait "$pid"; pid=
exit "$result"
