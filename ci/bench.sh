#!/usr/bin/env bash
# The end-to-end benchmark (design §18 test 13, §19.2 item 1;
# docs/performance.md): the synchronous record step without FUSE
# (crates/agentfs-core/benches/record_step.rs), then workloads through a
# real agentfs mount, each beside a baseline in a plain directory on the
# same filesystem:
#
#   close4k  4 KiB write-and-close latency (ci/boundary-latency.py);
#   clone    `git clone` of git.git v2.47.0 with agentfs.nested=working-tree;
#   npm      30,000 small files written into an excluded node_modules/;
#   create   100,000 small files, then
#   mvdir    a rename of their directory (one move, design §3.1), then
#   rm       `rm -rf` of them;
#   bigclose closing a 1 GiB file, dirty and after fsync (reflink on XFS,
#            copy on ext4).
#
# After each workload it waits for the commit loop to catch up (`agentfs
# status` shows no backlog) and times that too, then checks the history
# against the backing tree.
#
#   ci/bench.sh xfs|ext4   a fresh loop filesystem of that type (sudo)
#   ci/bench.sh dir PATH   the filesystem PATH is on
#
# Needs Linux with /dev/fuse, fuse3, git, jq, python3, passwordless sudo,
# and for xfs xfsprogs; reproducible on any such host. Results go to
# $BENCH_OUT (default ./bench-out): env.json, record.md, record.jsonl,
# results.jsonl, and the mount logs; ci/bench-report.py turns them into
# markdown. BENCH_QUICK=1 shrinks every workload for a smoke run.
# BENCH_SHARD picks the steps: `many` runs create, mvdir and rm; `rest`
# runs everything else (record_step, close4k, clone, npm, bigclose); `all`
# (the default) runs both. The bench workflow runs the two shards as
# separate jobs and reports their outputs together, per filesystem.

. "$(dirname "$0")/lib.sh"

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/.." && pwd)
AGENTFS_BIN=${AGENTFS_BIN:-$ROOT/target/release/agentfs}
OUT=${BENCH_OUT:-$PWD/bench-out}
FS=${1:-}
GIT_URL=${BENCH_GIT_URL:-https://github.com/git/git.git}
GIT_TAG=${BENCH_GIT_TAG:-v2.47.0}

SHARD=${BENCH_SHARD:-all}
case "$SHARD" in
  all | many | rest) ;;
  *) fail "BENCH_SHARD must be all, many or rest, not $SHARD" ;;
esac
# Whether this run includes the shard's steps.
in_shard() { [ "$SHARD" = all ] || [ "$SHARD" = "$1" ]; }

# clone and npm run once; bigclose runs RUNS times.
if [ "${BENCH_QUICK:-0}" = 1 ]; then
  NPM_FILES=2000 MANY=5000 BIG=64m RUNS=1 CLOSES=200
else
  NPM_FILES=30000 MANY=100000 BIG=1g RUNS=3 CLOSES=2000
fi

case "$FS" in
  xfs) EXPECT_FREEZE=reflink ;;
  ext4) EXPECT_FREEZE=copy ;;
  dir) [ -n "${2:-}" ] || fail "usage: $0 dir PATH"; EXPECT_FREEZE= ;;
  *) fail "usage: $0 xfs|ext4|dir PATH" ;;
esac

[ -c /dev/fuse ] || fail "/dev/fuse is missing: this benchmark needs FUSE"
sudo -n true 2>/dev/null || fail "this benchmark needs passwordless sudo"
for tool in git jq python3; do
  command -v $tool >/dev/null || fail "$tool is needed"
done
[ -x "$AGENTFS_BIN" ] || fail "no agentfs binary at $AGENTFS_BIN (cargo build --release -p agentfs)"

mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
: >"$OUT/results.jsonl"
: >"$OUT/record.jsonl"

W=$(mktemp -d)
chmod 755 "$W"
LOOP_IMG=
FSDIR=
MOUNT_PID=
MNT=

cleanup() {
  status=$?
  if [ -n "$MOUNT_PID" ] && sudo kill -0 "$MOUNT_PID" 2>/dev/null; then
    sudo kill -TERM "$MOUNT_PID" 2>/dev/null || true
    wait "$MOUNT_PID" 2>/dev/null || true
  fi
  if [ -n "$MNT" ] && mountpoint -q "$MNT"; then sudo umount -l "$MNT" || true; fi
  if [ -n "$LOOP_IMG" ]; then
    sudo umount "$FSDIR" 2>/dev/null || sudo umount -l "$FSDIR" 2>/dev/null || true
    sudo rm -f "$LOOP_IMG"
  fi
  sudo rm -rf "$W"
  exit $status
}
trap cleanup EXIT

now_ns() { date +%s%N; }
seconds_since() { # start-ns
  awk -v a="$1" -v b="$(now_ns)" 'BEGIN { printf "%.3f", (b - a) / 1e9 }'
}

