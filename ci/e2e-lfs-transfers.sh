#!/usr/bin/env bash
# End-to-end tests of LFS transfers (design §7), after ci/e2e-lfs.sh and
# against the same servers: git-lfs's lfs-test-server behind nginx, whose
# LFS host (127.0.0.1:8081) takes fault rules from `lfs_faults` (ci/lib.sh)
# to throttle or fail chosen requests:
#
# 12. lfs.concurrenttransfers: downloads overlap with 4, never with 1;
#     many objects uploaded at once all reach the server, and git-lfs
#     pulls them back.
# 13. uploads resume by object: one object's PUT fails, the others reach
#     the server and the branch does not move; the next push sends only
#     that object;
# 14. downloads resume by byte: a download killed halfway leaves a partial
#     in <state>/lfs/incomplete/, and the next one asks for the rest with
#     Range (lfs-test-server answers 206).
# 16. agentfs lfs fetch: the head's objects, a revision's, --all, and a
#     dry run; a restore after a fetch downloads nothing.
#
# Needs ci/git-http-server.sh started with GIT_SERVER_LFS=1, ci/lfs-server.sh,
# and git-lfs.

. "$(dirname "$0")/lib.sh"

: "${GIT_SERVER_REPOS:?start ci/git-http-server.sh with GIT_SERVER_LFS=1 first}"
: "${GIT_SERVER_HTPASSWD:?}" "${GIT_SERVER_LFS_HTPASSWD:?}" "${GIT_HTTP_BASE:?}"
: "${LFS_SEPARATE_BASE:?}" "${GIT_SERVER_ACCESS_LOG:?}" "${LFS_SERVER_DIR:?start ci/lfs-server.sh first}"
: "${GIT_SERVER_LFS_FAULTS:?}" "${GIT_SERVER_LFS_RANGE_LOG:?}"
command -v git-lfs >/dev/null || fail "git-lfs is not installed"
LOG=$GIT_SERVER_ACCESS_LOG

server_repo() { # name
  git init -q --bare "$GIT_SERVER_REPOS/$1.git"
  echo "$GIT_SERVER_REPOS/$1.git"
}

set_password() { # htpasswd user password
  local others
  others=$(grep -v "^$2:" "$1" || true)
  { [ -n "$others" ] && echo "$others"; echo "$2:$(openssl passwd -apr1 "$3")"; } >"$1"
}

log_since() { tail -n +"$(($1 + 1))" "$LOG"; }
log_lines() { wc -l <"$LOG"; }

# The LFS objects lfs-test-server holds for this run's content directory.
on_lfs_host() { # oid
  [ -f "$LFS_SERVER_DIR/separate/content/${1:0:2}/${1:2:2}/${1:4}" ]
}

oid_of() { # path in the volume's head
  git --git-dir "$STATE/git" show "agentfs/$VOLNAME:$1" | sed -n 2p | sed 's/^oid sha256://'
}

# A volume whose remote `sep` pushes to the git host as alice and to the
# separate LFS host as lfsbob, under /acme/<name>.
lfs_volume() { # name
  new_volume "$1"
  VOLNAME=$1
  volume_config agentfs.lfsThreshold 1k
  REPO=$(server_repo "$1")
  volume_config remote.sep.url "$GIT_HTTP_BASE/$1.git"
  volume_config remote.sep.agentfsCredentials "$CREDS/alice"
  volume_config remote.sep.lfsurl "$LFS_SEPARATE_BASE/acme/$1"
  volume_config remote.sep.agentfsLfsCredentials "$CREDS/lfsbob"
}

# How many GETs of LFS objects on the LFS host, logged since line $1,
# started while an earlier one was still running.
overlapping_gets() { # since-line
  log_since "$1" | awk '$2 == 8081 && $4 == "\"GET" && $5 ~ /\/objects\// {
      printf "%.3f %.3f\n", $1 - $NF, $1 }' | sort -n |
    awk 'NR > 1 && $1 < end { n++ } { if ($2 > end) end = $2 } END { print n + 0 }'
}

set_password "$GIT_SERVER_HTPASSWD" alice pw1
set_password "$GIT_SERVER_LFS_HTPASSWD" lfsbob lfspw
CREDS=$(mktemp -d)
printf 'username=alice\npassword=pw1\n' >"$CREDS/alice"
printf 'username=lfsbob\npassword=lfspw\n' >"$CREDS/lfsbob"
lfs_faults ""

