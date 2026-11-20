#!/usr/bin/env bash
# The tree views' tracer, end to end on the built image (docs/trees.md):
# `init` and `mount` in a privileged agentfs container, the
# workload in a second container as 10000:10000 that reaches the mount by
# propagation, as the Docker example does (docs/docker.md).
#
#   ci/e2e-trees-tracer.sh                 steps 1 to 3 below
#   ci/e2e-trees-tracer.sh --figures FS    steps 1 to 4, with the state area
#                                          on a fresh loop filesystem, FS
#                                          ext4 or xfs (sudo)
#   ci/e2e-trees-tracer.sh diagnose        the agentfs container's log and history
#
# 1. `init --volume t` twice gives one id, `created` then `found`;
#    `--volume u` at the same state area exits 1 with every file's bytes
#    unchanged.
# 2. With an operator settings include naming a remote, the mount logs
#    `not built: remote.origin.url (push)` and serves anyway. The workload
#    writes a.txt. Right after its close returns, the root's head
#    holds it (at `files/<aa>/<bb>/a.txt`, as every envelope commit does: docs/trees.md), with `Agentfs-Volume: t`, the volume's id and the pod trailers
#    the mount was given. The workload makes a directory and an executable
#    script and runs it: history holds the script as `100755` and the
#    directory's `dirs/` record. With `*.db -agentfs` and `.env -agentfs` in
#    history.git/info/attributes, the workload's state.db and .env work through
#    the mount and never reach history (docs/trees.md, "Rules and the untracked
#    area").
# 3. agentfs is killed with SIGKILL while the workload writes, and mounts
#    again with `--on-stale-mount recover`, stacked over the stale mount it
#    left: every write the workload saw answered is in history, with its pod
#    trailers, and a restarted workload writes through the new mount;
#    state.db and .env are still there.
# 4. (--figures) The flat workloads of ci/trees-figures.py through the mount,
#    on the image's release build, each against its own deadline: the report
#    is $RUNNER_TEMP/trees-figures-FS.json (docs/performance.md, "Tree views,
#    release build"), a gated figure in ci/trees-budget.json (`create`) that
#    misses its bar fails the script once the rest is checked, any other is a
#    warning, and the root's history holds the last save each workload saw
#    answered.
# 5. (--figures) The view is stopped and mounted again on the state step 4
#    left (about 50,000 files): `restart` is its time to mount, from the
#    container's start to its `mounted` line by their log timestamps, and
#    its peak memory; `long_history` is 2,000 more creates on it. Both are
#    added to the same report and only warn (owner, 2026-11-20).
#
# Needs the agentfs image as $AGENTFS_IMAGE (default agentfs:ci), the fuse
# module loaded, a shared mount under $RUNNER_TEMP (as / is on systemd
# hosts), and sudo (for the cleanup's lazy unmount on the host).

. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/tree-git-test-lib.sh"

IMAGE=${AGENTFS_IMAGE:-agentfs:ci}
WORKLOAD_IMAGE=debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f
NAME=agentfs-trees-tracer
DATA=agentfs-trees-tracer
MOUNT_DIR=${RUNNER_TEMP:-/tmp}/agentfs-trees-tracer
RECEIPTS=${RUNNER_TEMP:-/tmp}/agentfs-trees-tracer-receipts
VIEW=$MOUNT_DIR/root
POD=(-e AGENTFS_NAMESPACE=ns-t -e AGENTFS_POD=pod-t -e AGENTFS_POD_UID=uid-t -e AGENTFS_NODE=node-t)
TRAILERS=("Agentfs-Namespace: ns-t" "Agentfs-Pod: pod-t" "Agentfs-Pod-Uid: uid-t" "Agentfs-Node: node-t")

FS=
if [ "${1:-}" = --figures ]; then
  FS=${2:?--figures needs ext4 or xfs}
  case $FS in ext4|xfs) ;; *) fail "--figures takes ext4 or xfs, not $FS" ;; esac
  # The state area on a fresh loop filesystem of that type, bound into the
  # agentfs container in place of the docker volume.
  LOOP_IMG=${RUNNER_TEMP:-/tmp}/agentfs-trees-tracer-$FS.img
  DATA=${RUNNER_TEMP:-/tmp}/agentfs-trees-tracer-fs
fi

