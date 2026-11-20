#!/usr/bin/env bash
# Runs the real FUSE tests of `agentfs mount` (crates/agentfs/tests/
# launcher_mount.rs) and `agentfs host` (crates/agentfs/tests/
# launcher_host.rs) as root, and the unit tests that need root (creating as
# the caller, tree views stacked over a stale mount).
#
# Needs Linux with /dev/fuse, fuse3 (fusermount3), git, jq and passwordless
# sudo. Reproducible on any such host:
#
#   ci/fuse-tests.sh
#
# Extra arguments go to the launcher_mount test binary (e.g. a test name
# filter).

. "$(dirname "$0")/lib.sh"

[ -c /dev/fuse ] || fail "/dev/fuse is missing: these tests need FUSE"
sudo -n true 2>/dev/null || fail "these tests need passwordless sudo"
command -v jq >/dev/null || fail "jq is needed to find the test binary"

step "build"
cargo build --locked -p agentfs
bin=$(cargo test --locked -p agentfs --test launcher_mount --no-run --message-format=json 2>/dev/null |
  jq -r 'select(.executable != null and .target.name == "launcher_mount") | .executable' | tail -n 1)
[ -x "$bin" ] || fail "could not find the launcher_mount test binary"
host_bin=$(cargo test --locked -p agentfs --test launcher_host --no-run --message-format=json 2>/dev/null |
  jq -r 'select(.executable != null and .target.name == "launcher_host") | .executable' | tail -n 1)
[ -x "$host_bin" ] || fail "could not find the launcher_host test binary"
unit=$(cargo test --locked -p agentfs-fuse --lib --no-run --message-format=json 2>/dev/null |
  jq -r 'select(.executable != null and .target.name == "agentfs_fuse") | .executable' | tail -n 1)
[ -x "$unit" ] || fail "could not find the agentfs-fuse unit test binary"

step "creating as the caller, as root (crates/agentfs-fuse/src/creds.rs)"
OUT=$(sudo "$unit" creds:: --nocapture 2>&1) || { echo "$OUT"; fail "creds tests failed"; }
echo "$OUT"
case "$OUT" in *"skipped: needs root"*) fail "the root test did not run as root" ;; esac

step "tree views stacked over a stale mount, as root (crates/agentfs-fuse/src/tree_view.rs)"
OUT=$(sudo "$unit" tree_view::tests::stale_view --ignored --test-threads=1 --nocapture 2>&1) ||
  { echo "$OUT"; fail "stale view tests failed"; }
echo "$OUT"
case "$OUT" in *"test result: ok. 3 passed"*) ;; *) fail "the stale view tests did not all run" ;; esac

step "agentfs mount, as root, kernel $(uname -r) (crates/agentfs/tests/launcher_mount.rs)"
sudo --preserve-env=RUST_BACKTRACE,RUNNER_TEMP "$bin" --ignored --test-threads=1 --nocapture "$@"

step "agentfs host, as root (crates/agentfs/tests/launcher_host.rs)"
OUT=$(sudo --preserve-env=RUST_BACKTRACE,RUNNER_TEMP "$host_bin" --ignored --test-threads=1 --nocapture 2>&1) ||
  { echo "$OUT"; fail "agentfs host tests failed"; }
echo "$OUT"
case "$OUT" in *"skipped: needs root"*) fail "the host tests did not run as root" ;; esac