step "12. lfs.concurrenttransfers: concurrent downloads and uploads"
lfs_volume conc
mkdir -p "$TREE/c"
for i in 1 2 3 4; do head -c $((2 << 20)) /dev/urandom >"$TREE/c/$i.bin"; done
agentfs record c/1.bin c/2.bin c/3.bin c/4.bin
agentfs push --remote sep
SOURCE=$(local_head conc)
cp -r "$TREE/c" "$VOL/c.orig"
# Each object's GET takes about 2 s at 1 MiB/s.
lfs_faults "$(lfs_location '^/acme/conc/objects/' 'limit_rate 1m;')"
for n in 4 1; do
  volume_config lfs.concurrenttransfers "$n"
  rm -rf "$TREE/c" "$STATE/lfs/objects"
  agentfs record c/1.bin c/2.bin c/3.bin c/4.bin
  before=$(log_lines)
  agentfs restore c "$SOURCE"
  diff -r "$TREE/c" "$VOL/c.orig" >/dev/null || fail "the restored objects differ"
  overlaps=$(overlapping_gets "$before")
  gets=$(log_since "$before" | awk '$2 == 8081 && $4 == "\"GET"' | wc -l)
  assert_eq 4 "$gets" "four objects downloaded with lfs.concurrenttransfers $n"
  if [ "$n" = 1 ]; then
    assert_eq 0 "$overlaps" "one at a time with lfs.concurrenttransfers 1"
  else
    [ "$overlaps" -ge 2 ] || fail "only $overlaps overlapping downloads with lfs.concurrenttransfers $n"
    pass "$overlaps downloads overlapped with lfs.concurrenttransfers $n"
  fi
done
lfs_faults ""
volume_config lfs.concurrenttransfers 8
mkdir -p "$TREE/many"
paths=()
for i in $(seq 20); do
  head -c 3000 /dev/urandom >"$TREE/many/$i.bin"
  paths+=("many/$i.bin")
done
agentfs record "${paths[@]}"
before=$(log_lines)
agentfs push --remote sep
assert_eq 20 "$(log_since "$before" | awk '$2 == 8081 && $4 == "\"PUT"' | wc -l)" "20 objects uploaded"
for path in "${paths[@]}"; do
  on_lfs_host "$(oid_of "$path")" || fail "$path is not on the LFS host"
done
pass "every object is on the LFS host"
CLONE=$VOL/clone
GIT_LFS_SKIP_SMUDGE=1 git clone -q -b agentfs/conc "http://alice:pw1@localhost:8080/git/conc.git" "$CLONE"
git -C "$CLONE" lfs install --local >/dev/null
git -C "$CLONE" config lfs.url "http://lfsbob:lfspw@127.0.0.1:8081/acme/conc"
git -C "$CLONE" lfs pull
diff -r "$CLONE/many" "$TREE/many" >/dev/null || fail "git lfs pull gives other content"
pass "git lfs pull reproduces the 20 objects"

step "13. uploads resume by object"
lfs_volume resume
mkdir -p "$TREE/r"
for i in 1 2 3 4 5; do head -c 5000 /dev/urandom >"$TREE/r/$i.bin"; done
agentfs record r/1.bin r/2.bin r/3.bin r/4.bin r/5.bin
FAILING=$(oid_of r/3.bin)
lfs_faults "$(lfs_location "^/acme/resume/objects/$FAILING\$" 'if ($request_method = PUT) { return 503; }')"
set +e
out=$(agentfs push --remote sep 2>&1); code=$?
set -e
assert_eq 1 "$code" "the push fails"
assert_contains "$out" "sep: failed (unreachable, stage lfs)" "a 503 for one object is unreachable at stage lfs"
assert_eq "" "$(remote_ref "$REPO" refs/heads/agentfs/resume)" "the branch was not pushed"
on_lfs_host "$FAILING" && fail "the failing object reached the server"
for i in 1 2 4 5; do
  on_lfs_host "$(oid_of "r/$i.bin")" || fail "r/$i.bin did not reach the server"
done
pass "the other four objects are on the server"
lfs_faults ""
before=$(log_lines)
agentfs push --remote sep
puts=$(log_since "$before" | awk '$2 == 8081 && $4 == "\"PUT"')
assert_eq 1 "$(grep -c . <<<"$puts")" "the next push uploads one object"
assert_contains "$puts" "$FAILING" "the one that failed"
assert_eq "$(local_head resume)" "$(remote_ref "$REPO" refs/heads/agentfs/resume)" "the branch is pushed"

