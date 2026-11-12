#!/usr/bin/env bash
# The model's recorded calls on real mounts (git-model §18, the first row): each
# mountable trace on a new mount of a failpoints build, its takes at the trace's
# points through AGENTFS_TAKE_GATE. Needs Linux, git, fuse3 and /dev/fuse.
# With RECORD_DIR, each mount's own recording of its calls is written there (it
# must be empty or missing) and then replays on a fresh model at the same floors:
# a mount's mismatch arrives as an engine trace the model replays (VERIFYING.md,
# "A regression found on the engine").
#   ci/e2e-tree-replay.sh TRACE_DIR MIN_SCENARIOS MIN_SEEDS [RECORD_DIR]
set -euo pipefail
traces=${1:?a directory of traces}
min_scenarios=${2:?a floor of scenario traces}
min_seeds=${3:?a floor of seed traces}
recorded=${4:-}
replay=${PHASE1_REPLAY:-$PWD/target/debug/phase1-replay}
export AGENTFS_BIN=${AGENTFS_BIN:-$PWD/target/debug/agentfs}
"$AGENTFS_BIN" --version | grep -q failpoints || {
    echo "$AGENTFS_BIN is not a failpoints build" >&2
    exit 2
}
if [ -z "$recorded" ]; then
    exec "$replay" mount "$traces" --min-scenarios "$min_scenarios" --min-seeds "$min_seeds"
fi
"$replay" mount "$traces" --min-scenarios "$min_scenarios" --min-seeds "$min_seeds" --record "$recorded"
"$replay" model "$recorded" --min-scenarios "$min_scenarios" --min-seeds "$min_seeds"
