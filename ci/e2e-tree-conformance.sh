#!/usr/bin/env bash
# Mounted subset of the validation in git-model.md. Needs Linux, git, python3 and /dev/fuse.
set -euo pipefail
. "$(dirname "$0")/tree-git-test-lib.sh"
bin=${AGENTFS_BIN:-$PWD/target/debug/agentfs}
work=$(mktemp -d)
pid=
cleanup() {
    status=$?
    for mount in "$work/main" "$work/fork"; do
        mountpoint -q "$mount" && fusermount3 -uz "$mount" || true
    done
    [ -z "$pid" ] || tree_stop "$pid"
    if [ "$status" != 0 ]; then cat "$work/mount.log" >&2 || true; fi
    rm -rf "$work"
}
on_exit cleanup
mkdir "$work/main" "$work/fork"
"$bin" --state "$work/state" init --volume t
"$bin" --state "$work/state" mount "$work/main" --fork-mount "$work/fork" --metrics-output "$work/metrics" >"$work/mount.log" 2>&1 &
pid=$!
for _ in $(seq 1 200); do
    if mountpoint -q "$work/main" && mountpoint -q "$work/fork"; then break; fi
    kill -0 "$pid"
    sleep 0.1
done
mountpoint -q "$work/main"
mountpoint -q "$work/fork"
fork=$(sed -n 's/^fork tree: //p' "$work/mount.log")
python3 ci/trees-conformance.py "$work/main" "$work/fork" "$work/state/history.git" "$fork" "$work/metrics" "${AGENTFS_MODEL_ORACLE:-$PWD/target/debug/phase1-oracle}"
fusermount3 -u "$work/main"
fusermount3 -u "$work/fork"
tree_wait_exit "$pid"
pid=
git --git-dir="$work/state/history.git" fsck --strict
