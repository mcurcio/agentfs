#!/usr/bin/env bash
# End-to-end tests of Git LFS for SSH remotes (design §7): agentfs runs
# git-lfs-authenticate over its own SSH transport against a real OpenSSH
# server, whose git-shell custom command (ci/git-ssh-server.sh with
# GIT_SSH_LFS_AUTH=1) logs every call and names git-lfs's lfs-test-server
# behind nginx (ci/git-http-server.sh with GIT_SERVER_LFS=1,
# ci/lfs-server.sh) with an Authorization header:
#
#  1. status shows the git-lfs-authenticate endpoint;
#  2. a push with no lfsurl runs `git-lfs-authenticate <path> upload` and
#     uploads to the endpoint it names, with the header it gives;
#  3. a real `git clone` over OpenSSH plus `git lfs pull` (git-lfs calling
#     the same command with download) gives back the exact content;
#  4. a restore and `agentfs lfs fetch` after the cache is lost download
#     through it;
#  5. an answer that expires at once is asked for again for each batch of
#     100 objects; a long one is used for both;
#  6. a header the LFS server refuses fails the push as auth, stage lfs;
#  7. an explicit lfsurl never runs the command;
#  8. a server without the command fails as refused, with the lfsurl hint;
#  9. an untrusted host key fails as untrusted_host before any key is
#     offered.
#
# Needs a binary built with --features ssh, git-lfs, openssh-client, and
# the three servers' variables. Uses sudo for the SSH server's files.

. "$(dirname "$0")/lib.sh"

: "${GIT_SSH_BASE:?start ci/git-ssh-server.sh with GIT_SSH_LFS_AUTH=1 first}"
: "${GIT_SSH_USER:?}" "${GIT_SSH_REPOS:?}" "${GIT_SSH_AUTHORIZED:?}" "${GIT_SSH_LOG:?}"
: "${GIT_SSH_KNOWN_HOSTS:?}" "${GIT_SSH_LFS_AUTH_DIR:?}"
: "${GIT_SERVER_LFS_HTPASSWD:?start ci/git-http-server.sh with GIT_SERVER_LFS=1 first}"
: "${LFS_SEPARATE_BASE:?}" "${GIT_SERVER_ACCESS_LOG:?}" "${LFS_SERVER_DIR:?start ci/lfs-server.sh first}"
command -v git-lfs >/dev/null || fail "git-lfs is not installed"
git lfs version

srv_git() { (cd / && sudo -H -u "$GIT_SSH_USER" git "$@"); }
server_repo() { # name
  srv_git init -q --bare "$GIT_SSH_REPOS/$1.git"
  echo "$GIT_SSH_REPOS/$1.git"
}
srv_ref() { # bare-repo ref
  srv_git --git-dir "$1" rev-parse --verify --quiet "$2" || true
}
fingerprint() { ssh-keygen -lf "$1" | awk '{print $2}'; }
set_password() { # htpasswd user password
  local others
  others=$(grep -v "^$2:" "$1" || true)
  { [ -n "$others" ] && echo "$others"; echo "$2:$(openssl passwd -apr1 "$3")"; } >"$1"
}

# The server command's settings and log.
lfs_auth() { printf '%s' "$2" | sudo tee "$GIT_SSH_LFS_AUTH_DIR/$1" >/dev/null; }
auth_log() { sudo cat "$GIT_SSH_LFS_AUTH_DIR/log"; }
auth_lines() { auth_log | wc -l; }
auth_since() { auth_log | tail -n +"$(($1 + 1))"; }
on_lfs_host() { [ -f "$LFS_SERVER_DIR/separate/content/${1:0:2}/${1:2:2}/${1:4}" ]; }
oid_of() { git --git-dir "$STATE/git" show "agentfs/lfsssh:$1" | sed -n 2p | sed 's/^oid sha256://'; }

set_password "$GIT_SERVER_LFS_HTPASSWD" lfsbob lfspw
lfs_auth base "$LFS_SEPARATE_BASE/acme"
lfs_auth token "$(printf lfsbob:lfspw | base64)"
lfs_auth expires_in 3600

KEYS=$(mktemp -d)
for k in alice probe; do
  ssh-keygen -q -t ed25519 -N '' -C "$k" -f "$KEYS/$k"