if [ "${1:-}" = diagnose ]; then
  docker logs "$NAME" 2>&1 | tail -n 100 || true
  docker run --rm -v "$DATA:/data" --entrypoint git "$IMAGE" \
    --git-dir /data/state/history.git log --format='%H %s%n%b' root 2>&1 | tail -n 100 || true
  grep -F "$MOUNT_DIR" /proc/self/mountinfo || true
  exit 0
fi

cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  # Every layer: a view stacked over a stale mount leaves two.
  while sudo umount -l "$VIEW" 2>/dev/null; do :; done
  if [ -n "$FS" ]; then
    sudo umount "$DATA" 2>/dev/null || sudo umount -l "$DATA" 2>/dev/null || true
    sudo rm -f "$LOOP_IMG"
  else
    docker volume rm "$DATA" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT
cleanup
mkdir -p "$VIEW" "$RECEIPTS"
rm -f "$RECEIPTS"/*
chmod 0777 "$RECEIPTS"
if [ -n "$FS" ]; then
  # close_1g's 1 GiB needs about 4 GiB while history builds: its journal
  # segments, the pack (the blob and its line map) and the checkout.
  sudo truncate -s 8G "$LOOP_IMG"
  if [ "$FS" = xfs ]; then
    sudo mkfs.xfs -q -f -m reflink=1 "$LOOP_IMG"
  else
    sudo mkfs.ext4 -q -F -E lazy_itable_init=0,lazy_journal_init=0 "$LOOP_IMG"
  fi
  mkdir -p "$DATA"
  sudo mount -o loop "$LOOP_IMG" "$DATA"
  assert_eq "$FS" "$(findmnt -no FSTYPE -T "$DATA")" "the state area is on $FS"
else
  docker volume create "$DATA" >/dev/null
fi

# agentfs on the state volume, with the pod context every step carries.
in_state() { # program args...
  docker run --rm -v "$DATA:/data" "${POD[@]}" --entrypoint "$1" "$IMAGE" "${@:2}"
}
init() { # volume
  in_state agentfs --state /data/state init --volume "$1"
}
in_history() { # git args...
  in_state git --git-dir /data/state/history.git "$@"
}
# History is built behind the calls (docs/trees.md): wait until `rev:path`
# holds `want`, or fail.
built() { # rev:path want what
  wait_for 120 sh -c "[ \"\$(docker run --rm -v '$DATA:/data' --entrypoint git '$IMAGE' --git-dir /data/state/history.git show '$1' 2>/dev/null)\" = '$2' ]" ||
    fail "$3: history never held $1"
}
# Every file under the state area with its SHA-256, in name order.
state_hashes() {
  in_state sh -c 'cd /data/state && find . -type f -print0 | sort -z | xargs -0 sha256sum'
}
# The mount's own line once every view is up (docs/trees.md, "Run it").
mounted() { docker logs "$NAME" 2>&1 | grep -F 'mounted volume t at /agentfs/root' >/dev/null; }
mount_view() { # [mount flags]
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker run -d --name "$NAME" \
    --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
    -v "$DATA:/data" "${POD[@]}" \
    --mount "type=bind,src=$MOUNT_DIR,dst=/agentfs,bind-propagation=rshared" \
    "$IMAGE" --state /data/state mount /agentfs/root --volume t \
    --metrics-output /data/metrics.json "$@" \
    >/dev/null
  # A bound on a hang; step 5's restart reads a long history first.
  wait_for "${MOUNT_WAIT:-60}" mounted || { docker logs "$NAME" >&2; fail 'the tree view did not mount'; }
  pass 'the root is mounted'
}
workload() { # [docker run options] -- command
  local options=()
  while [ "$1" != -- ]; do options+=("$1"); shift; done
  shift
  docker run --rm --user 10000:10000 "${options[@]}" \
    --mount "type=bind,src=$MOUNT_DIR,dst=/agentfs,bind-propagation=rslave" \
    "$WORKLOAD_IMAGE" sh -c "$1"
}
has_trailers() { # message what
  for trailer in "${TRAILERS[@]}"; do
    assert_contains "$1" "$trailer" "$2 carries $trailer"
  done
}

step '1. ensure: one id, and another name refused with nothing written'
first=$(init t)
read -r name id outcome <<<"$first"
assert_eq "t created" "$name $outcome" 'the first init creates'
[[ $id =~ ^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[0-9a-f]{4}-[0-9a-f]{12}$ ]] || fail "not a version 4 id: $id"
assert_eq "t $id found" "$(init t)" 'the second init finds the same id'
before=$(state_hashes)
status=0
init u >/dev/null 2>"$RECEIPTS/refused.log" || status=$?
assert_eq 1 "$status" 'another name exits 1'
assert_contains "$(cat "$RECEIPTS/refused.log")" 'another volume is here' 'and says why'
assert_eq "$before" "$(state_hashes)" 'every file under the state area is unchanged'

step '2. a close returns with its save on record, named'
# The operator's rules, read when the volume opens.
in_state sh -c 'mkdir -p /data/state/history.git/info &&
  printf "*.db -agentfs\n.env -agentfs\n" > /data/state/history.git/info/attributes'
# An operator settings include naming a remote: push is not built on tree
# volumes yet, so the mount says so at boot and serves anyway.
in_state sh -c 'printf "[remote \"origin\"]\n\turl = /nowhere\n" > /data/operator.conf'
in_state agentfs --state /data/state init --volume t --include /data/operator.conf >/dev/null
mount_view
assert_contains "$(docker logs "$NAME" 2>&1)" 'not built: remote.origin.url (push)' \
  'the mount logs the remote it does not push to'
# The root starts owned by whoever ran `init` (root here), mode 0755:
# root gives it to the workload's user through the mount, as with the
# recording engine, and history records the new owner.
docker exec "$NAME" chown 10000:10000 /agentfs/root || fail 'root could not chown the mount root'
pass 'root chowned the mount root to the workload user through the mount'
# The close's release reaches agentfs after close(2) returns; the view takes
# requests in order, so the read that follows sees the release done.
workload -- 'printf hello > /agentfs/root/a.txt && cat /agentfs/root/a.txt >/dev/null'
built "root:files/$(tree_git_fanned a.txt)" hello 'the root holds a.txt once its close returned and history is built'
pass 'the root holds a.txt once its close returned and history is built'
message=$(in_history log -1 --format=%B root)
assert_contains "$message" 'Agentfs-Volume: t' 'the save names its volume'
assert_contains "$message" "Agentfs-Volume-Id: $id" 'and its id'
has_trailers "$message" 'the save'
assert_contains "$(in_history show root:dirs/root)" '"uid":10000' 'history records the chowned mount root'
assert_contains "$(in_history show "root:lines/$(tree_git_fanned a.txt)")" '"uid":10000' \
  "history records a.txt as the workload user's"
# A directory and an executable bit, through the view and in history.
workload -- 'mkdir -p /agentfs/root/tools/bin &&
  printf "#!/bin/sh\necho hi\n" > /agentfs/root/tools/bin/run.sh &&
  chmod +x /agentfs/root/tools/bin/run.sh &&
  [ "$(/agentfs/root/tools/bin/run.sh)" = hi ]' || fail 'the workload could not make and run a script in a directory'
pass 'the workload made a directory and ran an executable script in it'
script=files/$(tree_git_fanned tools/bin/run.sh)
built_mode() { # path want what
  wait_for 120 sh -c "[ \"\$(docker run --rm -v '$DATA:/data' --entrypoint git '$IMAGE' --git-dir /data/state/history.git ls-tree --full-tree root -- '$1' 2>/dev/null | cut -d' ' -f1)\" = '$2' ]" ||
    fail "$3: history never held $1 as $2"
}
built_mode "$script" 100755 'the script is executable in history'
pass 'the script is 100755 in history'
record=$(tree_git_dir_record tools/bin)
built_mode "$record" 100644 'the directory has its record'
assert_contains "$(in_history show "root:$record")" '"path":"tools/bin","kind":"dir"' 'the record is the directory'
pass 'the directory tools/bin has its dirs/ record'
# A database and a secret the rules keep out: written and read through the
# mount, never in history.
workload -- 'printf "SQLite format 3" > /agentfs/root/state.db &&
  printf "TOKEN=x\n" > /agentfs/root/.env &&
  [ "$(cat /agentfs/root/state.db)" = "SQLite format 3" ] &&
  [ "$(cat /agentfs/root/.env)" = "TOKEN=x" ] &&
  ls -a /agentfs/root | grep -x .env >/dev/null' || fail 'the workload could not write state.db and .env'
# A later save is built after them: history up to it holds neither.
workload -- 'printf later > /agentfs/root/later.txt'
built "root:files/$(tree_git_fanned later.txt)" later 'a save after them is built'
names=$(in_history log --all --name-only --format=)
if grep -E '/(state\.db|\.env)$' <<<"$names"; then fail 'an untracked file reached history'; fi
pass 'state.db and .env work through the mount and are not in history'

step '3. SIGKILL mid-stream: every answered write is in history'
workload -v "$RECEIPTS:/receipts" -d -- '
  n=0
  while :; do
    n=$((n + 1))
    printf "%s\n" "$n" > /agentfs/root/f$n || exit 0
    echo "$n" >> /receipts/answered
  done' >/dev/null
wait_for 60 test -s "$RECEIPTS/answered" || fail 'the workload wrote nothing'
wait_for 60 sh -c "[ \$(wc -l < '$RECEIPTS/answered') -ge 20 ]" || fail 'the workload stalled'
docker kill -s KILL "$NAME" >/dev/null
docker wait "$NAME" >/dev/null || true
answered=$(wc -l <"$RECEIPTS/answered")
[ "$answered" -ge 20 ] || fail "only $answered writes answered"
# The killed daemon left its mount stale: the restart stacks over it.
mount_view --on-stale-mount recover
assert_contains "$(docker logs "$NAME" 2>&1)" \
  'stacked over a stale mount at /agentfs/root left by a crashed daemon' \
  'the restart stacks over the stale mount'
missing=$(docker run --rm -i -v "$DATA:/data" --entrypoint bash "$IMAGE" -c "$(declare -f tree_git_fanned)"'
  while read -r n; do
    path=files/$(tree_git_fanned "f$n")
    [ "$(git --git-dir /data/state/history.git show "root:$path" 2>/dev/null)" = "$n" ] || echo "f$n"
  done' <"$RECEIPTS/answered")
assert_eq '' "$missing" "all $answered answered writes are in history"
unnamed=$(in_history log --format='%H%n%(trailers:key=Agentfs-Pod,valueonly)%n--' root |
  awk '/^--$/ { if (!pod) print sha; sha = ""; pod = 0; next } !sha { sha = $0; next } $0 == "pod-t" { pod = 1 }')
assert_eq '' "$unnamed" 'every commit on the root carries its pod trailers'
workload -- 'printf again > /agentfs/root/after.txt && cat /agentfs/root/after.txt >/dev/null'
built "root:files/$(tree_git_fanned after.txt)" again 'a restarted workload writes through the new mount'
pass 'a restarted workload writes through the new mount'
workload -- '[ "$(cat /agentfs/root/state.db)" = "SQLite format 3" ] &&
  [ "$(cat /agentfs/root/.env)" = "TOKEN=x" ]' || fail 'state.db or .env did not survive the restart'
pass 'state.db and .env survive the kill and restart'
has_trailers "$(in_history log -1 --format=%B root)" 'its save'

if [ -n "$FS" ]; then
  step "4. figures: the flat workloads on the release build, on $FS"
  # The gate itself: a create short of its bar fails the run.
  if python3 "$(dirname "$0")/trees-figures.py" --self-test --budget "$(dirname "$0")/trees-budget.json" >/dev/null; then
    fail 'a create short of its bar would not fail the gate'
  fi
  pass 'a create short of its bar fails the gate'
  # And only create's: every other bar missed only warns (owner, 2026-11-20).
  python3 "$(dirname "$0")/trees-figures.py" --self-test-warn --budget "$(dirname "$0")/trees-budget.json" >/dev/null ||
    fail 'a bar other than create would fail the run'
  pass 'every other bar missed only warns'
  report=${RUNNER_TEMP:-/tmp}/trees-figures-$FS.json
  # As root, which the view lets through (docs/trees.md), so the runner's
  # files need no other owner, and so a stuck view's connection can be
  # aborted; the stat after each save is the timed end.
  # Exit 1 when a gated figure missed its bar: the rest is checked first.
  gated=0
  sudo python3 "$(dirname "$0")/trees-figures.py" --view "$VIEW" --filesystem "$FS" \
    --metrics "$DATA/metrics.json" --drain-cap 180 \
    --view-pid "$(docker inspect --format '{{.State.Pid}}' "$NAME")" \
    --image "$(docker image inspect --format '{{.Id}}' "$IMAGE")" \
    --budget "$(dirname "$0")/trees-budget.json" --output "$report" || gated=$?
  cat "$report"
  figure() { # workload field
    python3 -c 'import json, sys
w = {w["name"]: w for w in json.load(open(sys.argv[1]))["workloads"]}[sys.argv[2]]
print(json.dumps(w.get(sys.argv[3], 0)))' "$report" "$1" "$2"
  }
  listed() { python3 -c 'import json, sys; print(*(w["name"] for w in json.load(open(sys.argv[1]))["workloads"]))' "$report"; }
  assert_eq "close_4k create memory_create close_64m close_1g write_1g history_1g memory_1g close_frag write_1m" \
    "$(listed)" 'the report lists every workload'
  # A save each workload saw answered is in history once the built point reached
  # it (drain_s); a save the deadline caught may have landed after it, so history
  # may hold one more. A workload whose history did not drain within the cap is
  # a figure, reported as such.
  drained() { [ "$(figure "$1" drain_s)" != null ] || { echo "::warning title=tree views, $FS::$1's history did not drain within the cap"; false; }; }
  n=$(figure close_4k count)
  if [ "$n" -gt 0 ] && drained close_4k; then
    held=$(in_history show "root:files/$(tree_git_fanned fig-close4k)" | sed -n 1p)
    [ "$held" -ge "$n" ] || fail "history holds 4 KiB save $held, the workload saw $n answered"
    pass "4 KiB save $n is in history"
  fi
  n=$(figure create count)
  if [ "$n" -gt 0 ] && drained create; then
    assert_eq "$n" "$(in_history show "root:files/$(tree_git_fanned "fig-create-$n")")" "create $n is in history"
  fi
  if [ "$(figure close_64m finished)" = true ] && drained close_64m; then
    assert_eq 67108864 "$(in_history cat-file -s "root:files/$(tree_git_fanned fig-64m)")" 'the 64 MiB save is in history'
  fi
  if [ "$(figure close_1g finished)" = true ] && drained close_1g; then
    assert_eq 1073741824 "$(in_history cat-file -s "root:files/$(tree_git_fanned fig-1g)")" 'the 1 GiB save is in history'
  fi
  closed() { case $(figure "$1" close_s) in 0 | null) false ;; esac; }
  if closed close_frag && drained close_frag; then
    assert_eq 67108864 "$(in_history cat-file -s "root:files/$(tree_git_fanned fig-frag)")" 'the fragmented 64 MiB save is in history'
  fi
  if closed write_1m && drained write_1m; then
    assert_eq 64000000 "$(in_history cat-file -s "root:files/$(tree_git_fanned fig-appends)")" 'the million appends are in history'
  fi

  step "5. restart: the view mounted again on the figures' state, then a long history"
  # A clean stop builds every record appended; the restart then reads it all.
  docker stop -t 600 "$NAME" >/dev/null
  MOUNT_WAIT=600 mount_view --on-stale-mount recover
  # From the container's start to the view's `mounted` line, by their log
  # timestamps (RFC 3339, nanoseconds).
  mount_s=$(python3 - "$(docker inspect --format '{{.State.StartedAt}}' "$NAME")" \
    "$(docker logs -t "$NAME" 2>&1 | grep -F 'mounted volume t at /agentfs/root' | sed -n 1p | cut -d' ' -f1)" <<'PY'
import datetime, sys
def at(stamp):
    whole, _, fraction = stamp.rstrip("Z").partition(".")
    seconds = datetime.datetime.fromisoformat(whole).replace(tzinfo=datetime.timezone.utc).timestamp()
    return seconds + float("0." + (fraction or "0"))
print(round(at(sys.argv[2]) - at(sys.argv[1]), 3))
PY
)
  restarted=0
  sudo python3 "$(dirname "$0")/trees-figures.py" --view "$VIEW" --filesystem "$FS" \
    --metrics "$DATA/metrics.json" --drain-cap 180 \
    --view-pid "$(docker inspect --format '{{.State.Pid}}' "$NAME")" \
    --image "$(docker image inspect --format '{{.Id}}' "$IMAGE")" \
    --budget "$(dirname "$0")/trees-budget.json" \
    --baseline "$report" --restart-mount-s "$mount_s" --output "$report" || restarted=$?
  cat "$report"
  assert_eq "close_4k create memory_create close_64m close_1g write_1g history_1g memory_1g close_frag write_1m restart long_history" \
    "$(listed)" 'the report lists the restart and the long history'
  n=$(figure long_history count)
  if [ "$n" -gt 0 ] && drained long_history; then
    assert_eq "$n" "$(in_history show "root:files/$(tree_git_fanned "fig-history-$n")")" "long history create $n is in history"
  fi
  [ "$restarted" = 0 ] || fail "a gated figure missed its bar after the restart on $FS (trees-figures.py exited $restarted)"
  [ "$gated" = 0 ] || fail "a gated figure missed its bar on $FS (trees-figures.py exited $gated)"
fi
pass 'tree views tracer'
