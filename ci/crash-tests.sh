#!/usr/bin/env bash
# Crash injection (design §18 test 3): the failpoint harness built with
# the `failpoints` feature.
#
# - crates/agentfs-core/tests/crash_points.rs: every engine failpoint,
#   killed and failed, against the crash oracle (no FUSE, no root);
# - crates/agentfs/tests/crash_daemon.rs: `record`, `push`, `run`,
#   `serve-history` with fronts and `patch apply`, killed (no FUSE, no
#   root);
# - crates/agentfs/tests/crash_mount.rs: a real FUSE mount killed after a
#   recorded close and while saving its repair queue (root, /dev/fuse).
#
# Needs git; the mount tests need Linux with /dev/fuse, fuse3, jq and
# passwordless sudo, and are skipped with a message without /dev/fuse.
# Reproducible on any such host:
#
#   ci/crash-tests.sh

. "$(dirname "$0")/lib.sh"

step "engine crash matrix"
cargo test --locked -p agentfs-core --features failpoints --test crash_points -- --nocapture

step "process crash tests"
cargo test --locked -p agentfs --features failpoints --test crash_daemon

step "a history service that cannot renew its state lock (design §14.2)"
cargo test --locked -p agentfs --features failpoints --test cli_standby

if [ ! -c /dev/fuse ]; then
  echo "skipping the mount crash tests: /dev/fuse is missing"
  exit 0
fi
sudo -n true 2>/dev/null || fail "the mount crash tests need passwordless sudo"
command -v jq >/dev/null || fail "jq is needed to find the test binary"

step "mount crash tests (as root)"
bin=$(cargo test --locked -p agentfs --features failpoints --test crash_mount --no-run --message-format=json 2>/dev/null |
  jq -r 'select(.executable != null and .target.name == "crash_mount") | .executable' | tail -n 1)
[ -x "$bin" ] || fail "could not find the crash_mount test binary"
sudo --preserve-env=RUST_BACKTRACE "$bin" --ignored --test-threads=1 --nocapture "$@"
