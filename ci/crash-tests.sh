#!/usr/bin/env bash
# Crash injection: the failpoint harness built with
# the `failpoints` feature.
#
# - crates/agentfs-core/tests/crash_points.rs: every engine failpoint,
#   killed and failed, against the crash oracle (no FUSE, no root);
# - crates/agentfs/tests/crash_daemon.rs: `record`, `push`, `run`,
#   `serve-history` with fronts and `patch apply`, killed (no FUSE, no
#   root).
#
# A tree view killed mid-stream is ci/e2e-tree-restart.sh's and the tracer's.
# Needs git. Reproducible on any such host:
#
#   ci/crash-tests.sh

. "$(dirname "$0")/lib.sh"

step "engine crash matrix"
cargo test --locked -p agentfs-core --features failpoints --test crash_points -- --nocapture

step "process crash tests"
cargo test --locked -p agentfs --features failpoints --test crash_daemon

