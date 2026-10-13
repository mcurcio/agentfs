#!/usr/bin/env bash
# End-to-end tests of pushing to real SSH remotes (design §11.1): Ed25519
# OpenSSH and RSA PEM keys with plain and hashed known_hosts, a wrong key
# that fails only its own remote, unknown and changed host keys refused
# before the key is ever offered, a key rotated under a running
# `agentfs run`, divergence over SSH, a patch-queue inbox fetch, host
# certificates through @cert-authority (valid, unknown CA, expired, wrong
# principal, revoked CA), ssh-agent identities (accepted, refused, dead
# agent), and passphrase-protected keys in OpenSSH, PKCS#8 and legacy PEM
# format (missing, wrong and rotated passphrases).
#
# Needs a binary built with --features ssh and a server from
# ci/git-ssh-server.sh with its GIT_SSH_* variables. Uses sudo to manage
# the server's repositories, authorized_keys and log.

. "$(dirname "$0")/lib.sh"

: "${GIT_SSH_BASE:?start ci/git-ssh-server.sh first}"
: "${GIT_SSH_USER:?}" "${GIT_SSH_REPOS:?}" "${GIT_SSH_AUTHORIZED:?}" "${GIT_SSH_LOG:?}"
: "${GIT_SSH_KNOWN_HOSTS:?}" "${GIT_SSH_KNOWN_HOSTS_HASHED:?}" "${GIT_SSH_DIR:?}"
: "${GIT_SSH_KNOWN_HOSTS_CA:?}" "${GIT_SSH_HOST_CA:?}" "${GIT_SSH_BASE_EXPIRED:?}" "${GIT_SSH_BASE_WRONGNAME:?}"

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
# The inbox ref existed only on the ssh remote, so applying it means it was
# fetched over ssh (the applied inbox ref is then removed locally).
assert_contains "$out" "patch 1 applied" "inbox patch fetched over ssh and applied"

authorize_more() { cat "$@" | sudo tee -a "$GIT_SSH_AUTHORIZED" >/dev/null; }
all_sshd_logs() { sudo cat "$GIT_SSH_DIR"/sshd*.log; }
never_offered() { # public-key-file message
  case "$(all_sshd_logs)" in
  *"$(fingerprint "$1")"*) fail "$2" ;;
  *) pass "$2" ;;
  esac
}
push_fails() { # kind remote...
  local kind=$1; shift
  local args=() r
  for r in "$@"; do args+=(--remote "$r"); done
  set +e
  out=$(agentfs push "${args[@]}" 2>&1); code=$?
  set -e
  echo "$out"
  assert_eq 1 "$code" "push exits 1"
  for r in "$@"; do assert_contains "$out" "$r: failed ($kind)" "$r fails as $kind"; done
}

step "host certificates are trusted through @cert-authority"
for k in certkey certprobe; do ssh-keygen -q -t ed25519 -N '' -C "$k" -f "$KEYS/$k"; done
authorize_more "$KEYS/certkey.pub" "$KEYS/certprobe.pub"
CERT_REPO=$(server_repo cert)
for r in certwrongca certexpired certwrongname certrevoked; do server_repo "$r" >/dev/null; done
volume_config remote.cert.url "$GIT_SSH_BASE/cert.git"
volume_config remote.cert.agentfsSshKey "$KEYS/certkey"
volume_config remote.cert.agentfsKnownHosts "$GIT_SSH_KNOWN_HOSTS_CA"
agentfs push --remote cert
assert_eq "$(local_head demo)" "$(srv_ref "$CERT_REPO" refs/heads/agentfs/demo)" "pushed with only a CA in known_hosts"
ssh-keygen -q -t ed25519 -N '' -f "$KEYS/other_ca"
printf '@cert-authority [localhost]:2222 %s\n' "$(cut -d' ' -f1,2 "$KEYS/other_ca.pub")" >"$KEYS/wrong_ca_hosts"
{ cat "$GIT_SSH_KNOWN_HOSTS_CA"; printf '@revoked * %s\n' "$(cut -d' ' -f1,2 "$GIT_SSH_HOST_CA")"; } >"$KEYS/revoked_ca_hosts"
volume_config remote.certwrongca.url "$GIT_SSH_BASE/certwrongca.git"
volume_config remote.certwrongca.agentfsKnownHosts "$KEYS/wrong_ca_hosts"
volume_config remote.certexpired.url "$GIT_SSH_BASE_EXPIRED/certexpired.git"
volume_config remote.certexpired.agentfsKnownHosts "$GIT_SSH_KNOWN_HOSTS_CA"
volume_config remote.certwrongname.url "$GIT_SSH_BASE_WRONGNAME/certwrongname.git"
volume_config remote.certwrongname.agentfsKnownHosts "$GIT_SSH_KNOWN_HOSTS_CA"
volume_config remote.certrevoked.url "$GIT_SSH_BASE/certrevoked.git"
volume_config remote.certrevoked.agentfsKnownHosts "$KEYS/revoked_ca_hosts"
for r in certwrongca certexpired certwrongname certrevoked; do
  volume_config "remote.$r.agentfsSshKey" "$KEYS/certprobe"
