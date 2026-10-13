#!/usr/bin/env bash
# End-to-end tests of pushing to real SSH remotes (design §11.1): Ed25519
# OpenSSH and RSA PEM keys with plain and hashed known_hosts, a wrong key
# that fails only its own remote, unknown and changed host keys refused
# before the key is ever offered, a key rotated under a running
# `agentfs run`, divergence over SSH, and a patch-queue inbox fetch.
#
# Needs a binary built with --features ssh and a server from
# ci/git-ssh-server.sh with its GIT_SSH_* variables. Uses sudo to manage
# the server's repositories, authorized_keys and log.

. "$(dirname "$0")/lib.sh"

: "${GIT_SSH_BASE:?start ci/git-ssh-server.sh first}"
: "${GIT_SSH_USER:?}" "${GIT_SSH_REPOS:?}" "${GIT_SSH_AUTHORIZED:?}" "${GIT_SSH_LOG:?}"
: "${GIT_SSH_KNOWN_HOSTS:?}" "${GIT_SSH_KNOWN_HOSTS_HASHED:?}"

# git on the server side, as the server's user (its repositories are its
# own, so no safe.directory exceptions are needed).
srv_git() { (cd / && sudo -H -u "$GIT_SSH_USER" git "$@"); }
server_repo() { # name
  srv_git init -q --bare "$GIT_SSH_REPOS/$1.git"
  echo "$GIT_SSH_REPOS/$1.git"
}
srv_ref() { # bare-repo ref
  srv_git --git-dir "$1" rev-parse --verify --quiet "$2" || true
}
srv_refs() { # bare-repo
  srv_git --git-dir "$1" for-each-ref --format='%(refname)'
}
authorize() { # public-key-files...
  cat "$@" | sudo tee "$GIT_SSH_AUTHORIZED" >/dev/null
}
sshd_log() { sudo cat "$GIT_SSH_LOG"; }
fingerprint() { ssh-keygen -lf "$1" | awk '{print $2}'; }

KEYS=$(mktemp -d)
for k in alice alice2 mallory probe other; do
  ssh-keygen -q -t ed25519 -N '' -C "$k" -f "$KEYS/$k"
done
ssh-keygen -q -t rsa -b 3072 -m PEM -N '' -C rsa -f "$KEYS/rsa"
grep -q 'BEGIN OPENSSH PRIVATE KEY' "$KEYS/alice" || fail "alice is not an OpenSSH key"
grep -q 'BEGIN RSA PRIVATE KEY' "$KEYS/rsa" || fail "rsa is not a PEM key"
authorize "$KEYS"/{alice,rsa,probe,other}.pub

# known_hosts files the server's keys are not in: one with other keys of
# every type for the right name (the host key "changed"), one with the
# right keys for another name (the host is "unknown").
for type in ed25519 ecdsa rsa; do
  ssh-keygen -q -t "$type" -N '' -f "$KEYS/fake_$type"
  printf '[localhost]:2222 %s\n' "$(cut -d' ' -f1,2 "$KEYS/fake_$type.pub")"
done >"$KEYS/changed_hosts"
sed 's/^\[localhost\]/[otherhost]/' "$GIT_SSH_KNOWN_HOSTS" >"$KEYS/unknown_hosts"

new_volume demo
SSH_REPO=$(server_repo ssh)
RSA_REPO=$(server_repo rsa)
server_repo badkey >/dev/null
volume_config remote.ssh.url "$GIT_SSH_BASE/ssh.git"
volume_config remote.ssh.agentfsSshKey "$KEYS/alice"
volume_config remote.ssh.agentfsKnownHosts "$GIT_SSH_KNOWN_HOSTS_HASHED"
volume_config remote.rsa.url "$GIT_SSH_BASE/rsa.git"
volume_config remote.rsa.agentfsSshKey "$KEYS/rsa"
volume_config remote.rsa.agentfsKnownHosts "$GIT_SSH_KNOWN_HOSTS"
volume_config remote.badkey.url "$GIT_SSH_BASE/badkey.git"
volume_config remote.badkey.agentfsSshKey "$KEYS/mallory"
volume_config remote.badkey.agentfsKnownHosts "$GIT_SSH_KNOWN_HOSTS"

step "record"
echo hello >"$TREE/a.txt"
mkdir -p "$TREE/bin" && printf '#!/bin/sh\n' >"$TREE/bin/run" && chmod +x "$TREE/bin/run"
agentfs record a.txt bin/run

step "push over ssh: Ed25519 key with hashed known_hosts, RSA PEM key with plain"
agentfs push --remote ssh --remote rsa
HEAD=$(local_head demo)
assert_eq "$HEAD" "$(srv_ref "$SSH_REPO" refs/heads/agentfs/demo)" "ed25519 remote has the branch"
assert_eq "$HEAD" "$(srv_ref "$RSA_REPO" refs/heads/agentfs/demo)" "rsa remote has the branch"
assert_eq 100755 "$(srv_git --git-dir "$SSH_REPO" ls-tree agentfs/demo bin/run | cut -c1-6)" "executable bit pushed"
assert_contains "$(sshd_log)" "Accepted publickey for $GIT_SSH_USER" "sshd logs accepted keys"
assert_contains "$(sshd_log)" "$(fingerprint "$KEYS/alice.pub")" "sshd log names the key used"

