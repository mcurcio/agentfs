#!/usr/bin/env bash
# The tree views' tracer, end to end on the built image (docs/trees.md):
# `trees init` and `trees mount` in a privileged agentfs container, the
# workload in a second container as 10000:10000 that reaches the mount by
# propagation, as the Docker example does (docs/docker.md).
#
#   ci/e2e-trees-tracer.sh                 steps 1 to 3 below
#   ci/e2e-trees-tracer.sh --figures FS    steps 1 to 4, with the state area
#                                          on a fresh loop filesystem, FS
#                                          ext4 or xfs (sudo)
#   ci/e2e-trees-tracer.sh diagnose        the agentfs container's log and history
#
# 1. `trees init --volume t` twice gives one id, `created` then `found`;
#    `--volume u` at the same state area exits 1 with every file's bytes
#    unchanged.
# 2. The workload writes a.txt. Right after its close returns, the root's head
#    holds it (under `files/`, as every envelope commit does), with `Agentfs-Volume: t`, the volume's id and the pod trailers
#    the mount was given.
# 3. agentfs is killed with SIGKILL while the workload writes, the stale mount
#    is lazily unmounted and agentfs mounts again: every write the workload saw
#    answered is in history, with its pod trailers, and a restarted workload
#    writes through the new mount.
# 4. (--figures) The flat workloads of ci/trees-figures.py through the mount,
#    on the image's release build, each against its own deadline: the report
#    is $RUNNER_TEMP/trees-figures-FS.json (docs/performance.md, "Tree views,
#    release build"), a missed bar in ci/trees-budget.json is a warning, and
#    the root's history holds the last save each workload saw answered.
#
# Needs the agentfs image as $AGENTFS_IMAGE (default agentfs:ci), the fuse
# module loaded, a shared mount under $RUNNER_TEMP (as / is on systemd
# hosts), and sudo (for the lazy unmount on the host).

. "$(dirname "$0")/lib.sh"

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
  sudo umount -l "$VIEW" 2>/dev/null || true
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
  sudo truncate -s 4G "$LOOP_IMG"
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
  in_state agentfs --state /data/state trees init --volume "$1"
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
mounted() { grep -qF " $VIEW " /proc/self/mountinfo; }
mount_view() {
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker run -d --name "$NAME" \
    --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
    -v "$DATA:/data" "${POD[@]}" \
    --mount "type=bind,src=$MOUNT_DIR,dst=/agentfs,bind-propagation=rshared" \
    "$IMAGE" --state /data/state trees mount /agentfs/root --volume t --owner 10000:10000 \
    --metrics-output /data/metrics.json \
    >/dev/null
  wait_for 60 mounted || { docker logs "$NAME" >&2; fail 'the tree view did not mount'; }
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
mount_view
# The close's release reaches agentfs after close(2) returns; the view takes
# requests in order, so the read that follows sees the release done.
workload -- 'printf hello > /agentfs/root/a.txt && cat /agentfs/root/a.txt >/dev/null'
built root:files/a.txt hello 'the root holds a.txt once its close returned and history is built'
pass 'the root holds a.txt once its close returned and history is built'
message=$(in_history log -1 --format=%B root)
assert_contains "$message" 'Agentfs-Volume: t' 'the save names its volume'
assert_contains "$message" "Agentfs-Volume-Id: $id" 'and its id'
has_trailers "$message" 'the save'

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
sudo umount -l "$VIEW"
answered=$(wc -l <"$RECEIPTS/answered")
[ "$answered" -ge 20 ] || fail "only $answered writes answered"
mount_view
missing=$(docker run --rm -i -v "$DATA:/data" --entrypoint sh "$IMAGE" -c '
  while read -r n; do
    [ "$(git --git-dir /data/state/history.git show "root:files/f$n" 2>/dev/null)" = "$n" ] || echo "f$n"
  done' <"$RECEIPTS/answered")
assert_eq '' "$missing" "all $answered answered writes are in history"
unnamed=$(in_history log --format='%H%n%(trailers:key=Agentfs-Pod,valueonly)%n--' root |
  awk '/^--$/ { if (!pod) print sha; sha = ""; pod = 0; next } !sha { sha = $0; next } $0 == "pod-t" { pod = 1 }')
assert_eq '' "$unnamed" 'every commit on the root carries its pod trailers'
workload -- 'printf again > /agentfs/root/after.txt && cat /agentfs/root/after.txt >/dev/null'
built root:files/after.txt again 'a restarted workload writes through the new mount'
pass 'a restarted workload writes through the new mount'
has_trailers "$(in_history log -1 --format=%B root)" 'its save'

if [ -n "$FS" ]; then
  step "4. figures: the flat workloads on the release build, on $FS"
  report=${RUNNER_TEMP:-/tmp}/trees-figures-$FS.json
  # As root, which the view lets through (docs/trees.md), so the runner's
  # files need no other owner, and so a stuck view's connection can be
  # aborted; the stat after each save is the timed end.
  sudo python3 "$(dirname "$0")/trees-figures.py" --view "$VIEW" --filesystem "$FS" \
    --metrics "$DATA/metrics.json" \
    --image "$(docker image inspect --format '{{.Id}}' "$IMAGE")" \
    --budget "$(dirname "$0")/trees-budget.json" --output "$report"
  cat "$report"
  figure() { # workload field
    python3 -c 'import json, sys
w = {w["name"]: w for w in json.load(open(sys.argv[1]))["workloads"]}[sys.argv[2]]
print(json.dumps(w.get(sys.argv[3], 0)))' "$report" "$1" "$2"
  }
  assert_eq "close_4k create close_64m" \
    "$(python3 -c 'import json, sys; print(*(w["name"] for w in json.load(open(sys.argv[1]))["workloads"]))' "$report")" \
    'the report lists every workload'
  # A save each workload saw answered is in history; a save the deadline
  # caught may have landed after it, so history may hold one more.
  n=$(figure close_4k count)
  if [ "$n" -gt 0 ]; then
    held=$(in_history show root:files/fig-close4k | sed -n 1p)
    [ "$held" -ge "$n" ] || fail "history holds 4 KiB save $held, the workload saw $n answered"
    pass "4 KiB save $n is in history"
  fi
  n=$(figure create count)
  if [ "$n" -gt 0 ]; then
    assert_eq "$n" "$(in_history show "root:files/fig-create-$n")" "create $n is in history"
  fi
  if [ "$(figure close_64m finished)" = true ]; then
    assert_eq 67108864 "$(in_history cat-file -s root:files/fig-64m)" 'the 64 MiB save is in history'
  fi
fi
pass 'tree views tracer'
