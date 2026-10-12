#!/usr/bin/env bash
# End-to-end tests of Git LFS (design §7) against git-lfs's reference
# server behind nginx, and the real git-lfs client:
#
#  1. content over agentfs.lfsThreshold, or with filter=lfs, is a pointer
#     in history, with the managed .gitattributes lines; the rest are blobs;
#  2. every LFS upload completes before the git push starts;
#  3. a separate LFS host gets only its own credentials, never the git
#     host's, and the git host never gets the LFS host's;
#  4. `git clone` plus `git lfs pull` gives back the exact content;
#  5. a wrong LFS password fails the push at stage lfs and pushes no ref;
#  6. so does a missing one, and the git credentials are not offered;
#  7. an endpoint derived from the git remote's URL reuses its credentials,
#     and git-lfs's own derived endpoint restores a clone;
#  8. shrinking and deleting LFS files removes their lines;
#  9. a file:// remote, cloned and pulled with git-lfs's standalone adapter;
# 10. with E2E_HTTPS=1 (built with --features https): an https LFS server,
#     and an untrusted certificate failing as untrusted_host;
# 11. restore of an LFS file from the cache, then from a server after the
#     cache is lost.
#
# Needs ci/git-http-server.sh started with GIT_SERVER_LFS=1, ci/lfs-server.sh,
# and git-lfs.

. "$(dirname "$0")/lib.sh"

: "${GIT_SERVER_REPOS:?start ci/git-http-server.sh with GIT_SERVER_LFS=1 first}"
: "${GIT_SERVER_HTPASSWD:?}" "${GIT_SERVER_LFS_HTPASSWD:?}" "${GIT_HTTP_BASE:?}"
: "${LFS_SEPARATE_BASE:?}" "${GIT_SERVER_ACCESS_LOG:?}" "${LFS_SERVER_DIR:?start ci/lfs-server.sh first}"
command -v git-lfs >/dev/null || fail "git-lfs is not installed"
git lfs version
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

# The access log lines written since line $1.
log_since() { tail -n +"$(($1 + 1))" "$LOG"; }
log_lines() { wc -l <"$LOG"; }

set_password "$GIT_SERVER_HTPASSWD" alice pw1
set_password "$GIT_SERVER_LFS_HTPASSWD" lfsbob lfspw
CREDS=$(mktemp -d)
printf 'username=alice\npassword=pw1\n' >"$CREDS/alice"
printf 'username=lfsbob\npassword=lfspw\n' >"$CREDS/lfsbob"

new_volume demo
volume_config agentfs.lfsThreshold 1k
printf '*.psd filter=lfs\nkeep.bin agentfs.store=blob\n' >"$STATE/git/info/attributes"
SEP_REPO=$(server_repo sep)
SAME_REPO=$(server_repo same)
volume_config remote.sep.url "$GIT_HTTP_BASE/sep.git"
volume_config remote.sep.agentfsCredentials "$CREDS/alice"
volume_config remote.sep.lfsurl "$LFS_SEPARATE_BASE/acme/sep"
volume_config remote.sep.agentfsLfsCredentials "$CREDS/lfsbob"
volume_config remote.same.url "$GIT_HTTP_BASE/same.git"
volume_config remote.same.agentfsCredentials "$CREDS/alice"

LFS_PATHS=("art.psd" "big.bin" "dir with space/large file.bin" "tool.bin")
step "1. record: pointers for LFS content, blobs for the rest"
head -c 4096 /dev/urandom >"$TREE/big.bin"
printf 'tiny psd\n' >"$TREE/art.psd"
head -c 3000 /dev/urandom >"$TREE/tool.bin" && chmod +x "$TREE/tool.bin"
mkdir -p "$TREE/dir with space" && head -c 5000 /dev/urandom >"$TREE/dir with space/large file.bin"
echo small >"$TREE/small.txt"
head -c 3000 /dev/urandom >"$TREE/keep.bin"
printf '*.txt text\n' >"$TREE/.gitattributes"
agentfs record big.bin art.psd tool.bin "dir with space/large file.bin" small.txt keep.bin .gitattributes
show() { git --git-dir "$STATE/git" show "agentfs/demo:$1"; }
for path in "${LFS_PATHS[@]}"; do
  pointer=$(show "$path")
  assert_eq "version https://git-lfs.github.com/spec/v1" "$(head -n1 <<<"$pointer")" "$path is a pointer"
  assert_eq "size $(stat -c %s "$TREE/$path")" "$(sed -n 3p <<<"$pointer")" "$path pointer size"
  oid=$(sed -n 2p <<<"$pointer" | sed 's/^oid sha256://')
  assert_eq "$(sha256sum "$TREE/$path" | cut -d' ' -f1)" "$oid" "$path pointer oid"
