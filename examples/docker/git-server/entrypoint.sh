#!/bin/sh
# The git server's entrypoint (Dockerfile):
#
#   git-server init    create the bare repository /srv/git/$GIT_REPO.git
#                      (default history) if it is missing, and exit
#   git-server serve   serve /srv/git with basic auth
#   git-server         both (the Docker examples)
#
# Users: $GIT_SERVER_HTPASSWD names an htpasswd file, which nginx reads as
# it changes (the CSI chart mounts its Secret's `htpasswd` key there).
# Without it, the agentfs credentials file /run/secrets/origin
# (`username=` and `password=` lines, the file agentfs pushes with) is
# hashed into one at start (the Docker examples).
#
# Started as root, it gives /srv/git to www-data and runs fcgiwrap and
# nginx's workers as www-data. Started as any other user (the chart: uid
# 33, www-data, with a read-only root filesystem), it runs everything as
# that user and needs /srv/git and /run/git-server writable.
set -eu

repo=/srv/git/${GIT_REPO:-history}.git
run=/run/git-server
root=false
[ "$(id -u)" != 0 ] || root=true

die() {
  echo "git-server: $*" >&2
  exit 1
}

init() {
  [ -d "$repo" ] || git init -q --bare "$repo"
  git --git-dir "$repo" config http.receivepack true
  if $root; then chown -R www-data:www-data /srv/git; fi
  echo "git-server: $repo is ready"
}

users() {
  if [ -n "${GIT_SERVER_HTPASSWD:-}" ]; then
    [ -s "$GIT_SERVER_HTPASSWD" ] || die "$GIT_SERVER_HTPASSWD (GIT_SERVER_HTPASSWD) is missing or empty"
    ln -sfn "$GIT_SERVER_HTPASSWD" "$run/htpasswd"
    echo "git-server: users from $GIT_SERVER_HTPASSWD"
    return
  fi
  credentials=/run/secrets/origin
  user=$(sed -n 's/^username=//p' "$credentials")
  password=$(sed -n 's/^password=//p' "$credentials")
  [ -n "$user" ] && [ -n "$password" ] ||
    die "$credentials needs username= and password= lines (or set GIT_SERVER_HTPASSWD)"
  printf '%s:%s\n' "$user" "$(openssl passwd -apr1 "$password")" >"$run/htpasswd"
  echo "git-server: user $user from $credentials"
}

serve() {
  if $root; then
    install -d -o www-data -g www-data "$run"
  else
    mkdir -p "$run" || die "$run must be writable"
  fi
  users
  if $root; then
    setpriv --reuid=www-data --regid=www-data --init-groups \
      fcgiwrap -s "unix:$run/fcgiwrap.sock" &
    workers='user www-data;'
  else
    fcgiwrap -s "unix:$run/fcgiwrap.sock" &
    workers=''
  fi
  i=0
  until [ -S "$run/fcgiwrap.sock" ]; do
    i=$((i + 1))
    [ $i -le 100 ] || die "fcgiwrap did not start"
    sleep 0.1
  done
  echo "git-server: serving /srv/git as uid $(id -u)"
  exec nginx -e stderr -c /etc/git-server/nginx.conf -g "daemon off; $workers"
}

case "${1:-}" in
init) init ;;
serve) serve ;;
"") init && serve ;;
*) die "usage: git-server [init|serve]" ;;
esac