# Appends one result: the JSON object on stdin, tagged.
result() { # workload mode [extra-json]
  local extra=${3:-}
  [ -n "$extra" ] || extra='{}'
  jq -c --arg fs "$FS" --arg w "$1" --arg m "$2" --argjson extra "$extra" \
    '. + {suite: "e2e", fs: $fs, workload: $w, mode: $m} + $extra' |
    tee -a "$OUT/results.jsonl"
}

workload() { sudo python3 "$HERE/bench-workloads.py" "$@"; }

drop_caches() { sync; echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null; }

step "the filesystem under test ($FS)"
if [ "$FS" = dir ]; then
  FSDIR=$2
  mkdir -p "$FSDIR"
  FSDIR=$(cd "$FSDIR" && pwd)
else
  # A sparse image on the runner's largest scratch disk.
  base=${RUNNER_TEMP:-/tmp}
  if [ -d /mnt ] && [ "$(df --output=avail -BG /mnt | tail -1 | tr -dc 0-9)" -ge 30 ]; then
    base=/mnt
  fi
  LOOP_IMG=$base/agentfs-bench-$FS.img
  sudo rm -f "$LOOP_IMG"
  sudo truncate -s 20G "$LOOP_IMG"
  if [ "$FS" = xfs ]; then
    sudo mkfs.xfs -q -f -m reflink=1 "$LOOP_IMG"
  else
    sudo mkfs.ext4 -q -F -E lazy_itable_init=0,lazy_journal_init=0 "$LOOP_IMG"
  fi
  FSDIR=$W/fs
  mkdir -p "$FSDIR"
  sudo mount -o loop "$LOOP_IMG" "$FSDIR"
  sudo chown "$(id -u):$(id -g)" "$FSDIR"
fi
fstype=$(findmnt -no FSTYPE -T "$FSDIR")
echo "measuring $fstype at $FSDIR (image: ${LOOP_IMG:-none})"
if [ "$FS" != dir ]; then
  assert_eq "$FS" "$fstype" "the loop filesystem is $FS"
fi
jq -n \
  --arg fs "$FS" --arg fstype "$fstype" --arg kernel "$(uname -r)" \
  --arg cpu "$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ *//')" \
  --argjson cpus "$(nproc)" \
  --argjson mem_mib "$(awk '/MemTotal/ { print int($2 / 1024) }' /proc/meminfo)" \
  --arg commit "$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)" \
  --arg git "$(git --version)" \
  --arg backing "$(findmnt -no SOURCE -T "${LOOP_IMG:-$FSDIR}" || true)" \
  --argjson quick "${BENCH_QUICK:-0}" --arg shard "$SHARD" \
  '{fs: $fs, shard: $shard, fstype: $fstype, kernel: $kernel, cpu: $cpu, cpus: $cpus,
    mem_mib: $mem_mib, commit: $commit, git: $git, image_on: $backing,
    quick: ($quick == 1)}' | tee "$OUT/env.json"

if in_shard rest; then
  step "the synchronous record step, no FUSE (record_step bench)"
  mkdir -p "$FSDIR/record"
  (
    cd "$ROOT"
    AGENTFS_BENCH_DIR=$FSDIR/record \
      AGENTFS_BENCH_JSON=$OUT/record.jsonl \
      AGENTFS_BENCH_EXPECT_FREEZE=$EXPECT_FREEZE \
      AGENTFS_BENCH_N=${BENCH_N:-500} \
      cargo bench --locked -p agentfs-core --bench record_step
  ) | tee "$OUT/record.md"
  rm -rf "$FSDIR/record"
  pass "record_step froze by ${EXPECT_FREEZE:-whatever the filesystem offers}"
fi

# --- one volume per workload ---------------------------------------------

af() { sudo "$AGENTFS_BIN" --state "$STATE" "$@"; }
sgit() { sudo git --git-dir "$STATE/git" "$@"; }

