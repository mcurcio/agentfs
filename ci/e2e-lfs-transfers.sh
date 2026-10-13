#!/usr/bin/env bash
# End-to-end tests of LFS transfers (design §7), after ci/e2e-lfs.sh and
# against the same servers: git-lfs's lfs-test-server behind nginx, whose
# LFS host (127.0.0.1:8081) takes fault rules from `lfs_faults` (ci/lib.sh)
# to throttle or fail chosen requests:
#
# 12. lfs.concurrenttransfers: downloads overlap with 4, never with 1;
#     many objects uploaded at once all reach the server, and git-lfs
#     pulls them back.
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

step "all LFS transfer checks passed"
