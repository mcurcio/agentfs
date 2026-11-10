#!/usr/bin/env bash
# Each model run's outcome as a digest, against a committed golden (VERIFYING.md,
# "Refactoring without changing outcomes"): the scenarios once, then each seed in its own
# process (its seed is in the run's name), the same shape as ci/replay-seeds.sh.
#   ci/model-digest.sh record|check             one whole run, in a temporary directory
#   ci/model-digest.sh collect DIR [SHARD OF]   digests of one shard's seeds, into DIR
#   ci/model-digest.sh accept|compare DIR       a filled DIR into, or against, the golden
# SHARD 0 also runs the scenarios; compare fails on any different, missing or extra line
# and names the runs. AGENTFS_MODEL_DIGEST_TEXT=1 also writes each run's text beside its
# digest, to diff. MODEL_DIGEST_LAST is the last seed (500), MODEL_DIGEST_THREADS each
# seed's test threads (4, never 1: the tests then run on the main thread and lose the
# test's name, which is in the run's), MODEL_DIGEST_JOBS how many seeds at once (nproc).
# A failing test does not stop the run: its digest is missing, the rest are compared, and
# the command still fails. In a container TMPDIR must be on the container's own disk (the
# default /tmp is), never a bind mount: every run makes a git repository there, and 8 seeds
# took 320 s with TMPDIR on a Docker Desktop bind mount against 24 s on /tmp. Use
# `docker run --init`: without an init process, orphaned git processes pile up as zombies
# until nothing can start. A test process past MODEL_DIGEST_TEST_SECONDS (1800) is killed
# and counts as failed, so a hang fails the run.
set -euo pipefail
cmd=${1:?record, check, collect, accept or compare}
root=$(cd "$(dirname "$0")/.." && pwd)
golden=$root/crates/agentfs-model/baseline/digests.txt
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
for dirs in "$PATH" /usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin; do
    found=$(PATH=$dirs git --version)
    if [ "$found" != "git version $release" ]; then
        echo "git here is $found, and the golden holds for $release (ci/git-release)" >&2
        exit 1
    fi
done

# Set by collect when a test failed: the digests of the runs that finished still count.
tests_failed=0

# collect DIR [SHARD OF]: the scenarios (in shard 0) and that shard's seeds, each seed in
# its own process with its digests in DIR, named as the run.
collect() {
    local dir=$1 shard=${2:-0} of=${3:-1}
    local target=${CARGO_TARGET_DIR:-$root/target}
    mkdir -p "$dir"
    cd "$root"
    # Build first, so a failure below can only be a test's.
    cargo test --locked -p agentfs-model --test scenarios --no-run
    if [ "$shard" = 0 ]; then
        AGENTFS_MODEL_DIGEST_DIR=$dir timeout "$limit" cargo test --locked -p agentfs-model --test scenarios \
            -- --test-threads="$threads" >/dev/null || tests_failed=1
    fi
    cargo test --locked -p agentfs-model --test interleavings --no-run
    # The test binary cargo just built: the newest of that name.
    local bin
    bin=$(find "$target/debug/deps" -maxdepth 1 -type f -executable -name 'interleavings-*' -printf '%T@ %p\n' | sort -n | tail -1 | cut -d' ' -f2)
    seq 1 "$last" | awk -v s="$shard" -v n="$of" '$1 % n == s % n' |
        AGENTFS_MODEL_DIGEST_DIR=$dir xargs -P "$jobs" -I{} env AGENTFS_MODEL_SEED={} timeout "$limit" "$bin" --skip commit_dates --test-threads="$threads" >/dev/null || tests_failed=1
    echo "collected $(find "$dir" -name '*.digest' | wc -l) runs in $dir"
    if [ "$tests_failed" != 0 ]; then
        echo "a test failed while collecting: its digests are missing" >&2
    fi
}

# The sorted `<run> <sha>` lines of a filled directory, read from each file's name and body.
listing() {
    local dir=$1 f run
    find "$dir" -name '*.digest' -print0 | while IFS= read -r -d '' f; do
        run=${f##*/}
        printf '%s %s\n' "${run%.digest}" "$(cat "$f")"
    done | LC_ALL=C sort
}

accept() {
    local dir=$1
    mkdir -p "$(dirname "$golden")"
    { printf '# git %s\n' "$release"; listing "$dir"; } > "$golden"
    echo "recorded $(grep -vc '^#' "$golden") runs in $golden"
}

# Any different, missing or extra line fails the check, and each is named.
compare() {
    local dir=$1 header
    header=$(sed -n 1p "$golden")
    if [ "$header" != "# git $release" ]; then
        echo "the golden says \"$header\", and ci/git-release pins $release: re-record it" >&2
        exit 1
    fi
    local have want status=0
    have=$(mktemp)
    want=$(mktemp)
    listing "$dir" > "$have"
    grep -v '^#' "$golden" > "$want"
    awk 'NR==FNR { want[$1]=$2; next }
         { have[$1]=$2 }
         END { bad=0
               for (r in want) { if (!(r in have)) { print "missing: " r; bad=1 } else if (want[r]!=have[r]) { print "different: " r; bad=1 } }
               for (r in have) { if (!(r in want)) { print "extra: " r; bad=1 } }
               exit bad }' "$want" "$have" | LC_ALL=C sort || status=${PIPESTATUS[0]}
    rm -f "$have" "$want"
    if [ "$status" != 0 ]; then
        echo "the digests differ from $golden" >&2
        exit 1
    fi
    echo "$(grep -vc '^#' "$golden") runs match $golden"
}

case $cmd in
    record | check)
        dir=$(mktemp -d)
        trap 'rm -rf "$dir"' EXIT
        collect "$dir"
        if [ "$cmd" = check ]; then
            compare "$dir"
        elif [ "$tests_failed" = 0 ]; then
            accept "$dir"
        fi
        exit "$tests_failed"
        ;;
    collect)
        collect "${2:?a directory}" "${3:-0}" "${4:-1}"
        exit "$tests_failed"
        ;;
    accept) accept "${2:?a directory}" ;;
    compare) compare "${2:?a directory}" ;;
    *)
        echo "usage: $0 record|check|collect DIR [SHARD OF]|accept DIR|compare DIR" >&2
        exit 2
        ;;
esac
