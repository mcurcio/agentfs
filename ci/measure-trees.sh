#!/usr/bin/env bash
# CI only: FUSE measurement preparation for git-model §18. No budget assertions.
# Usage: AGENTFS_BIN=/path/to/agentfs ci/measure-trees.sh MODE /path/to/result.json
# MODE: calls, hour, storage (10,000 saves), pilot (1,000 saves), held-log, smoke,
# or smoke-kill (kills the harness mid-run and checks the report it leaves).
# The report is <result>, with samples in <result>.samples.jsonl, from the start.
# Set MEASURE_TREES_TMPDIR to a directory on the filesystem under test.
set -euo pipefail
cd "$(dirname "$0")/.."
mode=${1:?a mode is required}
out=${2:?output JSON path required}
case "$mode" in calls|hour|storage|pilot|held-log|smoke|smoke-kill) ;; *) echo 'invalid mode' >&2; exit 2;; esac
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
    if [ "$status" != 0 ]; then cat "$work"/*.log "${mount_log:-/dev/null}" 2>/dev/null || true; fi
    rm -rf "$work"
}
trap cleanup EXIT
mkdir "$work/main" "$work/sandbox" "$work/plain" "$work/backing"
harness_mode=$mode
[ "$mode" = smoke-kill ] && harness_mode=pilot
args=(--output "$out" --mode "$harness_mode" --bin "$bin" --state "$work/state")
notrun() { python3 ci/measure-trees.py "${args[@]}" --reason "$1"; }
for cmd in mountpoint fusermount3 findmnt git python3; do
    if ! command -v "$cmd" >/dev/null; then notrun "missing $cmd"; exit 0; fi
done
if [ ! -c /dev/fuse ] || [ ! -x "$bin" ]; then notrun 'missing /dev/fuse or executable agentfs binary'; exit 0; fi
if ! "$bin" --state "$work/state" trees init --volume t >"$work/init.log" 2>&1; then notrun 'tree init failed; see CI log'; cat "$work/init.log" >&2; exit 1; fi
# The mount's log sits beside the report, so a run that is cut still has it.
mount_log="$out.mount.log"
"$bin" --state "$work/state" trees mount "$work/main" --fork-mount "$work/sandbox" --metrics-output "$work/metrics.json" >"$mount_log" 2>&1 &
pid=$!
for _ in $(seq 1 200); do
    if mountpoint -q "$work/main" && mountpoint -q "$work/sandbox"; then break; fi
    if ! kill -0 "$pid" 2>/dev/null; then break; fi
    sleep 0.1
done
if ! mountpoint -q "$work/main" || ! mountpoint -q "$work/sandbox"; then notrun 'tree FUSE mount failed; see CI log'; cat "$mount_log" >&2; exit 1; fi
args+=(--mount-pid "$pid" --main "$work/main" --sandbox "$work/sandbox" --metrics "$work/metrics.json")
# Opt in only when automatic sandbox-to-parent landing is available.
if [ "${MEASURE_TREES_LANDING:-0}" = 1 ]; then args+=(--landing); fi
# A backlog of saves takes minutes to drain: the harness waits for events, and
# the step's own time limit ends a run that does not finish.
case "$mode" in calls|storage|pilot|held-log|smoke-kill) args+=(--visibility-timeout 3600);; esac
if [[ "$mode" = calls || "$mode" = smoke ]] && command -v bindfs >/dev/null; then
    # A FUSE mount, whatever subtype this libfuse reports for bindfs.
    if bindfs -o direct_io "$work/backing" "$work/plain" >"$work/bindfs.log" 2>&1 &&
       [[ "$(findmnt -n -o FSTYPE -T "$work/plain")" == fuse* ]]; then
        args+=(--baseline "$work/plain")
    else
        echo "plain-FUSE baseline unavailable: bindfs log and mount table follow" >&2
        cat "$work/bindfs.log" >&2
        findmnt -T "$work/plain" >&2 || true
    fi
fi
if [ "$mode" != smoke-kill ]; then
    python3 ci/measure-trees.py "${args[@]}"
    exit
fi
# Kill the harness once it has sampled, then read what it left: a report that
# parses and says RUNNING, and every sample line up to the kill.
samples="$out.samples.jsonl"
python3 ci/measure-trees.py "${args[@]}" --sample-every 10 &
harness=$!
until [ -f "$samples" ] && [ "$(wc -l <"$samples")" -ge 3 ]; do
    if ! kill -0 "$harness" 2>/dev/null; then echo 'harness ended before it sampled' >&2; exit 1; fi
    sleep 0.1
done
kill -KILL "$harness"
wait "$harness" 2>/dev/null || true
python3 -I - "$out" "$samples" <<'PY'
import json, sys
report = json.load(open(sys.argv[1]))
rows = [json.loads(line) for line in open(sys.argv[2])]
assert report['status'] == 'RUNNING', report['status']
assert len(rows) >= 3 and all('saves' in r and 'metrics' in r for r in rows), rows
print('smoke-kill: RUNNING report parses; %d samples, last at %d saves' % (len(rows), rows[-1]['saves']))
PY
