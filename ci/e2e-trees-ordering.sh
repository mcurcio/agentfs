#!/usr/bin/env bash
# Strict close-to-open experiments for git-model §3 and §18. Linux CI only.
set -euo pipefail
bin=${AGENTFS_BIN:-$PWD/target/debug/agentfs}
work=$(mktemp -d)
pid=
cleanup() {
    status=$?
    mountpoint -q "$work/main" && fusermount3 -uz "$work/main" || true
    mountpoint -q "$work/sandbox" && fusermount3 -uz "$work/sandbox" || true
    [ -z "$pid" ] || kill "$pid" 2>/dev/null || true
    if [ "$status" != 0 ]; then cat "$work/mount.log" 2>/dev/null || true; fi
    rm -rf "$work"
}
trap cleanup EXIT
mkdir "$work/main" "$work/sandbox"
"$bin" --state "$work/state" trees init --volume t
"$bin" --state "$work/state" trees mount "$work/main" --sandbox "$work/sandbox" >"$work/mount.log" 2>&1 &
pid=$!
for _ in $(seq 1 200); do
    if mountpoint -q "$work/main" && mountpoint -q "$work/sandbox"; then break; fi
    kill -0 "$pid"
    sleep 0.1
done
mountpoint -q "$work/main"
mountpoint -q "$work/sandbox"
python3 ci/trees-ordering.py "$work/main"
