#!/usr/bin/env bash
# Mounted subset of the validation in git-model.md. Needs Linux, git, python3 and /dev/fuse.
set -euo pipefail
bin=${AGENTFS_BIN:-$PWD/target/debug/agentfs}
work=$(mktemp -d)
pid=
cleanup() {
    status=$?
    for mount in "$work/main" "$work/sandbox"; do
        mountpoint -q "$mount" && fusermount3 -uz "$mount" || true
    done
    [ -z "$pid" ] || kill "$pid" 2>/dev/null || true
    [ -z "$pid" ] || wait "$pid" 2>/dev/null || true
    if [ "$status" != 0 ]; then cat "$work/mount.log" >&2 || true; fi
    rm -rf "$work"
}
trap cleanup EXIT
mkdir "$work/main" "$work/sandbox"
"$bin" --state "$work/state" trees init --volume t
fork=$("$bin" --state "$work/state" trees fork)
"$bin" --state "$work/state" trees mount "$work/main" --sandbox "$work/sandbox" --fork "$fork" --metrics-output "$work/metrics" >"$work/mount.log" 2>&1 &
pid=$!
for _ in $(seq 1 200); do
    if mountpoint -q "$work/main" && mountpoint -q "$work/sandbox"; then break; fi
    kill -0 "$pid"
    sleep 0.1
done
mountpoint -q "$work/main"
mountpoint -q "$work/sandbox"
python3 ci/trees-conformance.py "$work/main" "$work/sandbox" "$work/state/history.git" "$fork" "$work/metrics" "${AGENTFS_MODEL_ORACLE:-$PWD/target/debug/phase1-oracle}"
fusermount3 -u "$work/main"
fusermount3 -u "$work/sandbox"
wait "$pid"
pid=
git --git-dir="$work/state/history.git" fsck --strict
