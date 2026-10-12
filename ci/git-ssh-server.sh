#!/usr/bin/env bash
# Starts an OpenSSH server for the SSH end-to-end tests (design §11.1): a
# root sshd of its own on 127.0.0.1:2222, key authentication only, for one
# system user whose shell is git-shell, with repositories under
#   ssh://agentfs-git@localhost:2222/home/agentfs-git/repos/<repo>.git
#
# Usage: ci/git-ssh-server.sh start <dir>
#        ci/git-ssh-server.sh stop <dir>
#
# `start` prints environment assignments for ci/e2e-ssh.sh:
#   GIT_SSH_DIR, GIT_SSH_USER, GIT_SSH_BASE, GIT_SSH_REPOS,
#   GIT_SSH_KNOWN_HOSTS (plain), GIT_SSH_KNOWN_HOSTS_HASHED (ssh-keyscan -H),
#   GIT_SSH_AUTHORIZED (the user's authorized_keys, root-owned: write it
#   with sudo), GIT_SSH_LOG (sshd's log, root-owned: read it with sudo).
#
# Needs passwordless sudo, sshd (openssh-server), ssh-keygen, ssh-keyscan
# and git (for git-shell). It creates the system user agentfs-git and
# /etc/agentfs-ci-ssh, which `stop` removes; the user stays.

set -euo pipefail

cmd=${1:?start|stop}
dir=$(realpath -m "${2:?state directory}")
conf=/etc/agentfs-ci-ssh
user=agentfs-git
home=/home/$user
port=2222

case "$cmd" in
stop)
  if [ -f "$dir/sshd.pid" ]; then
    sudo kill "$(sudo cat "$dir/sshd.pid")" 2>/dev/null || true
  fi
  sudo rm -rf "$conf"
  exit 0
  ;;
start) ;;
*) echo "unknown command $cmd" >&2; exit 2 ;;
esac

mkdir -p "$dir"
shell=$(command -v git-shell)
if ! id "$user" >/dev/null 2>&1; then
  sudo useradd --system --create-home --home-dir "$home" --shell "$shell" "$user"
fi
# A new system user's account is locked ("!"), and sshd refuses locked
# accounts without PAM. "*" means no password, but not locked.
sudo usermod -p '*' "$user"
sudo -u "$user" mkdir -p "$home/repos"

# Host keys of every type, so known_hosts must list them all (libssh2 picks
# the type, design §11.1).
sudo install -d -m 0755 "$conf" "$conf/authorized_keys"
for type in ed25519 ecdsa rsa; do
  sudo ssh-keygen -q -t "$type" -N '' -f "$conf/ssh_host_${type}_key"
done
sudo install -m 0644 /dev/null "$conf/authorized_keys/$user"
sudo mkdir -p /run/sshd

sudo tee "$conf/sshd_config" >/dev/null <<EOF
ListenAddress 127.0.0.1
ListenAddress ::1
Port $port
HostKey $conf/ssh_host_ed25519_key
HostKey $conf/ssh_host_ecdsa_key
HostKey $conf/ssh_host_rsa_key
PidFile $dir/sshd.pid
AuthorizedKeysFile $conf/authorized_keys/%u
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
AllowUsers $user
StrictModes yes
LogLevel VERBOSE
EOF
sudo /usr/sbin/sshd -t -f "$conf/sshd_config"
sudo /usr/sbin/sshd -f "$conf/sshd_config" -E "$dir/sshd.log"

# ssh-keyscan tries only the first address of a name, so pin IPv4, which
# sshd always listens on (::1 too where the host has IPv6).
keyscan() { ssh-keyscan -4 -p "$port" "$@" localhost 2>>"$dir/keyscan.err" || true; }
for _ in $(seq 100); do
  [ -n "$(keyscan)" ] && break
  sleep 0.2
done
keyscan | sort >"$dir/known_hosts"
keyscan -H >"$dir/known_hosts.hashed"
[ "$(grep -c . "$dir/known_hosts")" -ge 3 ] || {
  echo "ssh-keyscan found fewer than 3 host keys:" >&2
  cat "$dir/known_hosts" "$dir/keyscan.err" >&2
  sudo cat "$dir/sshd.log" >&2
  exit 1
}

cat <<EOF
GIT_SSH_DIR=$dir
GIT_SSH_USER=$user
GIT_SSH_BASE=ssh://$user@localhost:$port$home/repos
GIT_SSH_REPOS=$home/repos
GIT_SSH_KNOWN_HOSTS=$dir/known_hosts
GIT_SSH_KNOWN_HOSTS_HASHED=$dir/known_hosts.hashed
GIT_SSH_AUTHORIZED=$conf/authorized_keys/$user
GIT_SSH_LOG=$dir/sshd.log
EOF
