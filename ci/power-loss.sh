#!/usr/bin/env bash
# Power loss and fsync ordering (design §3.3, §18 test 3): a workload of
# `agentfs record` and `agentfs push` on a filesystem on dm-log-writes,
# which logs every write, flush and FUA the filesystem sends the disk.
# The log is then replayed onto a second device, and at every FUA (or
# every flush, with `flush`) the state the disk would have after a power
# cut there is checked by ci/power-loss-check.sh, on a snapshot: the
# filesystem mounts, `agentfs flush` recovers, git fsck passes, and every
# record and push the workload had been told was done (its promise
# fsynced before the cut) is in history.
#
#   sudo REPLAY_LOG=... AGENTFS_BIN=... ci/power-loss.sh ext4|xfs [fua|flush]
#
# Needs root, the dm-log-writes and dm-snapshot kernel modules, losetup,
# dmsetup, mkfs for the filesystem, git, and replay-log
# (ci/install-replay-log.sh). POWER_LOSS_DIR names the work directory
# (default: a new one under $TMPDIR); its logs stay for a failed run.

. "$(dirname "$0")/lib.sh"

fs=${1:?usage: ci/power-loss.sh ext4|xfs [fua|flush]}
check=${2:-fua}
case $fs in ext4 | xfs) ;; *) fail "unknown filesystem $fs" ;; esac
case $check in fua | flush) ;; *) fail "unknown check mode $check" ;; esac
[ "$(id -u)" = 0 ] || fail "run as root: it creates loop and device-mapper devices"
[ -x "${REPLAY_LOG:-}" ] || fail "REPLAY_LOG must name replay-log (ci/install-replay-log.sh)"
[ -x "$AGENTFS_BIN" ] || fail "no agentfs binary at $AGENTFS_BIN"
for m in dm_log_writes dm_snapshot; do
  modprobe "$m" || fail "kernel module $m is not available"
done

work=${POWER_LOSS_DIR:-$(mktemp -d)}
mkdir -p "$work"
mnt=$work/mnt
records=${POWER_LOSS_RECORDS:-40}
mkdir -p "$mnt" "$work/check"
LW=agentfs-lw
CK=agentfs-ck
devices=()

cleanup() {
  umount "$mnt" 2>/dev/null || true
  umount "$work/check" 2>/dev/null || true
  dmsetup remove "$CK" 2>/dev/null || true
  dmsetup remove "$LW" 2>/dev/null || true
  for d in "${devices[@]}"; do losetup -d "$d" 2>/dev/null || true; done
}
trap cleanup EXIT

loop() { # file size
  truncate -s "$2" "$1"
  local dev
  dev=$(losetup -f --show "$1")
  devices+=("$dev")
  echo "$dev"
}

step "devices ($fs, check at every $check)"
data=$(loop "$work/data.img" 512M)
log=$(loop "$work/log.img" 4G)
replay=$(loop "$work/replay.img" 512M)
cow=$(loop "$work/cow.img" 512M)
sectors=$(blockdev --getsz "$data")
dmsetup create "$LW" --table "0 $sectors log-writes $data $log"
case $fs in
  ext4) mkfs.ext4 -q -F "/dev/mapper/$LW" ;;
  xfs) mkfs.xfs -q -f "/dev/mapper/$LW" ;;
esac
dmsetup message "$LW" 0 mark mkfs
mount "/dev/mapper/$LW" "$mnt"

step "workload: $records records, a push every 8"
# The manifest (off the device) lists every record before it starts:
# `<n> <path> <sha256 or ->`. The promise file (on the device) gets
# `R <n>` once record n returned and `U <oid>` once a push of <oid>
# returned, each fsynced before the workload goes on.
manifest=$work/manifest
: >"$manifest"
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null HOME=$work
agentfs_() { "$AGENTFS_BIN" --state "$mnt/state" --root "$mnt/tree" "$@"; }
mkdir -p "$mnt/tree"
agentfs_ init --volume demo >/dev/null
git init -q --bare "$mnt/remote.git"
git --git-dir "$mnt/state/git" config remote.origin.url "file://$mnt/remote.git"
git --git-dir "$mnt/state/git" config agentfs.commitWindow 10ms
: >"$mnt/promises"
sync
echo S >>"$mnt/promises"
sync "$mnt/promises"
dmsetup message "$LW" 0 mark setup

for n in $(seq 1 "$records"); do
  path=f$((n % 6))
  case $((n % 10)) in
    7) # a removal
      rm -f "$mnt/tree/$path"
      sum=- ;;
    3) # something larger, rewritten in place
      head -c $((150000 + n)) /dev/urandom >"$mnt/tree/$path"
      sum=$(sha256sum <"$mnt/tree/$path" | cut -d' ' -f1) ;;
    *)
      printf 'record %s\n' "$n" >"$mnt/tree/$path"
      sum=$(sha256sum <"$mnt/tree/$path" | cut -d' ' -f1) ;;
  esac
  echo "$n $path $sum" >>"$manifest"
  agentfs_ record "$path" >/dev/null
  echo "R $n" >>"$mnt/promises"
  sync "$mnt/promises"
  if [ $((n % 8)) = 0 ]; then
    agentfs_ push >/dev/null
    echo "U $(git --git-dir "$mnt/state/git" rev-parse refs/heads/agentfs/demo)" >>"$mnt/promises"
    sync "$mnt/promises"
  fi
done
umount "$mnt"
dmsetup remove "$LW"
pass "workload done: $($REPLAY_LOG --log "$log" --num-entries) log entries"

step "replay, checking at every $check"
export REPLAY_DEV=$replay COW_DEV=$cow CHECK_MNT=$work/check MANIFEST=$manifest CHECK_DM=$CK
export CHECK_COUNT=$work/checks FS=$fs AGENTFS_BIN
: >"$CHECK_COUNT"
"$REPLAY_LOG" --log "$log" --replay "$replay" --end-mark mkfs
"$REPLAY_LOG" --log "$log" --replay "$replay" --start-mark mkfs \
  --check "$check" --fsck "$PWD/ci/power-loss-check.sh" ||
  fail "a check failed (above); the log is in $work"

step "the final state: everything promised"
CHECK_FINAL=1 ci/power-loss-check.sh || fail "the final state failed its check"
pass "$(wc -l <"$CHECK_COUNT") power-cut points checked on $fs"