done
push_fails untrusted_host certwrongca certexpired certwrongname certrevoked
assert_contains "$out" "which no @cert-authority line" "an unknown CA is named"
assert_contains "$out" "$(ssh-keygen -lf "$GIT_SSH_HOST_CA" | awk '{print $2}')" "messages carry the CA fingerprint"
assert_contains "$out" "is expired" "an expired certificate is refused"
assert_contains "$out" "is for otherhost, not localhost" "a certificate for another host is refused"
assert_contains "$out" "is marked @revoked" "a revoked CA is refused"
assert_contains "$out" "ssh-keygen -L -f" "messages say how to inspect the certificate"
never_offered "$KEYS/certprobe.pub" "the key was never offered to a host with an untrusted certificate"

step "ssh-agent identities"
for k in agentkey agentwrong; do ssh-keygen -q -t ed25519 -N '' -C "$k" -f "$KEYS/$k"; done
authorize_more "$KEYS/agentkey.pub"
AGENT_REPO=$(server_repo agent)
server_repo agentwrong >/dev/null
start_agent() { # socket keys...
  local socket=$1; shift
  ssh-agent -a "$socket" -s | grep -o 'SSH_AGENT_PID=[0-9]*' | cut -d= -f2
  for k in "$@"; do SSH_AUTH_SOCK=$socket ssh-add -q "$k"; done
}
AGENT_PID=$(start_agent "$KEYS/agent.sock")
SSH_AUTH_SOCK=$KEYS/agent.sock ssh-add -q "$KEYS/agentwrong" "$KEYS/agentkey"
WRONG_AGENT_PID=$(start_agent "$KEYS/wrong-agent.sock")
SSH_AUTH_SOCK=$KEYS/wrong-agent.sock ssh-add -q "$KEYS/agentwrong"
volume_config remote.agent.url "$GIT_SSH_BASE/agent.git"
volume_config remote.agent.agentfsSshAgent "$KEYS/agent.sock"
volume_config remote.agent.agentfsKnownHosts "$GIT_SSH_KNOWN_HOSTS"
volume_config remote.agentwrong.url "$GIT_SSH_BASE/agentwrong.git"
volume_config remote.agentwrong.agentfsSshAgent "$KEYS/wrong-agent.sock"
volume_config remote.agentwrong.agentfsKnownHosts "$GIT_SSH_KNOWN_HOSTS"
# agentfs never reads SSH_AUTH_SOCK: only the configured socket is used.
SSH_AUTH_SOCK=$KEYS/wrong-agent.sock agentfs push --remote agent
assert_eq "$(local_head demo)" "$(srv_ref "$AGENT_REPO" refs/heads/agentfs/demo)" "pushed with the agent's second identity"
assert_contains "$(sshd_log)" "$(fingerprint "$KEYS/agentkey.pub")" "sshd accepted the agent's key"
push_fails auth agentwrong
assert_contains "$out" "rejected every identity of the ssh-agent at $KEYS/wrong-agent.sock" "a refused agent names its socket"
assert_contains "$out" "$(fingerprint "$KEYS/agentwrong.pub")" "and the identities it offered"
kill "$AGENT_PID" "$WRONG_AGENT_PID"
echo agent >"$TREE/agent.txt"
agentfs record agent.txt
push_fails auth agent
assert_contains "$out" "ssh-agent at $KEYS/agent.sock" "a dead agent is an auth failure naming the socket"