step "14. downloads resume by byte"
head -c $((4 << 20)) /dev/urandom >"$TREE/huge.bin"
cp "$TREE/huge.bin" "$VOL/huge.orig"
agentfs record huge.bin
agentfs push --remote sep
SOURCE=$(local_head resume)
HUGE=$(oid_of huge.bin)
rm "$TREE/huge.bin"
agentfs record huge.bin
rm -rf "$STATE/lfs/objects"
lfs_faults "$(lfs_location "^/acme/resume/objects/$HUGE\$" 'limit_rate 256k;')"
PARTIAL=$STATE/lfs/incomplete/$HUGE
agentfs_start "$VOL/restore.log" restore huge.bin "$SOURCE"
wait_for 60 sh -c "[ \"\$(stat -c %s '$PARTIAL' 2>/dev/null || echo 0)\" -gt $((512 << 10)) ]" ||
  fail "no partial download grew past 512 KiB: $(cat "$VOL/restore.log")"
kill -9 "$AGENTFS_PID"
wait "$AGENTFS_PID" 2>/dev/null || true
held=$(stat -c %s "$PARTIAL")
[ "$held" -lt $((4 << 20)) ] || fail "the download finished before it was killed"
pass "a killed download left $held bytes in lfs/incomplete"
[ -e "$TREE/huge.bin" ] && fail "the killed restore wrote huge.bin"
lfs_faults ""
before=$(wc -l <"$GIT_SERVER_LFS_RANGE_LOG")
agentfs restore huge.bin "$SOURCE"
cmp "$TREE/huge.bin" "$VOL/huge.orig" && pass "the restore completed with the right content"
ranged=$(tail -n +"$((before + 1))" "$GIT_SERVER_LFS_RANGE_LOG" | grep "objects/$HUGE" || true)
held_now=$(grep -o '"bytes=[0-9]*-"' <<<"$ranged" | head -n1 | tr -dc 0-9)
[ -n "$held_now" ] && [ "$held_now" -ge "$held" ] || fail "no Range request resumed the download: $ranged"
assert_contains "$ranged" " 206 \"bytes=$held_now-\"" "lfs-test-server answered the Range with 206"
assert_eq "$HUGE" "$(sha256sum "$STATE/lfs/objects/${HUGE:0:2}/${HUGE:2:2}/$HUGE" | cut -d' ' -f1)" "the cached object's hash"
assert_eq 0 "$(find "$STATE/lfs/incomplete" -type f | wc -l)" "lfs/incomplete is empty"

step "16. agentfs lfs fetch"
lfs_volume fetchv
gets_since() { log_since "$1" | awk '$2 == 8081 && $4 == "\"GET" && $5 ~ /\/objects\//' | wc -l; }
head -c 3000 /dev/urandom >"$TREE/a.bin"
agentfs record a.bin
FIRST=$(local_head fetchv)
cp "$TREE/a.bin" "$VOL/a.first"
head -c 3000 /dev/urandom >"$TREE/a.bin"
head -c 3000 /dev/urandom >"$TREE/b.bin"
agentfs record a.bin b.bin
agentfs push --remote sep
rm -rf "$STATE/lfs/objects"
before=$(log_lines)
out=$(agentfs lfs fetch --dry-run)
assert_contains "$out" "would fetch 2 object(s), 6000 byte(s)" "a dry run counts the head's objects"
assert_eq 0 "$(gets_since "$before")" "a dry run downloads nothing"
out=$(agentfs lfs fetch)
assert_contains "$out" "sep: fetched 2 object(s)" "the head's two objects"
assert_eq 2 "$(gets_since "$before")" "two downloads"
before=$(log_lines)
out=$(agentfs lfs fetch "$FIRST")
assert_contains "$out" "sep: fetched 1 object(s)" "the first commit's object"
assert_eq 1 "$(gets_since "$before")" "one download for the revision"
rm -rf "$STATE/lfs/objects"
before=$(log_lines)
out=$(agentfs lfs fetch --all)
assert_contains "$out" "3 object(s), 9000 byte(s) referenced" "--all: every object of the branch"
assert_eq 3 "$(gets_since "$before")" "three downloads"
rm "$TREE/a.bin"
agentfs record a.bin
before=$(log_lines)
agentfs restore a.bin "$FIRST"
cmp "$TREE/a.bin" "$VOL/a.first" && pass "restored from the fetched cache"
assert_eq 0 "$(gets_since "$before")" "the restore downloaded nothing"

step "all LFS transfer checks passed"
