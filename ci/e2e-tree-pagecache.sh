#!/usr/bin/env bash
# Two-open object/page-cache observations, on Linux CI only.
set -euo pipefail
bin=${AGENTFS_BIN:-$PWD/target/debug/agentfs}
work=$(mktemp -d)
pid=
cleanup() {
  fusermount3 -uz "$work/main" >/dev/null 2>&1 || true
  [ -z "$pid" ] || { kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; }
  rm -rf "$work"
}
trap cleanup EXIT
mkdir "$work/main"
"$bin" --state "$work/state" trees init
"$bin" --state "$work/state" trees mount "$work/main" >"$work/mount.log" 2>&1 & pid=$!
for _ in $(seq 1 200); do
  mountpoint -q "$work/main" && break
  kill -0 "$pid" || { cat "$work/mount.log" >&2; exit 1; }
  sleep .1
done
mountpoint -q "$work/main"
python3 ci/trees-pagecache.py "$work/main" "$work/state/history.git"
fusermount3 -u "$work/main"
wait "$pid"; pid=
