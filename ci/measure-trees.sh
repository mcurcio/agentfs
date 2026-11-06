#!/usr/bin/env bash
# CI only: FUSE measurement preparation for git-model §18. No budget assertions.
# Usage: AGENTFS_BIN=/path/to/agentfs ci/measure-trees.sh calls|hour|storage|smoke /path/to/result.json
# Set MEASURE_TREES_TMPDIR to a directory on the filesystem under test.
set -euo pipefail
cd "$(dirname "$0")/.."
mode=${1:?calls, hour, storage, or smoke required}
out=${2:?output JSON path required}
case "$mode" in calls|hour|storage|smoke) ;; *) echo 'invalid mode' >&2; exit 2;; esac
out=$(realpath -m "$out")
bin=${AGENTFS_BIN:-$PWD/target/debug/agentfs}
work=$(mktemp -d "${MEASURE_TREES_TMPDIR:-${TMPDIR:-/tmp}}/agentfs-measure.XXXXXXXX")
pid=
cleanup() {
    status=$?
    for m in "$work/plain" "$work/main" "$work/sandbox"; do
        if mountpoint -q "$m"; then fusermount3 -u "$m" || fusermount3 -uz "$m" || true; fi
    done
    if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
    if [ "$status" != 0 ]; then cat "$work"/*.log 2>/dev/null || true; fi
    rm -rf "$work"
}
trap cleanup EXIT
mkdir "$work/main" "$work/sandbox" "$work/plain" "$work/backing"
args=(--output "$out" --mode "$mode" --bin "$bin" --state "$work/state")
notrun() { python3 ci/measure-trees.py "${args[@]}" --reason "$1"; }
for cmd in mountpoint fusermount3 findmnt git python3; do
    if ! command -v "$cmd" >/dev/null; then notrun "missing $cmd"; exit 0; fi
done
if [ ! -c /dev/fuse ] || [ ! -x "$bin" ]; then notrun 'missing /dev/fuse or executable agentfs binary'; exit 0; fi
if ! "$bin" --state "$work/state" trees init >"$work/init.log" 2>&1; then notrun 'tree init failed; see CI log'; cat "$work/init.log" >&2; exit 1; fi
"$bin" --state "$work/state" trees mount "$work/main" --sandbox "$work/sandbox" --metrics-output "$work/metrics.json" >"$work/mount.log" 2>&1 &
pid=$!
for _ in $(seq 1 200); do
    if mountpoint -q "$work/main" && mountpoint -q "$work/sandbox"; then break; fi
    if ! kill -0 "$pid" 2>/dev/null; then break; fi
    sleep 0.1
done
if ! mountpoint -q "$work/main" || ! mountpoint -q "$work/sandbox"; then notrun 'tree FUSE mount failed; see CI log'; cat "$work/mount.log" >&2; exit 1; fi
args+=(--main "$work/main" --sandbox "$work/sandbox" --metrics "$work/metrics.json")
# Opt in only when automatic sandbox-to-parent landing is available.
if [ "${MEASURE_TREES_LANDING:-0}" = 1 ]; then args+=(--landing); fi
if [[ "$mode" != hour ]] && command -v bindfs >/dev/null; then
    if bindfs -o direct_io "$work/backing" "$work/plain" >"$work/bindfs.log" 2>&1 &&
       [ "$(findmnt -n -o FSTYPE -T "$work/plain")" = fuse.bindfs ]; then
        args+=(--baseline "$work/plain")
    else
        cat "$work/bindfs.log" >&2
    fi
fi
python3 ci/measure-trees.py "${args[@]}"