done
cmp <(show small.txt) "$TREE/small.txt" && pass "small.txt is a blob"
cmp <(show keep.bin) "$TREE/keep.bin" && pass "keep.bin is a blob (agentfs.store=blob)"
expected_attrs=$(printf '%s\n' '*.txt text' \
  '# agentfs-lfs-begin: maintained by agentfs (Git LFS paths); edits are replaced' \
  '/art.psd filter=lfs diff=lfs merge=lfs -text' \
  '/big.bin filter=lfs diff=lfs merge=lfs -text' \
  '/dir[[:space:]]with[[:space:]]space/large[[:space:]]file.bin filter=lfs diff=lfs merge=lfs -text' \
  '/tool.bin filter=lfs diff=lfs merge=lfs -text' \
  '# agentfs-lfs-end')
assert_eq "$expected_attrs" "$(show .gitattributes)" "managed .gitattributes block in history"
assert_eq '*.txt text' "$(cat "$TREE/.gitattributes")" "the backing tree's .gitattributes is untouched"
assert_contains "$(agentfs status)" "lfs: $LFS_SEPARATE_BASE/acme/sep (remote.sep.lfsurl, credentials from agentfsLfsCredentials)" "status shows the LFS endpoint"

step "2. every LFS upload completes before the git push starts"
before=$(log_lines)
agentfs push --remote sep
assert_eq "$(local_head demo)" "$(remote_ref "$SEP_REPO" refs/heads/agentfs/demo)" "sep has the branch"
log_since "$before" >"$VOL/push.log"
# nginx logs $msec when a request ends; it started $request_time (the
# last field) earlier. The push opens with GET info/refs?service=
# git-receive-pack: every PUT must have ended before the first such
# request (or the receive-pack POST) started.
puts=$(awk '$2 == 8081 && $4 == "\"PUT" {print $1}' "$VOL/push.log")
assert_eq 4 "$(wc -l <<<"$puts")" "four objects uploaded to the LFS host"
grep -q '^[0-9.]* 8080 [^ ]* "GET /git/sep.git/info/refs?service=git-receive-pack ' "$VOL/push.log" ||
  fail "no info/refs?service=git-receive-pack in the log: $(cat "$VOL/push.log")"
push_start=$(awk '$2 == 8080 && $5 ~ /git-receive-pack/ {
    start = $1 - $NF
    if (first == "" || start < first) first = start
  } END { if (first != "") printf "%.3f\n", first }' "$VOL/push.log")
[ -n "$push_start" ] || fail "no git-receive-pack in the log: $(cat "$VOL/push.log")"
for put in $puts; do
  awk -v a="$put" -v b="$push_start" 'BEGIN { exit !(a <= b) }' ||
    fail "an LFS PUT ended at $put, after the git push started at $push_start"
done
pass "all PUTs ended before the git push started"
for path in "${LFS_PATHS[@]}"; do
  oid=$(show "$path" | sed -n 2p | sed 's/^oid sha256://')
  # lfs-test-server stores <content>/aa/bb/<the rest of the oid>.
  [ -f "$LFS_SERVER_DIR/separate/content/${oid:0:2}/${oid:2:2}/${oid:4}" ] || fail "$path not on the LFS host"
done
pass "objects are on the separate LFS host"

step "3. each host gets only its own credentials"
assert_eq "lfsbob" "$(awk '$2 == 8081 {print $3}' "$LOG" | sort -u | tr '\n' ' ' | xargs)" "the LFS host saw only lfsbob"
[ -z "$(awk '$2 == 8080 && $3 == "lfsbob"' "$LOG")" ] || fail "the git host saw lfsbob"
pass "the git host never saw lfsbob"

