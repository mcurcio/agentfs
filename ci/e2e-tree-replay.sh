#!/usr/bin/env bash
# The model's recorded calls on real mounts (git-model §18, the first row): each
# mountable trace on a new mount of a failpoints build, its takes at the trace's
# points through AGENTFS_TAKE_GATE. Needs Linux, git, fuse3 and /dev/fuse.
#   ci/e2e-tree-replay.sh TRACE_DIR MIN_SCENARIOS MIN_SEEDS
set -euo pipefail
traces=${1:?a directory of traces}
min_scenarios=${2:?a floor of scenario traces}
min_seeds=${3:?a floor of seed traces}
replay=${PHASE1_REPLAY:-$PWD/target/debug/phase1-replay}
export AGENTFS_BIN=${AGENTFS_BIN:-$PWD/target/debug/agentfs}
"$AGENTFS_BIN" --version | grep -q failpoints || {
    echo "$AGENTFS_BIN is not a failpoints build" >&2
    exit 2
}
"$replay" mount "$traces" --min-scenarios "$min_scenarios" --min-seeds "$min_seeds"
