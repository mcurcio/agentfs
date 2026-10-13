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
# 15. remote.<name>.agentfsMaxLfsObjectSize: a file over the smallest
#     limit is refused when recorded; an object over a remote's own limit
#     fails its push as too_large at stage lfs, with no upload; a bad value
#     is a misconfiguration;
# 16. agentfs lfs fetch: the head's objects, a revision's, --all, and a
#     dry run; a restore after a fetch downloads nothing;
# 17. agentfs lfs prune: only objects lfs-test-server and a file:// remote
#     both hold are deleted; a restore downloads a pruned object again; a
#     remote added after the prune needs `lfs fetch --all` before its
#     push; a misconfigured remote refuses the prune;
# 18. proxies (with ci/proxy-server.sh): remote.<name>.proxy carries the git
#     push and the LFS uploads and downloads; a dead http.proxy fails at
#     stage lfs as unreachable, and an empty remote.<name>.proxy bypasses
#     it; with E2E_HTTPS=1, https git and LFS servers through CONNECT, and
#     an untrusted certificate still refused through the proxy.
#
# Needs ci/git-http-server.sh started with GIT_SERVER_LFS=1, ci/lfs-server.sh,
# ci/proxy-server.sh (PROXY_URL, PROXY_LOG) and git-lfs.

. "$(dirname "$0")/lib.sh"

