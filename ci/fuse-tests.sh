#!/usr/bin/env bash
# Runs the real FUSE mount tests (crates/agentfs/tests/mount.rs) as root.
#
# Needs Linux with /dev/fuse, fuse3 (fusermount3), sqlite3, git, jq,
# setpriv and passwordless sudo; the unprivileged-daemon test also needs
# `user_allow_other` in /etc/fuse.conf. Reproducible on any such host:
#
#   AGENTFS_TEST_PASSTHROUGH=auto AGENTFS_TEST_UNPRIV_USER=$USER ci/fuse-tests.sh
#
# AGENTFS_TEST_PASSTHROUGH (auto, on, off) is passed to every mount.
# Extra arguments go to the test binary (e.g. a test name filter).

. "$(dirname "$0")/lib.sh"

[ -c /dev/fuse ] || fail "/dev/fuse is missing: these tests need FUSE"
sudo -n true 2>/dev/null || fail "these tests need passwordless sudo"
command -v jq >/dev/null || fail "jq is needed to find the test binary"

step "build"
cargo build --locked -p agentfs
bin=$(cargo test --locked -p agentfs --test mount --no-run --message-format=json 2>/dev/null |
  jq -r 'select(.executable != null and .target.name == "mount") | .executable' | tail -n 1)
[ -x "$bin" ] || fail "could not find the mount test binary"

step "kernel $(uname -r), passthrough ${AGENTFS_TEST_PASSTHROUGH:-auto}"
sudo --preserve-env=AGENTFS_TEST_PASSTHROUGH,AGENTFS_TEST_UNPRIV_USER,RUST_BACKTRACE,RUNNER_TEMP \
  "$bin" --ignored --test-threads=1 --nocapture "$@"