step "4. git clone + git lfs pull restores the content"
CLONE=$VOL/clone
GIT_LFS_SKIP_SMUDGE=1 git clone -q -b agentfs/demo "http://alice:pw1@localhost:8080/git/sep.git" "$CLONE"
assert_eq "version https://git-lfs.github.com/spec/v1" "$(head -n1 "$CLONE/big.bin")" "a clone without smudge holds pointers"
git -C "$CLONE" lfs install --local >/dev/null
git -C "$CLONE" config lfs.url "http://lfsbob:lfspw@127.0.0.1:8081/acme/sep"
git -C "$CLONE" lfs pull
for path in "${LFS_PATHS[@]}" small.txt keep.bin; do
  cmp "$CLONE/$path" "$TREE/$path" || fail "$path differs after git lfs pull"
done
pass "every file matches the tree"
assert_eq "$(printf '%s\n' "${LFS_PATHS[@]}" | sort)" "$(git -C "$CLONE" lfs ls-files -n | sort)" "git lfs ls-files lists the LFS paths"
assert_eq "" "$(git -C "$CLONE" status --porcelain)" "the clone is clean"
[ -x "$CLONE/tool.bin" ] && pass "the executable bit survives"

step "5. a wrong LFS password fails the push at stage lfs, with no ref pushed"
head -c 2048 /dev/urandom >"$TREE/new.bin"
agentfs record new.bin
printf 'username=lfsbob\npassword=wrong\n' >"$CREDS/lfsbob.new" && mv "$CREDS/lfsbob.new" "$CREDS/lfsbob"
pushed=$(remote_ref "$SEP_REPO" refs/heads/agentfs/demo)
set +e
out=$(agentfs push --remote sep 2>&1); code=$?
set -e
assert_eq 1 "$code" "push exits 1"
assert_contains "$out" "sep: failed (auth, stage lfs)" "the failure is an lfs-stage auth failure"
assert_eq "$pushed" "$(remote_ref "$SEP_REPO" refs/heads/agentfs/demo)" "the branch did not move"
printf 'username=lfsbob\npassword=lfspw\n' >"$CREDS/lfsbob.new" && mv "$CREDS/lfsbob.new" "$CREDS/lfsbob"
agentfs push --remote sep
assert_eq "$(local_head demo)" "$(remote_ref "$SEP_REPO" refs/heads/agentfs/demo)" "fixing the file lets the next push through"

step "6. without LFS credentials the git ones are never offered"
head -c 2048 /dev/urandom >"$TREE/new2.bin"
agentfs record new2.bin
git --git-dir "$STATE/git" config --unset remote.sep.agentfsLfsCredentials
before=$(log_lines)
set +e
out=$(agentfs push --remote sep 2>&1); code=$?
set -e
assert_eq 1 "$code" "push exits 1"
assert_contains "$out" "sep: failed (auth, stage lfs)" "anonymous is refused"
assert_eq "-" "$(log_since "$before" | awk '$2 == 8081 {print $3}' | sort -u | xargs)" "the LFS host got no credentials"
[ -z "$(awk '$2 == 8081 && $3 == "alice"' "$LOG")" ] || fail "the LFS host saw alice"
pass "alice never reached the LFS host"
volume_config remote.sep.agentfsLfsCredentials "$CREDS/lfsbob"
agentfs push --remote sep

step "7. the endpoint derived from the git remote reuses its credentials"
before=$(log_lines)
agentfs push --remote same
assert_eq "$(local_head demo)" "$(remote_ref "$SAME_REPO" refs/heads/agentfs/demo)" "same has the branch"
log_since "$before" >"$VOL/same.log"
grep -q '^[0-9.]* 8080 alice "POST /git/same.git/info/lfs/objects/batch' "$VOL/same.log" ||
  fail "no batch request to the derived endpoint: $(cat "$VOL/same.log")"
grep -q '^[0-9.]* 8080 alice "PUT /git/same/objects/' "$VOL/same.log" || fail "no upload through the git host"
pass "uploads went to the derived endpoint as alice"
CLONE2=$VOL/clone-same
printf 'http://alice:pw1@localhost:8080\n' >"$VOL/git-credentials"
GIT_LFS_SKIP_SMUDGE=1 git clone -q -c credential.helper="store --file=$VOL/git-credentials" \
  -b agentfs/demo "http://localhost:8080/git/same.git" "$CLONE2"
