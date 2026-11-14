#!/usr/bin/env bash
# Each model run's outcome as a digest, against a committed golden (VERIFYING.md,
# "Refactoring without changing outcomes"): the scenarios once, then each seed in its own
# process (its seed is in the run's name), the same shape as ci/replay-seeds.sh. Run it
# through ci/model-docker.sh, never a hand-written `docker run`.
#   ci/model-digest.sh record                    one whole run into the golden, timed into
#                                                planted/budget.json
#   ci/model-digest.sh check [--all]             one run against the golden: the scenarios
#                                                first, stopping at their first difference
#                                                before any seed runs, then the regression
#                                                seeds (tests/regression-seeds.txt, the fast
#                                                tier); --all runs every seed and reports
#                                                every difference
#   ci/model-digest.sh collect DIR [SHARD OF]    one shard's digests, into a new or empty DIR
#   ci/model-digest.sh accept|compare DIR...     filled DIRs into, or against, the golden
# A shard splits the run under a time limit, each into its own DIR, chained with `&&`:
#   collect D0 0 2 && collect D1 1 2 && compare D0 D1
# SHARD 0 also runs the scenarios. collect writes a success marker only when every test
# passed; accept and compare need one marker for each shard 0..OF-1, so a missing digest
# (a failed or killed test) can never be filled by an old one. compare fails on any
# different, missing or extra line and names the runs. AGENTFS_MODEL_DIGEST_TEXT=1 also
# writes each run's text beside its digest, to diff. MODEL_DIGEST_LAST is the last seed
# (500), MODEL_DIGEST_THREADS each seed's test threads (4, never 1: the tests then run on
# the main thread and lose the test's name, which is in the run's), MODEL_DIGEST_JOBS how
# many seeds at once (nproc). A failing test does not stop the run: its digest is missing,
# the rest are compared, and the command still fails. TMPDIR must not be on a host file
# share: every run makes a git repository there, and 8 seeds took 320 s on a Docker Desktop
# bind mount against 24 s on the container's /tmp, so collect refuses one (exit 2). A test
# process past MODEL_DIGEST_TEST_SECONDS (1800) is killed and counts as failed, so a hang
# fails the run. A check slower than 1.5 times its budgeted seconds fails. Each unit (the
# scenarios, a seed) prints `unit-start:` and `unit-done:` lines for the wrapper's alarm.
set -euo pipefail
cmd=${1:?record, check, collect, accept or compare}
root=$(cd "$(dirname "$0")/.." && pwd)
golden=$root/crates/agentfs-model/baseline/digests.txt
budget=$root/crates/agentfs-model/planted/budget.json
release=$(sed -n 1p "$root/ci/git-release")
last=${MODEL_DIGEST_LAST:-500}
threads=${MODEL_DIGEST_THREADS:-4}
jobs=${MODEL_DIGEST_JOBS:-$(nproc)}
limit=${MODEL_DIGEST_TEST_SECONDS:-1800}
if [ "$threads" -lt 2 ]; then
    echo "MODEL_DIGEST_THREADS is $threads: with one thread libtest loses each test's name" >&2
    exit 1
fi

# Line identity depends on the release (MODEL.md, finding 42): the golden holds for one.
git_release() {
    local dirs found
    for dirs in "$PATH" /usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin; do
        found=$(PATH=$dirs git --version)
        if [ "$found" != "git version $release" ]; then
            echo "git here is $found, and the golden holds for $release (ci/git-release)" >&2
            exit 1
        fi
    done
}

# Set by collect when a test failed: the digests of the runs that finished still count.
tests_failed=0

# The path of test target $1 that cargo builds, as cargo prints it.
test_binary() {
    local path
    path=$(cargo test --locked -p agentfs-model --test "$1" --no-run 2>&1 |
        sed -n "s|^ *Executable tests/$1\.rs (\(.*\))\$|\1|p")
    if [ -n "$path" ]; then realpath "$path"; fi
}

# A budgeted time in seconds, or nothing.
budgeted() {
    python3 -c 'import json,sys
b=json.load(open(sys.argv[1])).get("digest", {}).get(sys.argv[2])
print(b if b is not None else "")' "$budget" "$1" 2>/dev/null || true
}

# Fails when $2 seconds is over 1.5 times the budget named $1.
within_budget() {
    local allowed
    allowed=$(budgeted "$1")
    if [ -n "$allowed" ] && [ $(( $2 * 2 )) -gt $(( allowed * 3 )) ]; then
        echo "$1 took $2 s, over 1.5 times its budget of $allowed s ($budget)" >&2
        return 1
    fi
}

