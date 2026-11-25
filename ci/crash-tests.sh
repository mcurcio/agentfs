#!/usr/bin/env bash
# Crash injection: the failpoint harness built with
# the `failpoints` feature.
#
# - crates/agentfs-core/tests/crash_points.rs: every recording-engine
#   failpoint, killed and failed, against the crash oracle (no FUSE, no
#   root);
# - crates/agentfs-cli/tests/crash_daemon.rs: `record`, `push`, `run`,
#   `serve-history` with fronts and `patch apply`, killed (no FUSE, no
#   root).
#
# The tree engine's kill tests are its `it` program's (agentfs-engine's
# tests/it/tree_actor.rs), run by the test job's all-features leg; the kill
# after a returned write runs below too. A tree view killed mid-stream is
# ci/e2e-tree-restart.sh's and the tracer's.
# Needs git and cargo-nextest. Reproducible on any such host:
#
#   ci/crash-tests.sh

. "$(dirname "$0")/lib.sh"

step "engine crash matrix"
cargo nextest run --locked -p agentfs-core --features failpoints --test crash_points --no-capture

step "process crash tests"
cargo nextest run --locked -p agentfs-cli --features failpoints --test crash_daemon


step "tree engine kill"
cargo nextest run --locked -p agentfs-engine --features failpoints --test it tree_actor::a_kill_after_a_write_returned_keeps_it