git -C "$CLONE2" config credential.helper "store --file=$VOL/git-credentials"
git -C "$CLONE2" lfs install --local >/dev/null
git -C "$CLONE2" lfs pull
cmp "$CLONE2/big.bin" "$TREE/big.bin" && cmp "$CLONE2/new2.bin" "$TREE/new2.bin" &&
  pass "git-lfs's own derived endpoint serves the content"

step "8. shrinking and deleting LFS files removes their lines"
echo tiny >"$TREE/big.bin"
rm "$TREE/art.psd"
agentfs record big.bin art.psd
attrs=$(show .gitattributes)
case "$attrs" in *"/big.bin "* | *"/art.psd "*) fail "stale managed lines: $attrs" ;; esac
assert_contains "$attrs" "/tool.bin filter=lfs" "other lines stay"
assert_eq "tiny" "$(show big.bin)" "big.bin is a blob again"

step "9. a file:// remote with git-lfs's standalone adapter"
FILE_REPO=$VOL/file.git
git init -q --bare "$FILE_REPO"
volume_config remote.file.url "file://$FILE_REPO"
agentfs push --remote file
FILE_CLONE=$VOL/clone-file
GIT_LFS_SKIP_SMUDGE=1 git clone -q -b agentfs/demo "file://$FILE_REPO" "$FILE_CLONE"
git -C "$FILE_CLONE" lfs install --local >/dev/null
git -C "$FILE_CLONE" lfs pull
cmp "$FILE_CLONE/dir with space/large file.bin" "$TREE/dir with space/large file.bin" &&
  pass "pulled from the file remote"

if [ "${E2E_HTTPS:-}" = 1 ]; then
  step "10. an https LFS server; an untrusted certificate is refused"
  TLS_REPO=$VOL/tls.git
  git init -q --bare "$TLS_REPO"
  volume_config remote.tls.url "$TLS_REPO"
  volume_config remote.tls.lfsurl "$LFS_HTTPS_BASE/acme/tls"
  out=$(agentfs push --remote tls 2>&1)
  assert_contains "$out" "tls: pushed" "pushed with LFS over https"
  find "$LFS_SERVER_DIR/tls/content" -type f | grep -q . || fail "nothing on the https LFS server"
  pass "objects are on the https LFS server"
  BAD_REPO=$VOL/badtls.git
  git init -q --bare "$BAD_REPO"
  volume_config remote.badtls.url "$BAD_REPO"
  volume_config remote.badtls.lfsurl "https://localhost:8444/x"
  set +e
  out=$(agentfs push --remote badtls 2>&1); code=$?
  set -e
  assert_eq 1 "$code" "push exits 1"
  assert_contains "$out" "badtls: failed (untrusted_host, stage lfs)" "an untrusted certificate is untrusted_host"
  assert_eq "" "$(remote_ref "$BAD_REPO" refs/heads/agentfs/demo)" "nothing pushed"
  git --git-dir "$STATE/git" config --remove-section remote.badtls
fi

step "11. restore from the cache, then from a server"
TARGET="dir with space/large file.bin"
cp "$TREE/$TARGET" "$VOL/original"
rm "$TREE/$TARGET"
agentfs record "$TARGET"
SOURCE=$(git --git-dir "$STATE/git" rev-parse agentfs/demo~1)
agentfs restore "$TARGET" "$SOURCE"
cmp "$TREE/$TARGET" "$VOL/original" && pass "restored from the cache"
rm "$TREE/$TARGET"
agentfs record "$TARGET"
rm -rf "$STATE/lfs/objects" "$FILE_REPO/lfs"
before=$(log_lines)
agentfs restore "$TARGET" "$SOURCE"
cmp "$TREE/$TARGET" "$VOL/original" && pass "restored after the cache was lost"
log_since "$before" | grep -q '"GET /git/same/objects/' || fail "not downloaded from a server: $(log_since "$before")"
pass "the object was downloaded from the same-host LFS server"

step "all LFS checks passed"
