#!/bin/sh
# The example git server's entrypoint (Dockerfile): creates the bare
# repository /srv/git/$GIT_REPO.git (history) if missing, takes the user
# and password from the credentials file /run/secrets/origin (the same
# file agentfs pushes with), and runs fcgiwrap and nginx as www-data.
set -eu

repo=/srv/git/${GIT_REPO:-history}.git
[ -d "$repo" ] || git init -q --bare "$repo"
git --git-dir "$repo" config http.receivepack true
chown -R www-data:www-data /srv/git

credentials=/run/secrets/origin
user=$(sed -n 's/^username=//p' "$credentials")
password=$(sed -n 's/^password=//p' "$credentials")
[ -n "$user" ] && [ -n "$password" ] || {
  echo "git-server: $credentials needs username= and password= lines" >&2
  exit 1
}
install -d -o www-data -g www-data /run/git-server
printf '%s:%s\n' "$user" "$(openssl passwd -apr1 "$password")" >/run/git-server/htpasswd

setpriv --reuid=www-data --regid=www-data --init-groups \
  fcgiwrap -s unix:/run/git-server/fcgiwrap.sock &
i=0
until [ -S /run/git-server/fcgiwrap.sock ]; do
  i=$((i + 1))
  [ $i -le 100 ] || { echo "git-server: fcgiwrap did not start" >&2; exit 1; }
  sleep 0.1
done
echo "git-server: serving $repo as user $user"
exec nginx -c /etc/git-server/nginx.conf -g 'daemon off;'
