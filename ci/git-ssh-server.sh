#!/usr/bin/env bash
# Starts OpenSSH servers for the SSH end-to-end tests (design §11.1): a
# root sshd of its own on localhost:2222, key authentication only, for one
# system user whose shell is git-shell, with repositories under
#   ssh://agentfs-git@localhost:2222/home/agentfs-git/repos/<repo>.git
#
# Usage: ci/git-ssh-server.sh start <dir>
#        ci/git-ssh-server.sh stop <dir>
#
# `start` prints environment assignments for ci/e2e-ssh.sh:
#   GIT_SSH_DIR, GIT_SSH_USER, GIT_SSH_BASE, GIT_SSH_REPOS,
#   GIT_SSH_KNOWN_HOSTS (plain), GIT_SSH_KNOWN_HOSTS_HASHED (ssh-keyscan -H),
#   GIT_SSH_HOST_CA (the host CA's public key), GIT_SSH_KNOWN_HOSTS_CA (only
#   an @cert-authority line for the three ports),
#   GIT_SSH_BASE_EXPIRED and GIT_SSH_BASE_WRONGNAME (the same repositories
#   through sshd on ports 2223 and 2224, whose host certificates are
#   expired, and for principal otherhost),
#   GIT_SSH_AUTHORIZED (the user's authorized_keys, root-owned: write it
#   with sudo), GIT_SSH_LOG (sshd's log, root-owned: read it with sudo;
#   the other two log to sshd-2223.log and sshd-2224.log beside it).
#
# Every host key has a certificate signed by one host CA, so a client
# that asks for certificates gets one (OpenSSH's sshd does not check a
# certificate's validity or principals: only the client does).
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
  for pid in "$dir"/sshd*.pid; do
    [ -f "$pid" ] && { sudo kill "$(sudo cat "$pid")" 2>/dev/null || true; }
  done
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

# Host keys of every type, so known_hosts has them all (agentfs asks for the
# types known_hosts lists first, design §11.1).
sudo install -d -m 0755 "$conf" "$conf/authorized_keys"
for type in ed25519 ecdsa rsa; do
  sudo ssh-keygen -q -t "$type" -N '' -f "$conf/ssh_host_${type}_key"
done
sudo install -m 0644 /dev/null "$conf/authorized_keys/$user"
sudo mkdir -p /run/sshd

# A host CA, and certificates of every host key for each server: valid for
# localhost (port 2222), expired (2223), and for another name (2224).
sudo ssh-keygen -q -t ed25519 -N '' -C agentfs-ci-host-ca -f "$conf/host_ca"
sudo cat "$conf/host_ca.pub" >"$dir/host_ca.pub"
sign() { # directory principal validity
  sudo install -d -m 0755 "$conf/$1"
  for type in ed25519 ecdsa rsa; do
    sudo cp "$conf/ssh_host_${type}_key.pub" "$conf/$1/ssh_host_${type}_key.pub"
    sudo ssh-keygen -q -s "$conf/host_ca" -I "$2" -h -n "$2" -V "$3" \
      "$conf/$1/ssh_host_${type}_key.pub"
  done
}
sign valid localhost -5m:+2h
sign expired localhost 20200101:20200102
sign wrongname otherhost -5m:+2h

serve() { # port certificate-directory name
  sudo tee "$conf/sshd_config.$1" >/dev/null <<EOF
ListenAddress 127.0.0.1
ListenAddress ::1
Port $1
HostKey $conf/ssh_host_ed25519_key
HostKey $conf/ssh_host_ecdsa_key
HostKey $conf/ssh_host_rsa_key
HostCertificate $conf/$2/ssh_host_ed25519_key-cert.pub
HostCertificate $conf/$2/ssh_host_ecdsa_key-cert.pub
HostCertificate $conf/$2/ssh_host_rsa_key-cert.pub
PidFile $dir/$3.pid
AuthorizedKeysFile $conf/authorized_keys/%u
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
AllowUsers $user
StrictModes yes
LogLevel VERBOSE
EOF
  sudo /usr/sbin/sshd -t -f "$conf/sshd_config.$1"
  sudo /usr/sbin/sshd -f "$conf/sshd_config.$1" -E "$dir/$3.log"
}
serve "$port" valid sshd
serve 2223 expired sshd-2223
serve 2224 wrongname sshd-2224
printf '@cert-authority [localhost]:2222,[localhost]:2223,[localhost]:2224 %s\n' \
  "$(cut -d' ' -f1,2 "$dir/host_ca.pub")" >"$dir/known_hosts.ca"

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
GIT_SSH_HOST_CA=$dir/host_ca.pub
GIT_SSH_KNOWN_HOSTS_CA=$dir/known_hosts.ca
GIT_SSH_BASE_EXPIRED=ssh://$user@localhost:2223$home/repos
GIT_SSH_BASE_WRONGNAME=ssh://$user@localhost:2224$home/repos
EOF
