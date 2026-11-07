#!/usr/bin/env bash
# The library's replay of the phase 1 generators at many seeds (git-model §18), one
# shard of them: records each seed in its own process, then replays what it recorded.
#   ci/replay-seeds.sh SHARD OF LAST_SEED TRACE_DIR
set -euo pipefail
shard=${1:?a shard}
of=${2:?how many shards}
last=${3:?the last seed}
traces=${4:?a directory of traces}
cargo build --locked -p agentfs-conformance --bin phase1-replay
cargo test --locked -p agentfs-model --test interleavings --no-run
# The test binary cargo just built: the newest of that name.
bin=$(find target/debug/deps -maxdepth 1 -type f -executable -name 'interleavings-*' -printf '%T@ %p\n' | sort -n | tail -1 | cut -d' ' -f2)
mkdir -p "$traces"
seq 1 "$last" | awk -v s="$shard" -v n="$of" '$1 % n == s' |
    AGENTFS_MODEL_TRACE_DIR=$traces xargs -P "$(nproc)" -I{} env AGENTFS_MODEL_SEED={} "$bin" phase1_scope >/dev/null
echo "recorded $(find "$traces" -name '*.trace' | wc -l) traces of seeds $shard mod $of up to $last"
target/debug/phase1-replay library "$traces" --min-scenarios 0 --min-seeds 1