# Creates and mounts a fresh volume: sets NAME, STATE, BACKING, MNT, LOG.
# EXCLUDE, if set, is a pattern for info/exclude.
mount_volume() { # name [key value]...
  NAME=$1; shift
  STATE=$FSDIR/vols/$NAME/state
  BACKING=$FSDIR/vols/$NAME/backing
  MNT=$W/mnt-$NAME
  LOG=$OUT/mount-$NAME.log
  sudo mkdir -p "$BACKING"
  mkdir -p "$MNT"
  af init --volume "$NAME" --local-only >/dev/null
  # No repack in the middle of a measurement.
  sgit config agentfs.repackInterval off
  while [ $# -gt 0 ]; do sgit config "$1" "$2"; shift 2; done
  if [ -n "${EXCLUDE:-}" ]; then
    sudo mkdir -p "$STATE/git/info"
    echo "$EXCLUDE" | sudo tee -a "$STATE/git/info/exclude" >/dev/null
  fi
  sudo "$AGENTFS_BIN" --state "$STATE" mount "$BACKING" "$MNT" >>"$LOG" 2>&1 &
  MOUNT_PID=$!
  wait_for 30 mountpoint -q "$MNT" || fail "$NAME never mounted"
  wait_for 30 grep -q '^mounted ' "$LOG" || fail "$NAME: no startup line"
}

unmount_volume() {
  sudo kill -TERM "$MOUNT_PID"
  wait "$MOUNT_PID" || fail "$NAME: mount did not exit cleanly"
  MOUNT_PID=
  MNT=
  sudo rm -rf "$FSDIR/vols/$NAME"
}

committed_seq() { # -1 before the first commit: sequence numbers start at 0
  af status | sed -n 's/^committed: through seq \([0-9]*\).*/\1/p' | grep . || echo -1
}

backlog() { af status | sed -n 's/^backlog: *\([0-9]*\).*/\1/p'; }

# Waits for the commit loop to commit every record, and prints a JSON
# object with how long that took and how many records and commits the
# workload made since `committed_seq` printed $1.
drain() { # seq-before commits-before
  local start
  start=$(now_ns)
  local deadline=$((SECONDS + 1800))
  until [ "$(backlog)" = 0 ]; do
    [ $SECONDS -lt $deadline ] || fail "$NAME: the backlog never drained: $(af status)"
    sleep 0.2
  done
  local drain_s
  drain_s=$(seconds_since "$start")
  jq -n --argjson drain_s "$drain_s" \
    --argjson records "$(($(committed_seq) - $1))" \
    --argjson commits "$(($(commits) - $2))" \
    '{drain_s: $drain_s, records: $records, commits: $commits}'
}

commits() {
  sgit rev-list --count "refs/heads/agentfs/$NAME" 2>/dev/null || echo 0
}

# Runs a workload in the mount and records its result with the drain. The
# result line is also left in $LAST for checks. The drain is assigned on
# its own line: inside the argument list its failure would only end the
# command substitution, and the script would go on.
LAST=
measured() { # workload-name workload-args...
  local name=$1; shift
  local seq commits_before json drained
  seq=$(committed_seq)
  commits_before=$(commits)
  drop_caches
  json=$(workload "$@")
  drained=$(drain "$seq" "$commits_before")
  LAST=$(echo "$json" | result "$name" agentfs "$drained")
  echo "$LAST"
}

# Bounds the records a workload of N change boundaries made (design §3.1):
# one per boundary. A file written and closed at once is racy at RELEASE,
# which compares it with the copy FLUSH froze rather than recording it
# again (design §19.2 item 4); a small slack covers the sweep recording
# files still open and a comparison that could not be made.
check_records() { # label boundaries
  local records
  records=$(echo "$LAST" | jq .records)
  local lo=$2 hi=$(($2 + $2 / 100 + 10))
  [ "$records" -ge "$lo" ] && [ "$records" -le "$hi" ] ||
    fail "$1: $records records for $2 boundaries, expected $lo to $hi"
  pass "$1: $records records for $2 boundaries ($(awk -v r="$records" -v n="$2" 'BEGIN { printf "%.2f", r / n }') per boundary)"
}

baseline() { # workload-name workload-args...
  local name=$1; shift
  drop_caches
  workload "$@" | result "$name" baseline
}

# The history at the branch equals the backing tree.
check_history() { # [--exclude NAME]...
  local out
  out=$(workload compare "$STATE/git" "refs/heads/agentfs/$NAME" "$BACKING" "$@") ||
    fail "$NAME: history and backing tree differ: $out"
  pass "$NAME: history equals the backing tree ($(echo "$out" | jq .files) files)"
}

PLAIN=$FSDIR/plain
sudo mkdir -p "$PLAIN"

if in_shard rest; then
  step "close4k: 4 KiB write and close"
  drop_caches
  sudo python3 "$HERE/boundary-latency.py" --json "$PLAIN/lat" "$CLOSES" baseline |
    result close4k baseline
  mount_volume close4k
  drop_caches
  before=$(committed_seq)
  json=$(sudo python3 "$HERE/boundary-latency.py" --json "$MNT/lat" "$CLOSES" agentfs)
  drained=$(drain "$before" 0)
  LAST=$(echo "$json" | result close4k agentfs "$drained")
  echo "$LAST"
  check_records close4k "$CLOSES"
  check_history
  unmount_volume

  step "clone: git clone of git.git $GIT_TAG, agentfs.nested=working-tree"
  SRC=$W/git.git
  sudo git clone -q --bare --depth 1 --branch "$GIT_TAG" "$GIT_URL" "$SRC"
  baseline clone clone "$SRC" "$PLAIN/clone"
  sudo rm -rf "$PLAIN/clone"
  mount_volume clone agentfs.nested working-tree
  measured clone clone "$SRC" "$MNT/clone"
  check_history
  sgit cat-file -e "refs/heads/agentfs/clone:clone/Makefile" ||
    fail "the clone's working tree is not in the history"
  if sgit ls-tree -r --name-only "refs/heads/agentfs/clone" -- "clone/.git" | grep -q .; then
    fail "the clone's git directory is in the history"
  fi
  pass "the working tree is recorded, its git directory is not"
  unmount_volume

  step "npm: $NPM_FILES files into an excluded node_modules/"
  baseline npm npm "$PLAIN/npm/node_modules" --files "$NPM_FILES"
  sudo rm -rf "$PLAIN/npm"
  EXCLUDE=node_modules/ mount_volume npm
  echo '{"name": "app"}' | sudo tee "$MNT/package.json" >/dev/null
  # package.json is recorded; commit it first, so the run's count is its own.
  drained=$(drain -1 0)
  seq_before=$(committed_seq)
  measured npm npm "$MNT/npm/node_modules" --files "$NPM_FILES"
  # Excluded paths are decided at record time: nothing frozen or
  # journaled. The committed sequence alone cannot show that, since a
  # record whose commit changes nothing (one the committer drops) does
  # not move it. So write a recorded marker afterwards: its commit carries
  # its own sequence number, which counts every record journaled before
  # it. The marker makes one record, or two when RELEASE cannot rule out
  # a change its racy close may have missed (docs/performance.md, "Known
  # costs"), and package.json may add a late RELEASE record of its own: at
  # most 4, against thousands if the excluded files were journaled.
  assert_eq 0 "$(echo "$LAST" | jq .records)" "npm: the excluded files made no commits"
  echo 1 | sudo tee "$MNT/npm-marker" >/dev/null
  drained=$(drain "$seq_before" 0)
  made=$(echo "$drained" | jq .records)
  [ "$made" -ge 1 ] && [ "$made" -le 4 ] ||
    fail "npm: $made records journaled with the marker, expected 1 to 4: the excluded files were journaled"
  pass "npm: the excluded files were not journaled ($made records with the marker)"
  n=$(sudo find "$BACKING/npm/node_modules" -type f | wc -l)
  assert_eq "$NPM_FILES" "$n" "npm: every file reached the backing tree"
  if sgit ls-tree -r --name-only refs/heads/agentfs/npm | grep -q node_modules; then
    fail "node_modules/ is in the history"
  fi
  pass "nothing under node_modules/ was recorded"
  check_history --exclude node_modules
  unmount_volume
fi

if in_shard many; then
  step "create and rm -rf: $MANY files"
  baseline create create "$PLAIN/many" "$MANY"
  baseline mvdir mvdir "$PLAIN/many" "$PLAIN/moved"
  baseline rm rm "$PLAIN/moved"
  mount_volume many
  measured create create "$MNT/many" "$MANY"
  check_records create "$MANY"
  n=$(sgit ls-tree -r --name-only refs/heads/agentfs/many | grep -c '^many/' || true)
  assert_eq "$MANY" "$n" "every created file is in the history"
  # A directory rename is one record whatever is beneath it (design §3.1),
  # its reconciliation none here.
  measured mvdir mvdir "$MNT/many" "$MNT/moved"
  assert_eq 1 "$(echo "$LAST" | jq .records)" "mvdir: the rename of $MANY files is one record"
  n=$(sgit ls-tree -r --name-only refs/heads/agentfs/many | grep -c '^moved/' || true)
  assert_eq "$MANY" "$n" "every moved file is in the history at its new path"
  check_history
  measured rm rm "$MNT/moved"
  n=$(sgit ls-tree -r --name-only refs/heads/agentfs/many | grep -c '^moved/' || true)
  assert_eq 0 "$n" "every removal is in the history"
  unmount_volume
fi

if in_shard rest; then
  step "bigclose: close a $BIG file, dirty and fsynced (freeze by ${EXPECT_FREEZE:-either})"
  for _ in $(seq "$RUNS"); do
    workload bigclose "$PLAIN/big.bin" --size "$BIG" --copy | result bigclose-dirty baseline
    workload bigclose "$PLAIN/big.bin" --size "$BIG" --fsync --copy | result bigclose-fsync baseline
  done
  sudo rm -f "$PLAIN/big.bin"
  for variant in dirty fsync; do
    flag=
    [ $variant = fsync ] && flag=--fsync
    mount_volume "big-$variant"
    for _ in $(seq "$RUNS"); do
      measured "bigclose-$variant" bigclose "$MNT/big.bin" --size "$BIG" $flag
    done
    check_history
    unmount_volume
  done
fi

step "done: $(wc -l <"$OUT/results.jsonl") results in $OUT"