# collect DIR [SHARD OF [WHAT]]: the scenarios (in shard 0) and that shard's seeds, each seed
# in its own process with its digests in DIR, named as the run; a marker if all passed.
# WHAT is `scenarios` or `seeds` to collect only those, or `regression` for only the
# regression seeds (check's halves; no marker).
collect() {
    local dir=$1 shard=${2:-0} of=${3:-1} what=${4:-all}
    git_release
    if [ "$what" != seeds ] && [ "$what" != regression ] && [ -e "$dir" ] && [ -n "$(ls -A "$dir")" ]; then
        echo "$dir is not empty: collect each shard into a new directory" >&2
        exit 1
    fi
    mkdir -p "$dir"
    dir=$(realpath "$dir")
    python3 "$root/crates/agentfs-model/planted/mounts.py" "${TMPDIR:-/tmp}" || exit 2
    cd "$root"
    # Build first, so a failure below can only be a test's.
    local scenarios bin
    scenarios=$(test_binary scenarios)
    bin=$(test_binary interleavings)
    if [ -z "$scenarios" ] || [ -z "$bin" ]; then
        echo "cargo printed no test binary" >&2
        exit 1
    fi
    if [ "$shard" = 0 ] && [ "$what" != seeds ] && [ "$what" != regression ]; then
        local s=$SECONDS
        echo "unit-start: scenarios"
        # In the package's directory, as cargo runs it.
        (cd crates/agentfs-model &&
            AGENTFS_MODEL_DIGEST_DIR=$dir timeout "$limit" "$scenarios" --test-threads="$threads" \
                >/dev/null) || tests_failed=1
        echo "unit-done: scenarios $((SECONDS - s))s"
        printf 'scenarios %s\n' $((SECONDS - s)) >> "$dir/units.tsv"
    fi
    if [ "$what" != scenarios ]; then
        { if [ "$what" = regression ]; then regression_seeds; else seq 1 "$last"; fi; } | awk -v s="$shard" -v n="$of" '$1 % n == s % n' |
            (cd crates/agentfs-model && AGENTFS_MODEL_DIGEST_DIR=$dir xargs -P "$jobs" -I{} \
                env AGENTFS_MODEL_SEED={} DIR="$dir" BIN="$bin" LIMIT="$limit" THREADS="$threads" sh -c '
                    s=$(date +%s); echo "unit-start: seed $AGENTFS_MODEL_SEED"
                    timeout "$LIMIT" "$BIN" --skip commit_dates --test-threads="$THREADS" >/dev/null
                    status=$?; t=$(( $(date +%s) - s ))
                    echo "unit-done: seed $AGENTFS_MODEL_SEED ${t}s"
                    echo "seed $t" >> "$DIR/units.tsv"
                    exit $status') || tests_failed=1
    fi
    echo "collected $(find "$dir" -name '*.digest' | wc -l) runs in $dir"
    if [ "$tests_failed" != 0 ]; then
        echo "a test failed while collecting: its digests are missing, and no marker is written" >&2
    elif [ "$what" = all ]; then
        printf '%s %s %s\n' "$shard" "$of" "$last" > "$dir/shard-$shard-of-$of.ok"
    fi
}

# Fails unless DIRs hold one success marker for each shard 0..OF-1 of one split.
markers() {
    local all="" of="" d f
    for d in "$@"; do
        for f in "$d"/shard-*.ok; do
            if [ -e "$f" ]; then all+=$(cat "$f")$'\n'; fi
        done
    done
    all=${all%$'\n'}
    if [ -z "$all" ]; then
        echo "no success marker in $*: a collect failed or did not run" >&2
        return 1
    fi
    of=$(printf '%s\n' "$all" | awk '{print $2 "/" $3}' | sort -u)
    if [ "$(printf '%s\n' "$of" | wc -l)" != 1 ]; then
        echo "the markers come from different splits: $of" >&2
        return 1
    fi
    local n=${of%/*} s
    for s in $(seq 0 $((n - 1))); do
        if [ "$(printf '%s\n' "$all" | awk -v s="$s" '$1 == s' | wc -l)" != 1 ]; then
            echo "shard $s of $n has no success marker, or two" >&2
            return 1
        fi
    done
}

# The sorted `<run> <sha>` lines of filled directories, read from each file's name and body.
listing() {
    local f run
    find "$@" -name '*.digest' -print0 | while IFS= read -r -d '' f; do
        run=${f##*/}
        printf '%s %s\n' "${run%.digest}" "$(cat "$f")"
    done | LC_ALL=C sort
}

# The regression seeds, both kinds, one per line (tests/regression-seeds.txt).
regression_seeds() {
    grep -v '^#' "$root/crates/agentfs-model/tests/regression-seeds.txt" |
        awk '{ for (i = 2; i <= NF; i++) print $i }' | sort -nu
}

# The golden's runs; with `scenarios`, only theirs (a seed's run ends in `.seed<n>`); with
# `regression`, the scenarios' and the regression seeds'.
golden_lines() {
    if [ "${1:-}" = regression ]; then
        grep -v '^#' "$golden" |
            awk -v keep="$(regression_seeds | paste -sd' ' -)" '
                BEGIN { n = split(keep, k, " "); for (i = 1; i <= n; i++) want[k[i]] = 1 }
                !match($1, /\.seed[0-9]+$/) { print; next }
                want[substr($1, RSTART + 5)] { print }'
    elif [ "${1:-}" = scenarios ]; then
        grep -v '^#' "$golden" | grep -Ev '\.seed[0-9]+ ' || true
    else
        grep -v '^#' "$golden"
    fi
}

accept() {
    markers "$@" || exit 1
    mkdir -p "$(dirname "$golden")"
    { printf '# git %s\n' "$release"; listing "$@"; } > "$golden"
    echo "recorded $(grep -vc '^#' "$golden") runs in $golden"
}

# Prints a line for each different, missing or extra run of the listing $1 against the
# golden lines $2 (only the first if $3 is `first`); fails if there is one.
differences() {
    awk -v only="${3:-}" 'NR==FNR { want[$1]=$2; next }
         { if ($1 in have) { out[++n]="twice: " $1 } have[$1]=$2 }
         END { for (r in want) { if (!(r in have)) out[++n]="missing: " r; else if (want[r]!=have[r]) out[++n]="different: " r }
               for (r in have) { if (!(r in want)) out[++n]="extra: " r }
               for (i = 1; i <= n; i++) print out[i]
               exit (n > 0) }' "$2" "$1" | LC_ALL=C sort | { if [ "${3:-}" = first ]; then head -1; else cat; fi; }
    return "${PIPESTATUS[0]}"
}

header_ok() {
    local header
    header=$(sed -n 1p "$golden")
    if [ "$header" != "# git $release" ]; then
        echo "the golden says \"$header\", and ci/git-release pins $release: re-record it" >&2
        exit 1
    fi
}

# Any different, missing or extra line fails the check, and each is named; so does a
# missing marker.
compare() {
    header_ok
    local have want status=0
    have=$(mktemp)
    want=$(mktemp)
    listing "$@" > "$have"
    golden_lines > "$want"
    differences "$have" "$want" || status=1
    rm -f "$have" "$want"
    markers "$@" || status=1
    if [ "$status" != 0 ]; then
        echo "the digests differ from $golden" >&2
        exit 1
    fi
    echo "$(grep -vc '^#' "$golden") runs match $golden"
}

# The slowest unit of kind $1 in DIR's units.tsv.
slowest() {
    awk -v k="$1" '$1 == k && $2 > m { m = $2 } END { print m + 0 }' "$2/units.tsv"
}

case $cmd in
    record)
        dir=$(mktemp -d)
        trap 'rm -rf "$dir"' EXIT
        start=$SECONDS
        collect "$dir"
        [ "$tests_failed" = 0 ] || exit 1
        accept "$dir"
        total=$((SECONDS - start))
        python3 - "$budget" "$(slowest scenarios "$dir")" "$total" "$(slowest seed "$dir")" <<'PY'
