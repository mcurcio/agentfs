#!/usr/bin/env bash
# End-to-end tests of pushing to real smart-HTTP remotes (design §11):
# credential files, auth failures that do not block other remotes, credential
# rotation picked up by a running `agentfs run`, divergence over HTTP, and,
# with E2E_HTTPS=1 (binary built with --features https), TLS with certificate
# verification.
#
# Needs a server from ci/git-http-server.sh and its GIT_* variables.

. "$(dirname "$0")/lib.sh"

: "${GIT_SERVER_REPOS:?start ci/git-http-server.sh first}"
: "${GIT_SERVER_HTPASSWD:?}" "${GIT_HTTP_BASE:?}"

server_repo() { # name
  git init -q --bare "$GIT_SERVER_REPOS/$1.git"
  echo "$GIT_SERVER_REPOS/$1.git"
}

set_password() { # user password
  local others
  others=$(grep -v "^$1:" "$GIT_SERVER_HTPASSWD" || true)
  { [ -n "$others" ] && echo "$others"; echo "$1:$(openssl passwd -apr1 "$2")"; } \
    >"$GIT_SERVER_HTPASSWD"
}

set_password alice pw1
set_password x-access-token tok1
CREDS=$(mktemp -d)
printf 'username=alice\npassword=pw1\n' >"$CREDS/alice"
printf 'tok1\n' >"$CREDS/token"
printf 'username=alice\npassword=wrong\n' >"$CREDS/wrong"

new_volume demo
HTTP_REPO=$(server_repo http)
TOKEN_REPO=$(server_repo token)
server_repo badauth >/dev/null
volume_config remote.http.url "$GIT_HTTP_BASE/http.git"
volume_config remote.http.agentfsCredentials "$CREDS/alice"
volume_config remote.token.url "$GIT_HTTP_BASE/token.git"
volume_config remote.token.agentfsCredentials "$CREDS/token"
volume_config remote.badauth.url "$GIT_HTTP_BASE/badauth.git"
volume_config remote.badauth.agentfsCredentials "$CREDS/wrong"

step "record"
echo hello >"$TREE/a.txt"
mkdir -p "$TREE/bin" && printf '#!/bin/sh\n' >"$TREE/bin/run" && chmod +x "$TREE/bin/run"
ln -s a.txt "$TREE/link"
agentfs record a.txt bin/run link

step "push over http with a key=value credential file and a bare token"
agentfs push --remote http --remote token
HEAD=$(local_head demo)
assert_eq "$HEAD" "$(remote_ref "$HTTP_REPO" refs/heads/agentfs/demo)" "http remote has the branch"
assert_eq "$HEAD" "$(remote_ref "$TOKEN_REPO" refs/heads/agentfs/demo)" "token remote has the branch"
assert_eq 100755 "$(git --git-dir "$HTTP_REPO" ls-tree agentfs/demo bin/run | cut -c1-6)" "executable bit pushed"

step "a rejected credential fails only its own remote"
set +e
out=$(agentfs push 2>&1); code=$?
set -e
assert_eq 1 "$code" "push exits 1 when a remote fails"
assert_contains "$out" "badauth: failed (auth)" "auth failure is classified"
assert_contains "$out" "http: up to date" "other remotes still pushed"
assert_contains "$(agentfs status)" "badauth" "status lists the failing remote"

step "a running service picks up rotated credentials"
volume_config agentfs.pushInterval 1s
echo two >"$TREE/b.txt"
agentfs record b.txt
HEAD=$(local_head demo)
set_password alice pw2 # the server rotates; the volume still has pw1
LOG=$VOL/run.log
agentfs run >"$LOG" 2>&1 &
RUN=$!
wait_for 30 grep -q "auth" "$LOG" || { cat "$LOG"; fail "run never reported the auth failure"; }
pass "run reports the stale credential"
printf 'username=alice\npassword=pw2\n' >"$CREDS/alice.new" && mv "$CREDS/alice.new" "$CREDS/alice"
http_has_head() { [ "$(remote_ref "$HTTP_REPO" refs/heads/agentfs/demo)" = "$HEAD" ]; }
wait_for 60 http_has_head || { cat "$LOG"; fail "rotated credential never used"; }
pass "pushed with the rotated credential, without a restart"
kill -TERM "$RUN"
wait "$RUN" || { cat "$LOG"; fail "run did not exit cleanly on SIGTERM"; }
pass "run exits cleanly on SIGTERM"

step "divergence over http saves the local head and never forces"
OTHER=$(mktemp -d)
git clone -q "http://alice:pw2@localhost:8080/git/http.git" "$OTHER/clone" 2>/dev/null
git -C "$OTHER/clone" checkout -q -b w origin/agentfs/demo
echo intruder >"$OTHER/clone/intruder"
git -C "$OTHER/clone" add intruder
git -C "$OTHER/clone" -c user.name=other -c user.email=o@example.com commit -q -m "another writer"
git -C "$OTHER/clone" push -q origin w:refs/heads/agentfs/demo
THEIRS=$(remote_ref "$HTTP_REPO" refs/heads/agentfs/demo)
echo three >"$TREE/c.txt"
agentfs record c.txt
set +e
out=$(agentfs push --remote http 2>&1)
set -e
assert_contains "$out" "diverged" "push reports the divergence"
assert_eq "$THEIRS" "$(remote_ref "$HTTP_REPO" refs/heads/agentfs/demo)" "the other writer's commit was not overwritten"
saved=$(git --git-dir "$HTTP_REPO" for-each-ref --format='%(objectname)' 'refs/agentfs/demo/diverged/')
assert_eq "$(local_head demo)" "$saved" "local head saved under refs/agentfs/demo/diverged/"

if [ "${E2E_HTTPS:-0}" = 1 ]; then
  : "${GIT_HTTPS_BASE:?}"
  step "https with certificate verification"
  new_volume tls
  TLS_REPO=$(server_repo tls)
  server_repo wronghost >/dev/null
  volume_config remote.tls.url "$GIT_HTTPS_BASE/tls.git"
  volume_config remote.tls.agentfsCredentials "$CREDS/alice"
  # The certificate is for "localhost" only.
  volume_config remote.wronghost.url "https://127.0.0.1:8443/git/wronghost.git"
  volume_config remote.wronghost.agentfsCredentials "$CREDS/alice"
  echo secure >"$TREE/s.txt"
  agentfs record s.txt
  set +e
  out=$(agentfs push 2>&1); code=$?
  set -e
  assert_contains "$out" "tls: pushed" "pushed over https"
  assert_eq "$(local_head tls)" "$(remote_ref "$TLS_REPO" refs/heads/agentfs/tls)" "https remote has the branch"
  assert_eq 1 "$code" "a certificate for another host fails the push"
  assert_contains "$out" "wronghost: failed (unreachable)" "certificate mismatch is refused"
fi

step "all remote scenarios passed"
