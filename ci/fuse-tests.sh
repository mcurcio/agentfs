#!/usr/bin/env bash
# Runs the real FUSE tests of `agentfs mount`
# (crates/agentfs-cli/tests/launcher_mount.rs), `agentfs host`
# (crates/agentfs-cli/tests/launcher_host.rs) and the CSI driver on tree
# volumes (crates/agentfs-cli/tests/csi_trees.rs) as root, and the unit
# tests that need root (creating as the caller, tree views stacked over a
# stale mount).
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
cargo build --locked -p agentfs-cli
# test_binary TARGET-NAME: the path of the test program TARGET-NAME in the JSON
# messages of `cargo test --no-run` on its standard input. Each cargo command is
# written out, so the repository's checks can read the package it selects. cargo
# test, not nextest: it only builds, and the tests below run the program itself
# under sudo with libtest's own flags.
test_binary() {
  local name=$1 path
  path=$(jq -r --arg name "$name" 'select(.executable != null and .target.name == $name) | .executable' |
    tail -n 1)
  [ -x "$path" ] || fail "could not find the $name test binary"
  echo "$path"
}
bin=$(cargo test --locked -p agentfs-cli --test launcher_mount --no-run --message-format=json 2>/dev/null |
  test_binary launcher_mount)
host_bin=$(cargo test --locked -p agentfs-cli --test launcher_host --no-run --message-format=json 2>/dev/null |
  test_binary launcher_host)
csi_bin=$(cargo test --locked -p agentfs-cli --test csi_trees --no-run --message-format=json 2>/dev/null |
  test_binary csi_trees)
unit=$(cargo test --locked -p agentfs-fuse --lib --no-run --message-format=json 2>/dev/null |
  test_binary agentfs_fuse)

step "creating as the caller, as root (crates/agentfs-fuse/src/creds.rs)"
OUT=$(sudo "$unit" creds:: --nocapture 2>&1) || { echo "$OUT"; fail "creds tests failed"; }
echo "$OUT"
case "$OUT" in *"skipped: needs root"*) fail "the root test did not run as root" ;; esac

step "tree views stacked over a stale mount, as root (crates/agentfs-fuse/src/tree_view.rs)"
OUT=$(sudo "$unit" tree_view::tests::stale_view --ignored --test-threads=1 --nocapture 2>&1) ||
  { echo "$OUT"; fail "stale view tests failed"; }
echo "$OUT"
case "$OUT" in *"test result: ok. 3 passed"*) ;; *) fail "the stale view tests did not all run" ;; esac

step "agentfs mount, as root, kernel $(uname -r) (crates/agentfs-cli/tests/launcher_mount.rs)"
sudo --preserve-env=RUST_BACKTRACE,RUNNER_TEMP "$bin" --ignored --test-threads=1 --nocapture "$@"

step "agentfs host, as root (crates/agentfs-cli/tests/launcher_host.rs)"
OUT=$(sudo --preserve-env=RUST_BACKTRACE,RUNNER_TEMP "$host_bin" --ignored --test-threads=1 --nocapture 2>&1) ||
  { echo "$OUT"; fail "agentfs host tests failed"; }
echo "$OUT"
case "$OUT" in *"skipped: needs root"*) fail "the host tests did not run as root" ;; esac

step "the CSI driver on tree volumes, as root (crates/agentfs-cli/tests/csi_trees.rs)"
OUT=$(sudo --preserve-env=RUST_BACKTRACE,RUNNER_TEMP "$csi_bin" --ignored --test-threads=1 --nocapture 2>&1) ||
  { echo "$OUT"; fail "csi_trees root tests failed"; }
echo "$OUT"
case "$OUT" in *"skipped: needs root"*) fail "the csi_trees tests did not run as root" ;; esac
case "$OUT" in *"test result: ok. 1 passed"*) ;; *) fail "the csi_trees root test did not run" ;; esac