step "keys encrypted with a passphrase"
ssh-keygen -q -t ed25519 -N 'two words' -C pp -f "$KEYS/pp"
ssh-keygen -q -t ecdsa -m PKCS8 -N 'two words' -C pp8 -f "$KEYS/pp8"
ssh-keygen -q -t rsa -b 3072 -m PEM -N 'two words' -C pppem -f "$KEYS/pppem"
grep -q 'BEGIN OPENSSH PRIVATE KEY' "$KEYS/pp" || fail "pp is not an OpenSSH key"
grep -q 'BEGIN ENCRYPTED PRIVATE KEY' "$KEYS/pp8" || fail "pp8 is not an encrypted PKCS#8 key"
grep -q 'DEK-Info: AES-128-CBC' "$KEYS/pppem" || fail "pppem is not an AES-128-CBC PEM key"
authorize_more "$KEYS"/{pp,pp8,pppem}.pub
printf 'wrong\n' >"$KEYS/wrong-passphrase"
printf 'two words\n' >"$KEYS/passphrase"
for r in pp pp8 pppem; do
  server_repo "$r" >/dev/null
  volume_config "remote.$r.url" "$GIT_SSH_BASE/$r.git"
  volume_config "remote.$r.agentfsSshKey" "$KEYS/$r"
  volume_config "remote.$r.agentfsKnownHosts" "$GIT_SSH_KNOWN_HOSTS"
done
push_fails auth pp
assert_contains "$out" "set agentfsSshKeyPassphrase" "an encrypted key needs a passphrase"
for r in pp pp8 pppem; do
  volume_config "remote.$r.agentfsSshKeyPassphrase" "$KEYS/wrong-passphrase"
done
push_fails auth pp pp8 pppem
assert_contains "$out" "cannot decrypt $KEYS/pp with $KEYS/wrong-passphrase" "a wrong passphrase is named"
for r in pp pp8 pppem; do
  never_offered "$KEYS/$r.pub" "the $r key was never offered with a wrong passphrase"
  volume_config "remote.$r.agentfsSshKeyPassphrase" "$KEYS/passphrase"
done
agentfs push --remote pp --remote pp8 --remote pppem
for r in pp pp8 pppem; do
  assert_eq "$(local_head demo)" "$(srv_ref "$GIT_SSH_REPOS/$r.git" refs/heads/agentfs/demo)" "pushed with the decrypted $r key"
done

step "a running service picks up a rotated passphrase"
ssh-keygen -q -p -P 'two words' -N 'new words' -f "$KEYS/pp"
echo three >"$TREE/pp.txt"
agentfs record pp.txt
HEAD=$(local_head demo)
LOG=$VOL/run-pp.log
agentfs_start "$LOG" run
RUN=$AGENTFS_PID
wait_for 30 grep -q "push to pp failed (auth).*cannot decrypt" "$LOG" || { cat "$LOG"; fail "run never reported the stale passphrase"; }
pass "run reports the stale passphrase"
printf 'new words\n' >"$KEYS/passphrase.new"
mv "$KEYS/passphrase.new" "$KEYS/passphrase"
pp_has_head() { [ "$(srv_ref "$GIT_SSH_REPOS/pp.git" refs/heads/agentfs/demo)" = "$HEAD" ]; }
wait_for 60 pp_has_head || { cat "$LOG"; fail "rotated passphrase never used"; }
pass "pushed with the rotated passphrase, without a restart"
kill -TERM "$RUN"
wait "$RUN" || { cat "$LOG"; fail "run did not exit cleanly on SIGTERM"; }

step "all ssh scenarios passed"