done
cat "$KEYS/alice.pub" "$KEYS/probe.pub" | sudo tee "$GIT_SSH_AUTHORIZED" >/dev/null
for type in ed25519 ecdsa rsa; do
  ssh-keygen -q -t "$type" -N '' -f "$KEYS/fake_$type"
  printf '[localhost]:2222 %s\n' "$(cut -d' ' -f1,2 "$KEYS/fake_$type.pub")"
done >"$KEYS/changed_hosts"

new_volume lfsssh
volume_config agentfs.lfsThreshold 1k
REPO=$(server_repo lfsssh)
REPO_PATH=${REPO#/}
volume_config remote.origin.url "$GIT_SSH_BASE/lfsssh.git"
volume_config remote.origin.agentfsSshKey "$KEYS/alice"
volume_config remote.origin.agentfsKnownHosts "$GIT_SSH_KNOWN_HOSTS"

step "1. status shows the git-lfs-authenticate endpoint"
assert_contains "$(agentfs status)" \
  "lfs: git-lfs-authenticate over $GIT_SSH_BASE/lfsssh.git (derived from remote.origin.url; credentials from git-lfs-authenticate)" \
  "the endpoint is git-lfs-authenticate's"

step "2. a push asks git-lfs-authenticate for the upload endpoint"
head -c 4096 /dev/urandom >"$TREE/big.bin"
head -c 3000 /dev/urandom >"$TREE/other.bin"
cp "$TREE/big.bin" "$VOL/big.orig"
agentfs record big.bin other.bin
before_access=$(wc -l <"$GIT_SERVER_ACCESS_LOG")
agentfs push
assert_eq "$(local_head lfsssh)" "$(srv_ref "$REPO" refs/heads/agentfs/lfsssh)" "the branch is pushed over ssh"
assert_contains "$(auth_log)" "upload $REPO_PATH" "git-lfs-authenticate ran for the upload, with the path"
for path in big.bin other.bin; do
  on_lfs_host "$(oid_of "$path")" || fail "$path is not on the LFS server"
done
pass "the objects are on the endpoint it named"
users=$(tail -n +"$((before_access + 1))" "$GIT_SERVER_ACCESS_LOG" | awk '$2 == 8081 {print $3}' | sort -u | xargs)
assert_eq "lfsbob" "$users" "the LFS server saw the header git-lfs-authenticate gave"

step "3. git clone over OpenSSH and git lfs pull get the content back"
export GIT_SSH_COMMAND="ssh -i $KEYS/alice -o IdentitiesOnly=yes -o UserKnownHostsFile=$GIT_SSH_KNOWN_HOSTS -o StrictHostKeyChecking=yes"
CLONE=$VOL/clone
GIT_LFS_SKIP_SMUDGE=1 git clone -q -b agentfs/lfsssh "$GIT_SSH_BASE/lfsssh.git" "$CLONE"
assert_eq "version https://git-lfs.github.com/spec/v1" "$(head -n1 "$CLONE/big.bin")" "the clone holds pointers"
git -C "$CLONE" lfs install --local >/dev/null
# git-lfs's pure SSH protocol (git-lfs-transfer) is not served: use
# git-lfs-authenticate, as agentfs does.
git -C "$CLONE" config lfs.sshtransfer never
before=$(auth_lines)
git -C "$CLONE" lfs pull
cmp "$CLONE/big.bin" "$VOL/big.orig" && pass "git lfs pull gives the exact content"
assert_contains "$(auth_since "$before")" " download " "git-lfs asked git-lfs-authenticate for the download"
unset GIT_SSH_COMMAND

step "4. a restore and lfs fetch download through it"
FIRST=$(local_head lfsssh)
rm "$TREE/big.bin"
agentfs record big.bin
rm -rf "$STATE/lfs/objects"
before=$(auth_lines)
agentfs restore big.bin "$FIRST"
cmp "$TREE/big.bin" "$VOL/big.orig" && pass "restored after the cache was lost"
assert_contains "$(auth_since "$before")" "download $REPO_PATH" "the restore asked for the download endpoint"
rm -rf "$STATE/lfs/objects"
before=$(auth_lines)
out=$(agentfs lfs fetch)
assert_contains "$out" "origin: fetched 2 object(s)" "lfs fetch downloaded the head's objects"
assert_contains "$(auth_since "$before")" "download $REPO_PATH" "through git-lfs-authenticate"

step "5. an expired answer is asked for again for each batch"
mkdir -p "$TREE/many"
paths=()
for i in $(seq 101); do
  head -c 2000 /dev/urandom >"$TREE/many/$i.bin"
  paths+=("many/$i.bin")
done
agentfs record "${paths[@]}"
lfs_auth expires_in 0
before=$(auth_lines)
agentfs push
assert_eq 2 "$(auth_since "$before" | grep -c ' upload ')" "two batches, two answers when each expires at once"
for i in $(seq 102 202); do
  head -c 2000 /dev/urandom >"$TREE/many/$i.bin"
done
agentfs record $(seq -f 'many/%g.bin' 102 202)
lfs_auth expires_in 3600
before=$(auth_lines)
agentfs push
assert_eq 1 "$(auth_since "$before" | grep -c ' upload ')" "one answer for both batches when it lasts"

step "6. a header the LFS server refuses fails at stage lfs"
lfs_auth token "$(printf lfsbob:wrong | base64)"
head -c 3000 /dev/urandom >"$TREE/new.bin"
agentfs record new.bin
pushed=$(srv_ref "$REPO" refs/heads/agentfs/lfsssh)
set +e
out=$(agentfs push 2>&1); code=$?
set -e
assert_eq 1 "$code" "the push fails"
assert_contains "$out" "origin: failed (auth, stage lfs)" "as auth at stage lfs"
assert_eq "$pushed" "$(srv_ref "$REPO" refs/heads/agentfs/lfsssh)" "the branch did not move"
lfs_auth token "$(printf lfsbob:lfspw | base64)"
agentfs push

step "7. an explicit lfsurl never runs git-lfs-authenticate"
CREDS=$(mktemp -d)
printf 'username=lfsbob\npassword=lfspw\n' >"$CREDS/lfsbob"
volume_config remote.origin.lfsurl "$LFS_SEPARATE_BASE/acme/lfsssh"
volume_config remote.origin.agentfsLfsCredentials "$CREDS/lfsbob"
head -c 3000 /dev/urandom >"$TREE/explicit.bin"
agentfs record explicit.bin
before=$(auth_lines)
agentfs push
assert_eq "" "$(auth_since "$before")" "no git-lfs-authenticate call"
git --git-dir "$STATE/git" config --unset remote.origin.lfsurl
git --git-dir "$STATE/git" config --unset remote.origin.agentfsLfsCredentials

step "8. a server without git-lfs-authenticate"
CMD=$(dirname "$GIT_SSH_LFS_AUTH_DIR")/git-shell-commands/git-lfs-authenticate
sudo mv "$CMD" "$CMD.off"
head -c 3000 /dev/urandom >"$TREE/norefuse.bin"
agentfs record norefuse.bin
set +e
out=$(agentfs push 2>&1); code=$?
set -e
sudo mv "$CMD.off" "$CMD"
assert_eq 1 "$code" "the push fails"
assert_contains "$out" "origin: failed (refused, stage lfs)" "as refused at stage lfs"
assert_contains "$out" "set remote.origin.lfsurl" "with the lfsurl hint"
agentfs push

step "9. an untrusted host key: no key offered, no command run"
volume_config remote.origin.agentfsSshKey "$KEYS/probe"
volume_config remote.origin.agentfsKnownHosts "$KEYS/changed_hosts"
head -c 3000 /dev/urandom >"$TREE/untrusted.bin"
agentfs record untrusted.bin
before=$(auth_lines)
set +e
out=$(agentfs push 2>&1); code=$?
set -e
assert_eq 1 "$code" "the push fails"
assert_contains "$out" "origin: failed (untrusted_host, stage lfs)" "as untrusted_host at stage lfs"
assert_eq "" "$(auth_since "$before")" "git-lfs-authenticate did not run"
case "$(sudo cat "$GIT_SSH_LOG")" in
*"$(fingerprint "$KEYS/probe.pub")"*) fail "the probe key was offered to an untrusted host" ;;
*) pass "the key was never offered" ;;
esac

step "all git-lfs-authenticate checks passed"