step "a wrong key fails only its own remote"
set +e
out=$(agentfs push 2>&1); code=$?
set -e
assert_eq 1 "$code" "push exits 1 when a remote fails"
assert_contains "$out" "badkey: failed (auth)" "wrong key is an auth failure"
assert_contains "$out" "ssh: up to date" "other remotes still pushed"
assert_contains "$out" "rsa: up to date" "other remotes still pushed"
assert_eq "" "$(srv_refs "$GIT_SSH_REPOS/badkey.git")" "nothing reached the wrong key's remote"
assert_contains "$(agentfs status)" "badkey" "status lists the failing remote"

step "unknown and changed host keys are refused before the key is offered"
UNKNOWN_REPO=$(server_repo unknownhost)
CHANGED_REPO=$(server_repo changedkey)
volume_config remote.unknownhost.url "$GIT_SSH_BASE/unknownhost.git"
volume_config remote.unknownhost.agentfsSshKey "$KEYS/probe"
volume_config remote.unknownhost.agentfsKnownHosts "$KEYS/unknown_hosts"
volume_config remote.changedkey.url "$GIT_SSH_BASE/changedkey.git"
volume_config remote.changedkey.agentfsSshKey "$KEYS/probe"
volume_config remote.changedkey.agentfsKnownHosts "$KEYS/changed_hosts"
set +e
out=$(agentfs push --remote unknownhost --remote changedkey 2>&1); code=$?
set -e
echo "$out"
assert_eq 1 "$code" "push exits 1"
assert_contains "$out" "unknownhost: failed (untrusted_host)" "unknown host is untrusted"
assert_contains "$out" "changedkey: failed (untrusted_host)" "changed host key is untrusted"
assert_contains "$out" "[localhost]:2222 is unknown" "unknown host message"
assert_contains "$out" "does not match known_hosts" "changed key message"
assert_contains "$out" "SHA256:" "messages carry the fingerprint"
assert_contains "$out" "ssh-keyscan -p 2222 localhost" "messages carry the remedy"
assert_eq "" "$(srv_refs "$UNKNOWN_REPO")" "nothing reached the unknown host's repository"
assert_eq "" "$(srv_refs "$CHANGED_REPO")" "nothing reached the changed host's repository"
# The probe key is authorized: had it been offered, sshd would have
# accepted it and logged its fingerprint.
case "$(sshd_log)" in
*"$(fingerprint "$KEYS/probe.pub")"*) fail "the probe key was offered to an untrusted host" ;;
*) pass "the key was never offered to an untrusted host" ;;
esac

step "a running service picks up a rotated key"
volume_config agentfs.pushInterval 1s
echo two >"$TREE/b.txt"
agentfs record b.txt
HEAD=$(local_head demo)
authorize "$KEYS"/{alice2,rsa,probe,other}.pub # the server rotates; the volume still has alice
LOG=$VOL/run.log
agentfs_start "$LOG" run
RUN=$AGENTFS_PID
wait_for 30 grep -q "push to ssh failed (auth)" "$LOG" || { cat "$LOG"; fail "run never reported the stale key"; }
pass "run reports the stale key"
mv "$KEYS/alice2" "$KEYS/alice" # a new inode, as a Secret update makes
ssh_has_head() { [ "$(srv_ref "$SSH_REPO" refs/heads/agentfs/demo)" = "$HEAD" ]; }
wait_for 60 ssh_has_head || { cat "$LOG"; fail "rotated key never used"; }
pass "pushed with the rotated key, without a restart"
kill -TERM "$RUN"
wait "$RUN" || { cat "$LOG"; fail "run did not exit cleanly on SIGTERM"; }
pass "run exits cleanly on SIGTERM"

step "divergence over ssh saves the local head and never forces"
OTHER=$(mktemp -d)
other_git() {
  GIT_SSH_COMMAND="ssh -i $KEYS/other -o IdentitiesOnly=yes -o UserKnownHostsFile=$GIT_SSH_KNOWN_HOSTS -o StrictHostKeyChecking=yes" \
    git "$@"
}
other_git clone -q "$GIT_SSH_BASE/ssh.git" "$OTHER/clone"
git -C "$OTHER/clone" checkout -q -b w origin/agentfs/demo
echo intruder >"$OTHER/clone/intruder"
git -C "$OTHER/clone" add intruder
git -C "$OTHER/clone" -c user.name=other -c user.email=o@example.com commit -q -m "another writer"
other_git -C "$OTHER/clone" push -q origin w:refs/heads/agentfs/demo
THEIRS=$(srv_ref "$SSH_REPO" refs/heads/agentfs/demo)
echo three >"$TREE/c.txt"
agentfs record c.txt
set +e
out=$(agentfs push --remote ssh 2>&1)
set -e
assert_contains "$out" "diverged" "push reports the divergence"
assert_eq "$THEIRS" "$(srv_ref "$SSH_REPO" refs/heads/agentfs/demo)" "the other writer's commit was not overwritten"
saved=$(srv_git --git-dir "$SSH_REPO" for-each-ref --format='%(objectname)' 'refs/agentfs/demo/diverged/')
assert_eq "$(local_head demo)" "$saved" "local head saved under refs/agentfs/demo/diverged/"

step "the patch queue fetches its inbox over ssh"
volume_config agentfs.patchRemote ssh
srv_git --git-dir "$SSH_REPO" update-ref refs/agentfs/demo/inbox/0000000001 "$THEIRS"
set +e
out=$(agentfs patch apply 2>&1)
set -e
echo "$out"
case "$out" in
*"fetching the patch queue from ssh failed"*) fail "the inbox fetch failed" ;;
esac
assert_eq "$THEIRS" "$(remote_ref "$STATE/git" refs/agentfs/demo/inbox/0000000001)" "inbox ref fetched over ssh"

step "all ssh scenarios passed"