import json, os, sys
path, scenarios, total, seed = sys.argv[1], *map(int, sys.argv[2:])
b = json.load(open(path)) if os.path.exists(path) else {}
b["digest"] = {"scenarios_seconds": scenarios, "full_seconds": total, "slowest_seed_seconds": seed}
# ci/model-docker.sh says when the times were measured beside another container.
if os.environ.get("MODEL_BUDGET_NOTE"):
    b["digest"]["note"] = os.environ["MODEL_BUDGET_NOTE"]
open(path, "w").write(json.dumps(b, indent=2, sort_keys=True) + "\n")
PY
        echo "recorded the digest's times in $budget: scenarios $(slowest scenarios "$dir") s, all $total s"
        ;;
    check)
        dir=$(mktemp -d)
        trap 'rm -rf "$dir"' EXIT
        start=$SECONDS
        if [ "${2:-}" = --all ]; then
            collect "$dir"
            compare "$dir"
            within_budget full_seconds $((SECONDS - start)) || exit 1
            exit "$tests_failed"
        fi
        # Fast: the scenarios first, stopping at their first difference.
        header_ok
        collect "$dir" 0 1 scenarios
        want=$(mktemp)
        golden_lines scenarios > "$want"
        if [ "$tests_failed" != 0 ] || ! differences <(listing "$dir") "$want" first; then
            echo "the scenarios' digests differ from $golden (first difference above): no seed runs" >&2
            exit 1
        fi
        # The budget after the seeds, so a slow scenario run never hides their result.
        scenario_seconds=$(slowest scenarios "$dir")
        collect "$dir" 0 1 regression
        golden_lines regression > "$want"
        if [ "$tests_failed" != 0 ] || ! differences <(listing "$dir") "$want" first; then
            echo "the regression seeds' digests differ from $golden (first difference above; check --all runs every seed)" >&2
            exit 1
        fi
        echo "$(wc -l < "$want") runs of the scenarios and the regression seeds match $golden"
        within_budget scenarios_seconds "$scenario_seconds" || exit 1
        ;;
    collect)
        collect "${2:?a directory}" "${3:-0}" "${4:-1}"
        exit "$tests_failed"
        ;;
    accept) accept "${2:?a directory}" "${@:3}" ;;
    compare) compare "${2:?a directory}" "${@:3}" ;;
    *)
        echo "usage: $0 record|check [--all]|collect DIR [SHARD OF]|accept DIR...|compare DIR..." >&2
        exit 2
        ;;
esac