: "${GIT_SERVER_REPOS:?start ci/git-http-server.sh with GIT_SERVER_LFS=1 first}"
: "${GIT_SERVER_HTPASSWD:?}" "${GIT_SERVER_LFS_HTPASSWD:?}" "${GIT_HTTP_BASE:?}"
: "${LFS_SEPARATE_BASE:?}" "${GIT_SERVER_ACCESS_LOG:?}" "${LFS_SERVER_DIR:?start ci/lfs-server.sh first}"
: "${GIT_SERVER_LFS_FAULTS:?}" "${GIT_SERVER_LFS_RANGE_LOG:?}"
: "${PROXY_URL:?start ci/proxy-server.sh first}" "${PROXY_LOG:?}"
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
[ ${#FAILING} = 64 ] || fail "r/3.bin is not a pointer: '$FAILING'"
echo "failing object: $FAILING"
# Only uploads touch the object during a push: answer every request for
# it with 503.
lfs_faults "$(lfs_location "^/acme/resume/objects/$FAILING" 'return 503;')"
assert_eq 503 "$(curl -s -o /dev/null -w '%{http_code}' -X PUT -u lfsbob:lfspw --data x \
  "http://127.0.0.1:8081/acme/resume/objects/$FAILING")" "nginx refuses that object's PUT"
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
lfs_faults "$(lfs_location "^/acme/resume/objects/$HUGE" 'limit_rate 256k;')"
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

step "15. agentfsMaxLfsObjectSize"
lfs_volume limits
volume_config remote.sep.agentfsMaxLfsObjectSize 4k
head -c 6144 /dev/urandom >"$TREE/six.bin"
head -c 3072 /dev/urandom >"$TREE/three.bin"
out=$(agentfs record six.bin three.bin 2>&1)
assert_contains "$out" "path refused at record: six.bin" "a 6 KiB LFS file is refused"
assert_contains "$out" "LFS object limit of remote sep" "naming the limit"
git --git-dir "$STATE/git" cat-file -e agentfs/limits:six.bin 2>/dev/null && fail "six.bin is in history"
oid_of three.bin >/dev/null && pass "three.bin is in history as LFS"
agentfs push --remote sep
# Another remote with a lower limit, set after three.bin was committed.
LOW=$VOL/low.git
git init -q --bare "$LOW"
volume_config remote.low.url "file://$LOW"
volume_config remote.low.agentfsMaxLfsObjectSize 2k
set +e
out=$(agentfs push --remote low 2>&1); code=$?
set -e
assert_eq 1 "$code" "the push to the lower limit fails"
assert_contains "$out" "low: failed (too_large, stage lfs)" "as too_large at stage lfs"
assert_contains "$out" "three.bin" "naming the path"
[ -d "$LOW/lfs" ] && fail "something was uploaded to the low remote"
assert_eq "" "$(remote_ref "$LOW" refs/heads/agentfs/limits)" "nothing pushed to it"
volume_config remote.low.agentfsMaxLfsObjectSize 0
assert_contains "$(agentfs status)" "low: misconfigured, not pushed: remote.low.agentfsMaxLfsObjectSize" \
  "a zero limit is a misconfiguration"
git --git-dir "$STATE/git" config --remove-section remote.low

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

step "17. agentfs lfs prune"
FILE_REMOTE=$VOL/file.git
git init -q --bare "$FILE_REMOTE"
volume_config remote.file.url "file://$FILE_REMOTE"
agentfs push --remote file
B=$(oid_of b.bin)
A=$(oid_of a.bin)
cached() { [ -f "$STATE/lfs/objects/${1:0:2}/${1:2:2}/$1" ]; }
# The file remote loses b.bin's object.
rm "$FILE_REMOTE/lfs/objects/${B:0:2}/${B:2:2}/$B"
cached "$A" && cached "$B" || fail "the objects should be cached before the prune"
out=$(agentfs lfs prune --dry-run)
echo "$out"
assert_contains "$out" "file lacks 1" "the dry run names the remote that lacks an object"
assert_contains "$out" "sep (upload): 3 confirmed" "lfs-test-server confirms all three"
cached "$A" || fail "the dry run deleted an object"
out=$(agentfs lfs prune)
assert_contains "$out" "pruned 2 object(s)" "two objects pruned"
cached "$A" && fail "a.bin's object is still cached"
cached "$B" || fail "b.bin's object was pruned though the file remote lacks it"
pass "only what both remotes hold was pruned"
rm "$TREE/a.bin"
agentfs record a.bin
agentfs restore a.bin agentfs/fetchv~1
cmp "$TREE/a.bin" "$VOL/a.first" || fail "the restored a.bin differs"
cached "$A" || fail "the restore did not download the pruned object"
pass "a restore downloads a pruned object again"
agentfs lfs prune >/dev/null
FRESH=$VOL/fresh.git
git init -q --bare "$FRESH"
volume_config remote.fresh.url "file://$FRESH"
set +e
out=$(agentfs push --remote fresh 2>&1); code=$?
set -e
assert_eq 1 "$code" "the fresh remote's push fails"
assert_contains "$out" "fresh: failed (local, stage lfs)" "at stage lfs, as local"
assert_contains "$out" "agentfs lfs fetch --all" "with the hint"
agentfs lfs fetch --all
agentfs push --remote fresh
assert_eq "$(local_head fetchv)" "$(remote_ref "$FRESH" refs/heads/agentfs/fetchv)" "after lfs fetch --all it pushes"
volume_config remote.broken.url "$FRESH"
volume_config remote.broken.agentfsMaxBlobSize 0
set +e
out=$(agentfs lfs prune 2>&1); code=$?
set -e
assert_eq 1 "$code" "a misconfigured remote refuses the prune"
assert_contains "$out" "remote broken is misconfigured" "naming it"
git --git-dir "$STATE/git" config --remove-section remote.broken

step "18. proxies"
lfs_volume prox
proxy_since() { tail -n +"$(($1 + 1))" "$PROXY_LOG"; }
proxy_lines() { wc -l <"$PROXY_LOG"; }
volume_config remote.sep.proxy "$PROXY_URL"
assert_contains "$(agentfs status)" "proxy: $PROXY_URL (remote.sep.proxy)" "status shows the proxy"
head -c 5000 /dev/urandom >"$TREE/p.bin"
agentfs record p.bin
before=$(proxy_lines)
agentfs push --remote sep
assert_eq "$(local_head prox)" "$(remote_ref "$REPO" refs/heads/agentfs/prox)" "pushed through the proxy"
on_lfs_host "$(oid_of p.bin)" || fail "p.bin is not on the LFS host"
through=$(proxy_since "$before")
assert_contains "$through" "localhost:8080" "the git push went through the proxy"
assert_contains "$through" "127.0.0.1:8081" "the LFS upload went through the proxy"
rm -rf "$STATE/lfs/objects"
before=$(proxy_lines)
agentfs lfs fetch --remote sep
assert_contains "$(proxy_since "$before")" "127.0.0.1:8081" "lfs fetch went through the proxy"
# A dead proxy for every remote: the LFS stage cannot reach its server.
git --git-dir "$STATE/git" config --unset remote.sep.proxy
volume_config http.proxy http://127.0.0.1:1
head -c 5000 /dev/urandom >"$TREE/q.bin"
agentfs record q.bin
pushed=$(remote_ref "$REPO" refs/heads/agentfs/prox)
set +e
out=$(agentfs push --remote sep 2>&1); code=$?
set -e
assert_eq 1 "$code" "a dead proxy fails the push"
assert_contains "$out" "sep: failed (unreachable, stage lfs)" "as unreachable at stage lfs"
assert_eq "$pushed" "$(remote_ref "$REPO" refs/heads/agentfs/prox)" "the branch did not move"
volume_config remote.sep.proxy ""
before=$(proxy_lines)
agentfs push --remote sep
assert_eq "$(local_head prox)" "$(remote_ref "$REPO" refs/heads/agentfs/prox)" "an empty remote proxy connects directly"
assert_eq "" "$(proxy_since "$before")" "nothing went through the proxy"
git --git-dir "$STATE/git" config --unset http.proxy
if [ "${E2E_HTTPS:-}" = 1 ]; then
  TLSP_REPO=$(server_repo tlsp)
  volume_config remote.tlsp.url "https://localhost:8443/git/tlsp.git"
  volume_config remote.tlsp.agentfsCredentials "$CREDS/alice"
  volume_config remote.tlsp.lfsurl "$LFS_HTTPS_BASE/acme/tlsp"
  volume_config remote.tlsp.proxy "$PROXY_URL"
  before=$(proxy_lines)
  out=$(agentfs push --remote tlsp 2>&1)
  assert_contains "$out" "tlsp: pushed" "https git and LFS through the proxy"
  assert_eq "$(local_head prox)" "$(remote_ref "$TLSP_REPO" refs/heads/agentfs/prox)" "the https remote has the branch"
  through=$(proxy_since "$before")
  assert_contains "$through" "localhost:8443" "the https git push was tunnelled"
  assert_contains "$through" "localhost:8445" "the https LFS upload was tunnelled"
  BADP=$VOL/badp.git
  git init -q --bare "$BADP"
  volume_config remote.badp.url "$BADP"
  volume_config remote.badp.lfsurl "https://localhost:8444/x"
  volume_config remote.badp.proxy "$PROXY_URL"
  set +e
  out=$(agentfs push --remote badp 2>&1); code=$?
  set -e
  assert_eq 1 "$code" "the untrusted endpoint fails"
  assert_contains "$out" "badp: failed (untrusted_host, stage lfs)" "still untrusted_host through the proxy"
fi

step "all LFS transfer checks passed"
