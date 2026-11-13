#!/usr/bin/env bash
# The one entry point for every run of the model in a container: its tests, the digest
# (ci/model-digest.sh) and the planted-bug corpus (planted/run.py). VERIFYING.md,
# "Refactoring without changing outcomes"; the repo skill model-runs.
#   ci/model-docker.sh [--stall SECONDS] [--] COMMAND...
# It always runs the container with --init (without it, orphaned git processes pile up as
# zombies until nothing can start), --pids-limit 4096 (a process leak fails within seconds
# instead of freezing Docker), TMPDIR=/tmp (the container's own disk: on a Docker Desktop
# bind mount 8 seeds took 320 s against 24 s) and named volumes for the cargo target
# (/target), the work folder (/work) and the cargo registry. The repository is mounted
# at /src. It is also the alarm: the job runs in the foreground, and the wrapper kills it
# and exits 3, naming the unit in flight, when no unit finishes (a `unit-done:` line; before
# the first `unit-start:`, any line) within the stall limit: --stall, or twice the slowest
# unit of the job's kind in crates/agentfs-model/planted/budget.json (1800 s without one). A job that hits the pid limit fails with
# exit 4. A job that writes the budget (`run.py --write`, `ci/model-digest.sh record`) is
# refused (exit 5) while another container of the image runs, since its times would be
# inflated; MODEL_BUDGET_CONTENDED=1 writes them anyway with a note saying so. MODEL_IMAGE (agentfs-dev), MODEL_DOCKER (docker) and MODEL_TARGET_VOLUME,
# MODEL_WORK_VOLUME, MODEL_CARGO_VOLUME (agentfs-model-target, -work, -cargo) override the
# image, the docker program and the volumes.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
budget=$root/crates/agentfs-model/planted/budget.json
docker=${MODEL_DOCKER:-docker}
image=${MODEL_IMAGE:-agentfs-dev}
stall=""
while [ $# -gt 0 ]; do
    case $1 in
        --stall) stall=${2:?seconds}; shift 2 ;;
        --) shift; break ;;
        *) break ;;
    esac
done
if [ $# -eq 0 ]; then
    echo "usage: $0 [--stall SECONDS] [--] COMMAND..." >&2
    exit 2
fi
# The job's own kind of unit: a bug of the corpus (fast or --full), or a seed or the scenarios
# of the digest. Without a measured unit of that kind (a first run, or any other command),
# 1800 s: the corpus's and the digest's own limit for one unit.
if [ -z "$stall" ]; then
    stall=$(python3 -c 'import json, sys
b = json.load(open(sys.argv[1]))
args = sys.argv[2:]
if any(a.endswith("run.py") for a in args):
    key = "corpus_full" if "--full" in args else "corpus_sweep" if "--sweep" in args else "corpus"
    units = [b.get(key, {}).get("slowest_bug_seconds", 0)]
elif any(a.endswith("model-digest.sh") for a in args):
    d = b.get("digest", {})
    units = [d.get("scenarios_seconds", 0), d.get("slowest_seed_seconds", 0)]
else:
    units = []
print(2 * max(units) if units and max(units) > 0 else 1800)' "$budget" "$@" 2>/dev/null || echo 1800)
fi

# Budget times are measured alone: another container of the image would inflate them.
note=""
case " $* " in
    *" --write "* | *" record "*)
        others=$("$docker" ps --filter "ancestor=$image" --format '{{.ID}} {{.Command}}' | paste -sd';' -)
        if [ -n "$others" ]; then
            if [ "${MODEL_BUDGET_CONTENDED:-}" != 1 ]; then
                echo "refusing to measure the budget while another $image container runs ($others):" \
                    "wait until it is the only one, or set MODEL_BUDGET_CONTENDED=1 to write it with a note" >&2
                exit 5
            fi
            note="measured while another $image container ran: $others"
        fi
        ;;
esac

name=agentfs-model-$$-$RANDOM
state=$(mktemp -d)
trap 'rm -rf "$state"' EXIT
# The time of the last progress, replaced whole so the alarm never reads it half written.
stamp() { date +%s > "$state/last.new" && mv "$state/last.new" "$state/last"; }
stamp
: > "$state/started"
: > "$state/done"

# The job, its output read line by line for the alarm.
(
    set +e
    "$docker" run --init --rm --name "$name" --pids-limit 4096 -e TMPDIR=/tmp \
        -v "$root:/src" -v "${MODEL_TARGET_VOLUME:-agentfs-model-target}:/target" \
        -v "${MODEL_WORK_VOLUME:-agentfs-model-work}:/work" \
        -v "${MODEL_CARGO_VOLUME:-agentfs-model-cargo}:/usr/local/cargo/registry" \
        -e CARGO_TARGET_DIR=/target -e MODEL_BUDGET_NOTE="$note" \
        -w /src "$image" "$@" 2>&1
    echo $? > "$state/status"
) | while IFS= read -r line; do
    printf '%s\n' "$line"
    case $line in
        unit-start:*)
            echo "${line#unit-start: }" >> "$state/started"
            touch "$state/units"
            ;;
        unit-done:*)
            unit=${line#unit-done: }
            echo "$unit" >> "$state/done"
            stamp
            ;;
        *)
            [ -e "$state/units" ] || stamp
            ;;
    esac
    case $line in
        *"Resource temporarily unavailable"*) touch "$state/pids" ;;
    esac
done &
job=$!

# The alarm: a stall limit has no event to wait for, so it is the one clock here.
while kill -0 "$job" 2>/dev/null; do
    last=$(cat "$state/last")
    if [ $(( $(date +%s) - last )) -gt "$stall" ]; then
        "$docker" kill "$name" >/dev/null 2>&1 || true
        wait "$job" 2>/dev/null || true
        # A unit is in flight when it started and no `unit-done:` line begins with its name.
        running=$(awk 'FILENAME == ARGV[1] { done[++n] = $0 " "; next }
                       { for (i = 1; i <= n; i++) if (index(done[i], $0 " ") == 1) next; print }' \
            "$state/done" "$state/started" | paste -sd, -)
        echo "stalled: no unit finished in $stall s; in flight: ${running:-none (the build or the first unit)}" >&2
        exit 3
    fi
    sleep 2
done
wait "$job" || true
status=$(cat "$state/status" 2>/dev/null || echo 1)
if [ -e "$state/pids" ]; then
    echo "the job could not start a process (\"Resource temporarily unavailable\"): it hit the container's limit of 4096 processes, a process leak" >&2
    exit 4
fi
exit "$status"
